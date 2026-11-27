//! GitHub dependencies (SPEC §13): clone into a shared cache keyed by
//! commit, verify `resolved:` content on every load, write back on first
//! fetch. A dep is source code, nothing more.

use crate::parser::Program;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One fetched dep: (dep name, checkout dir).
pub struct FetchedDep {
    pub name: String,
    pub dir: PathBuf,
}

/// Parse `github:owner/repo@ref` (ref optional → default branch).
fn parse_spec(spec: &str) -> Result<(String, String, Option<String>), String> {
    let rest = spec
        .strip_prefix("github:")
        .ok_or_else(|| format!("dep spec must start with `github:` — got `{spec}`"))?;
    let (repo, reference) = match rest.rsplit_once('@') {
        Some((r, f)) => (r.to_string(), Some(f.to_string())),
        None => (rest.to_string(), None),
    };
    let mut it = repo.split('/');
    let owner = it.next().unwrap_or("");
    let name = it.next().unwrap_or("");
    if owner.is_empty() || name.is_empty() || it.next().is_some() {
        return Err(format!("bad github dep spec `{spec}` — want github:owner/repo[@ref]"));
    }
    Ok((owner.to_string(), name.to_string(), reference))
}

fn cache_root() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("zap").join("deps");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".cache").join("zap").join("deps")
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn init_remote(dir: &Path, url: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    git(dir, &["init", "-q"])?;
    git(dir, &["remote", "add", "origin", url])?;
    Ok(())
}

/// recorded form: `owner/repo@commit/treehash`
fn recorded_commit(rec: &str) -> &str {
    rec.split('@')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or("")
}

/// Fetch all githubDeps. Returns deps plus newly-written resolved entries
/// (`owner/repo@commit/tree`) the caller must persist into Zappr.yaml.
pub fn fetch_all(
    deps: &BTreeMap<String, String>,
    resolved: &BTreeMap<String, String>,
) -> Result<(Vec<FetchedDep>, Vec<(String, String)>), String> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    for (name, spec) in deps {
        let (owner, repo, reference) = parse_spec(spec)?;
        let url = format!("https://github.com/{owner}/{repo}.git");
        let base = cache_root().join(format!("{owner}__{repo}"));

        let dir = if let Some(rec) = resolved.get(name) {
            let commit = recorded_commit(rec).to_string();
            if commit.is_empty() {
                return Err(format!("dep `{name}`: malformed resolved entry `{rec}`"));
            }
            let cdir = base.join(&commit);
            if cdir.join(".git").exists() {
                let head = git(&cdir, &["rev-parse", "HEAD"])?;
                if head != commit {
                    return Err(format!(
                        "dep `{name}`: cache poisoned — HEAD {head} != resolved {commit}"
                    ));
                }
                cdir
            } else {
                init_remote(&cdir, &url)?;
                git(&cdir, &["fetch", "-q", "--depth", "1", "origin", &commit])?;
                git(&cdir, &["checkout", "-q", "FETCH_HEAD"])?;
                let head = git(&cdir, &["rev-parse", "HEAD"])?;
                if head != commit {
                    return Err(format!(
                        "dep `{name}`: fetched {head} but resolved pins {commit} — refusing (§13 verify, never hope)"
                    ));
                }
                cdir
            }
        } else {
            // first fetch: clone the requested ref (or default branch),
            // then re-key the checkout by commit so cache is content-stable.
            let work = base.join("work");
            if !work.join(".git").exists() {
                init_remote(&work, &url)?;
            }
            let reference = reference.clone().unwrap_or_else(|| "HEAD".into());
            git(&work, &["fetch", "-q", "--depth", "1", "origin", &reference])
                .map_err(|e| format!("dep `{name}`: cannot fetch `{reference}` from {url}: {e}"))?;
            git(&work, &["checkout", "-q", "FETCH_HEAD"])?;
            let commit = git(&work, &["rev-parse", "HEAD"])?;
            let tree = git(&work, &["rev-parse", "HEAD^{tree}"])?;
            let final_dir = base.join(&commit);
            if final_dir.exists() {
                let _ = std::fs::remove_dir_all(&work);
            } else {
                std::fs::create_dir_all(base.parent().unwrap_or(&base)).map_err(|e| e.to_string())?;
                std::fs::rename(&work, &final_dir)
                    .map_err(|e| format!("cannot move dep checkout: {e}"))?;
            }
            pending.push((name.clone(), format!("{owner}/{repo}@{commit}/{tree}")));
            final_dir
        };
        out.push(FetchedDep {
            name: name.clone(),
            dir,
        });
    }
    Ok((out, pending))
}

/// Load a dep's zap/ modules. Module names fuse as usual; the dep key
/// only selects the repo — `use.` sees the dep's own module names.
pub fn load_dep_modules(
    dep: &FetchedDep,
    include_tests: bool,
) -> Result<Vec<(String, PathBuf, Program)>, String> {
    let src = dep.dir.join("zap");
    if !src.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    crate::project::collect_znz_pub(&src, &src, &mut files)?;
    let mut out = Vec::new();
    for (mname, path) in files {
        if !include_tests && mname.starts_with("tests") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("dep `{}`: cannot read {}: {e}", dep.name, path.display()))?;
        let prog = crate::parser::parse(&text).map_err(|e| {
            format!(
                "dep `{}` {}:{}:{}: parse error: {}",
                dep.name, path.display(), e.line, e.col, e.msg
            )
        })?;
        out.push((mname, path, prog));
    }
    Ok(out)
}

/// Rewrite the `resolved:` block in Zappr.yaml with the merged map.
/// Replaces the existing top-level `resolved:` block in place (preserving
/// all other keys and comments), or appends one at EOF.
pub fn write_resolved(root: &Path, map: &BTreeMap<String, String>) -> Result<(), String> {
    let yaml_path = root.join("Zappr.yaml");
    let src = std::fs::read_to_string(&yaml_path).map_err(|e| e.to_string())?;
    let mut block = String::from("resolved:\n");
    for (k, v) in map {
        block.push_str(&format!("  {k}: {v}\n"));
    }
    let lines: Vec<&str> = src.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut replaced = false;
    while i < lines.len() {
        let l = lines[i];
        if !replaced && l.starts_with("resolved:") {
            // skip the old block: indented lines following the key
            i += 1;
            while i < lines.len() && (lines[i].starts_with(' ') || lines[i].trim().is_empty()) {
                i += 1;
            }
            out.push_str(&block);
            replaced = true;
            continue;
        }
        out.push_str(l);
        out.push('\n');
        i += 1;
    }
    if !replaced {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&block);
    }
    std::fs::write(&yaml_path, out).map_err(|e| e.to_string())
}
