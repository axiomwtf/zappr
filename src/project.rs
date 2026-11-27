//! Project loading: Zappr.yaml + zap/**/*.znz → modules (SPEC §17, §12).

use crate::config::{self, Config};
use crate::deps;
use crate::interp::Vm;
use crate::parser::{self, Program};
use crate::value::{TypeRepr, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Banned folder names — vague role-name drawers (SPEC §5.3).
const BANNED_DIRS: &[&str] = &["server", "utils", "misc", "helpers", "common", "lib"];

pub struct Project {
    pub root: PathBuf,
    pub config: Config,
    pub modules: Vec<(String, PathBuf, Program)>,
}

/// Fuse path segments into the camelCase namespace name (§12.1):
/// zap/http/auth/session.znz → httpAuthSession
pub fn fuse_segments(segs: &[String]) -> String {
    let mut out = String::new();
    for (i, s) in segs.iter().enumerate() {
        if i == 0 {
            out.push_str(&s.to_lowercase());
        } else {
            let mut c = s.chars();
            if let Some(f) = c.next() {
                out.push(f.to_ascii_uppercase());
                out.push_str(&c.as_str().to_lowercase());
            }
        }
    }
    out
}

/// Public wrapper for dep loading (deps.rs walks dep zap/ dirs).
pub fn collect_znz_pub(
    dir: &Path,
    base: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    collect_znz(dir, base, out)
}

/// Walk zap/ recursively, returning (module_name, path) pairs.
fn collect_znz(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if p.is_dir() {
            if BANNED_DIRS.contains(&name.as_str()) {
                return Err(format!(
                    "tidiness law (§5.3): banned folder `{name}/` — name parts of your program, not jobs"
                ));
            }
            collect_znz(&p, base, out)?;
        } else if name.ends_with(".znz") {
            // filename must be lowercase (§5.1)
            if name != name.to_lowercase() {
                return Err(format!(
                    "naming law (§5.2): filename `{name}` must be lowercase — `userService.znz` is an error, `userservice.znz` is correct"
                ));
            }
            let rel = p.strip_prefix(base).unwrap_or(&p);
            let segs: Vec<String> = rel
                .with_extension("")
                .iter()
                .map(|s| s.to_string_lossy().to_lowercase())
                .collect();
            out.push((fuse_segments(&segs), p));
        }
    }
    Ok(())
}

/// Load a project from `dir` (found by walking up for Zappr.yaml).
pub fn load(dir: &Path, include_tests: bool) -> Result<Project, String> {
    let root = find_root(dir)?;
    let yaml_path = root.join("Zappr.yaml");
    let yaml_src = std::fs::read_to_string(&yaml_path)
        .map_err(|e| format!("cannot read {}: {e}", yaml_path.display()))?;
    let config = config::parse(&yaml_src)?;

    let src_dir = root.join("zap");
    if !src_dir.is_dir() {
        return Err(format!("no `zap/` source directory in {}", root.display()));
    }
    let mut files = Vec::new();
    collect_znz(&src_dir, &src_dir, &mut files)?;

    // scattered-files law: zap/ root holds exactly main.znz (§17)
    let root_files: Vec<_> = files
        .iter()
        .filter(|(m, p)| {
            p.parent() == Some(src_dir.as_path()) && m.as_str() != "main"
        })
        .collect();
    if !root_files.is_empty() {
        let names: Vec<_> = root_files.iter().map(|(m, _)| m.clone()).collect();
        return Err(format!(
            "tidiness law (§5.3): scattered files at zap/ root: {} — exactly one `main.znz` lives at root; everything else goes in folders",
            names.join(", ")
        ));
    }
    // exactly one main.znz
    let mains: Vec<_> = files.iter().filter(|(m, _)| m == "main").collect();
    if mains.len() > 1 {
        return Err("multiple `main.znz` entry points — exactly one allowed (§17)".into());
    }

    let mut modules = Vec::new();
    for (mname, path) in files {
        if !include_tests && mname.starts_with("tests") {
            continue;
        }
        let src = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let prog = parser::parse(&src).map_err(|e| {
            format!("{}:{}:{}: parse error: {}", path.display(), e.line, e.col, e.msg)
        })?;
        modules.push((mname, path, prog));
    }

    // githubDeps (§13): fetch/verify, then load dep modules into the
    // same visibility domain. Newly resolved pins are written back.
    if !config.github_deps.is_empty() {
        let (fetched, pending) =
            deps::fetch_all(&config.github_deps, &config.resolved)?;
        if !pending.is_empty() {
            let mut merged = config.resolved.clone();
            for (k, v) in &pending {
                merged.insert(k.clone(), v.clone());
            }
            deps::write_resolved(&root, &merged)?;
        }
        for dep in &fetched {
            for (mname, path, prog) in deps::load_dep_modules(dep, include_tests)? {
                if modules.iter().any(|(n, _, _)| n == &mname) {
                    return Err(format!(
                        "module name `{mname}` from dep `{}` collides with an existing module",
                        dep.name
                    ));
                }
                modules.push((mname, path, prog));
            }
        }
    }

    Ok(Project {
        root,
        config,
        modules,
    })
}

pub fn find_root(dir: &Path) -> Result<PathBuf, String> {
    let mut cur = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    loop {
        if cur.join("Zappr.yaml").exists() {
            return Ok(cur);
        }
        if !cur.pop() {
            return Err(format!(
                "no Zappr.yaml found in {} or any parent",
                dir.display()
            ));
        }
    }
}

/// Build a Vm from a loaded project: registers all modules, sets zapYaml.
pub fn vm_for(p: &Project) -> Result<Vm, String> {
    let mut vm = Vm::new();
    for (mname, _, prog) in &p.modules {
        vm.load_module(mname, prog)
            .map_err(|e| format!("load error in module `{mname}`: {}", e.msg))?;
    }
    vm.zap_yaml = yval_to_value_map(&p.config.vals);
    Ok(vm)
}

fn yval_to_value_map(m: &BTreeMap<String, crate::config::YVal>) -> Value {
    let entries: Vec<(Value, Value)> = m
        .iter()
        .map(|(k, v)| (Value::str(k.clone()), yval_to_value(v)))
        .collect();
    Value::Map(std::rc::Rc::new(std::cell::RefCell::new(entries)))
}

fn yval_to_value(v: &crate::config::YVal) -> Value {
    use crate::config::YVal::*;
    match v {
        Str(s) => Value::str(s.clone()),
        Int(i) => Value::Int(*i as i128),
        Float(f) => Value::Float(*f),
        Bool(b) => Value::Bool(*b),
        Null => Value::Nothing,
        Seq(items) => Value::List(std::rc::Rc::new(std::cell::RefCell::new(
            items.iter().map(yval_to_value).collect(),
        ))),
        Map(m) => {
            let entries: Vec<(Value, Value)> = m
                .iter()
                .map(|(k, v)| (Value::str(k.clone()), yval_to_value(v)))
                .collect();
            Value::Map(std::rc::Rc::new(std::cell::RefCell::new(entries)))
        }
    }
}

/// `zap new <name>` — scaffold project + Zappr.yaml (§18).
pub fn scaffold(name: &str, dir: &Path) -> Result<(), String> {
    let root = dir.join(name);
    if root.exists() {
        return Err(format!("{} already exists", root.display()));
    }
    let zap = root.join("zap");
    std::fs::create_dir_all(&zap).map_err(|e| e.to_string())?;
    std::fs::write(
        root.join("Zappr.yaml"),
        format!(
            "name: {name}\nversion: 0.1.0\n\ntargets:\n  - linux\n\n# val:\n#   appName: {name}\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        zap.join("main.znz"),
        format!("main() {{\n  print.nl(\"hello from {name}\")\n}}\n"),
    )
    .map_err(|e| e.to_string())?;
    std::fs::create_dir_all(zap.join("tests")).map_err(|e| e.to_string())?;
    std::fs::write(
        zap.join("tests").join("test.znz"),
        "test() {\n  print.nl(\"no tests yet\")\n}\n",
    )
    .map_err(|e| e.to_string())?;
    println!("created {name}/ — Zappr.yaml + zap/main.znz + zap/tests/test.znz");
    Ok(())
}

/// Type-check only: parse + naming law + semantic checks (§18 `zap check`).
pub fn check_project(p: &Project) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    // build the cross-module symbol table once, then check each module
    let globals = crate::checker::GlobalSyms::from_programs(
        p.modules.iter().map(|(m, _, prog)| (m.as_str(), prog)),
    );
    let mut errors = Vec::new();
    for (mname, path, prog) in &p.modules {
        check_program(mname, path, prog, &mut notes)?;
        let out = crate::checker::check_program(prog, mname, &globals);
        for w in out.warnings {
            notes.push(format!("{}: {w}", path.display()));
        }
        for e in out.errors {
            errors.push(format!("{}: {e}", path.display()));
        }
    }
    if !errors.is_empty() {
        return Err(format!("check failed:\n  {}", errors.join("\n  ")));
    }
    Ok(notes)
}

fn check_program(
    mname: &str,
    path: &Path,
    prog: &Program,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    for item in &prog.items {
        if let parser::Item::Fn {
            name, params, body, ..
        } = item
        {
            // naming law: functions camelCase (§5.2)
            if !is_camel(name) {
                notes.push(format!(
                    "{}: function `{name}` should be camelCase (§5.2)",
                    path.display()
                ));
            }
            for p in params {
                if !is_camel(&p.name) {
                    notes.push(format!(
                        "{}: param `{}` should be camelCase (§5.2)",
                        path.display(),
                        p.name
                    ));
                }
            }
            check_stmts(body, path, notes, 0)?;
        }
    }
    let _ = mname;
    Ok(())
}

fn is_camel(s: &str) -> bool {
    let mut c = s.chars();
    match c.next() {
        Some(f) if f.is_ascii_lowercase() => {}
        _ => return false,
    }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.contains('_')
}

fn is_pascal(s: &str) -> bool {
    s.chars().next().map_or(false, |c| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn check_stmts(
    stmts: &[parser::Stmt],
    path: &Path,
    notes: &mut Vec<String>,
    _depth: usize,
) -> Result<(), String> {
    use parser::Stmt::*;
    for s in stmts {
        match s {
            Decl { name, .. } | Reassign { target: name, .. } => {
                if name.starts_with(|c: char| c.is_ascii_uppercase()) && !is_pascal(name) {
                    notes.push(format!("{}: `{name}` looks PascalCase but isn't (§5.2)", path.display()));
                }
            }
            ReassignField { .. } => {}
            If {
                then_body,
                else_body,
                ..
            } => {
                check_stmts(then_body, path, notes, _depth + 1)?;
                if let Some(e) = else_body {
                    check_stmts(e, path, notes, _depth + 1)?;
                }
            }
            ForIn { body, .. }
            | ForCond { body, .. }
            | ForInfinite { body, .. }
            | ForCStyle { body, .. } => check_stmts(body, path, notes, _depth + 1)?,
            Match { arms, .. } => {
                for a in arms {
                    check_stmts(&a.body, path, notes, _depth + 1)?;
                }
            }
            Parallel { body, .. } => check_stmts(body, path, notes, _depth + 1)?,
            _ => {}
        }
    }
    Ok(())
}
