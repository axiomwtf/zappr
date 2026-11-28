//! Artifact build (SPEC §18.1, §20, §21).
//! `zap build` emits a self-contained artifact: the zap host binary (the
//! full Rust toolchain runtime) with the project source tree packed into
//! a footer archive. The host detects the archive at startup and runs the
//! project's `main` in-process — Rust only, no external compiler.

use crate::callcollector::CallGraph;
use crate::interp::Vm;
use crate::pack;
use crate::project::Project;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

/// `zap build` entry point — content-hash artifact cache (§21).
/// `release` selects dist/release vs dist/debug.
pub fn build(p: &Project, vm: &mut Vm, target: &str, release: bool) -> Result<(), String> {
    let key = build_cache_key(p, target, release);
    let cdir = artifact_cache_dir();
    let cfile = cdir.join(format!("{key:016x}"));
    let meta = cdir.join(format!("{key:016x}.json"));

    // figure out expected outputs for this target (§18.1)
    let is_web = matches!(target, "web" | "wasm" | "wasm32");
    let (out, html) = artifact_paths(p, target, release);

    // cache hit: artifact + optional html sidecar present and fresh
    if cfile.exists() && std::fs::metadata(&cfile).map(|m| m.len() > 0).unwrap_or(false)
        && (!is_web || html.exists() || std::fs::metadata(cdir.join(format!("{key:016x}.html"))).is_ok())
    {
        std::fs::copy(&cfile, &out).map_err(|e| format!("cache restore failed: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
        }
        if is_web {
            let hp = cdir.join(format!("{key:016x}.html"));
            if hp.exists() {
                std::fs::copy(&hp, &html).map_err(|e| format!("cache restore failed: {e}"))?;
            }
        }
        println!("built {} (cached)", out.display());
        return Ok(());
    }

    build_inner(p, vm, target, release, &out)?;

    // populate cache
    if let Ok(bytes) = std::fs::read(&out) {
        let _ = std::fs::write(&cfile, &bytes);
        let _ = std::fs::write(&meta, format!("target={target}"));
        if is_web {
            if let Ok(h) = std::fs::read(&html) {
                let _ = std::fs::write(cdir.join(format!("{key:016x}.html")), &h);
            }
        }
    }
    Ok(())
}

/// dist/{debug|release}/<name>[.exe|.wasm] under the project root (§18.1)
pub(crate) fn artifact_paths(p: &Project, target: &str, release: bool) -> (PathBuf, PathBuf) {
    let name = p.config.name.clone().unwrap_or_else(|| "app".into());
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let dir = p.root.join("dist").join(if release { "release" } else { "debug" });
    let _ = std::fs::create_dir_all(&dir);
    let stem = dir.join(&safe);
    match target {
        "linux" | "linux-x86_64" => (stem.clone(), stem.with_extension("html")),
        "windows" | "windows-x86_64" => (stem.with_extension("exe"), stem.with_extension("html")),
        _ => (stem.with_extension("wasm"), stem.with_extension("html")),
    }
}

fn artifact_cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME").map(PathBuf::from).ok()
        .or_else(|| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")).ok());
    let d = base.unwrap_or_else(std::env::temp_dir).join("zap").join("artifacts");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Content key over everything that can change the artifact: all module
/// sources, Zappr.yaml, cli files, sign key, target, tool version, and
/// every zap source file (the host binary is part of the artifact) (§21).
fn build_cache_key(p: &Project, target: &str, release: bool) -> u64 {
    let mut h = DefaultHasher::new();
    target.hash(&mut h);
    release.hash(&mut h);
    p.config.sign.hash(&mut h);
    env!("CARGO_PKG_VERSION").hash(&mut h);
    for (m, path, _) in &p.modules {
        m.hash(&mut h);
        path.hash(&mut h);
        if let Ok(b) = std::fs::read(path) { b.hash(&mut h); }
    }
    if let Ok(b) = std::fs::read(p.root.join("Zappr.yaml")) { b.hash(&mut h); }
    for rel in &p.config.cli {
        if let Ok(b) = std::fs::read(p.root.join(rel)) { b.hash(&mut h); }
    }
    if let Some(k) = &p.config.sign_key {
        if let Ok(b) = std::fs::read(p.root.join(k)) { b.hash(&mut h); }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Ok(rd) = std::fs::read_dir(manifest.join("src")) {
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|f| f.extension().map(|e| e == "rs").unwrap_or(false))
            .collect();
        files.sort();
        for f in files {
            if let Ok(b) = std::fs::read(&f) { b.hash(&mut h); }
        }
    }
    if let Ok(b) = std::fs::read(manifest.join("Cargo.toml")) { b.hash(&mut h); }
    h.finish()
}

/// The zap host binary that becomes the artifact.
/// Linux reuses the running zap binary; windows cross-compiles a
/// windows-gnu host into the artifact cache (keyed by source hash).
fn host_exe(target: &str) -> Result<PathBuf, String> {
    match target {
        "linux" | "linux-x86_64" => std::env::current_exe()
            .map_err(|e| format!("cannot locate zap binary: {e}")),
        "windows" | "windows-x86_64" => host_exe_win(),
        _ => Err(format!("no host binary for target {target}")),
    }
}

/// Cross-compile the zap host for windows-gnu once per source hash.
fn host_exe_win() -> Result<PathBuf, String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut h = DefaultHasher::new();
    if let Ok(rd) = std::fs::read_dir(manifest.join("src")) {
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|f| f.extension().map(|e| e == "rs").unwrap_or(false))
            .collect();
        files.sort();
        for f in files {
            if let Ok(b) = std::fs::read(&f) { b.hash(&mut h); }
        }
    }
    if let Ok(b) = std::fs::read(manifest.join("Cargo.toml")) { b.hash(&mut h); }
    let key = h.finish();
    let out = artifact_cache_dir().join(format!("zap-host-win-{key:016x}.exe"));
    if out.exists() { return Ok(out); }

    // rustup's cargo proxy does not set RUSTC; cargo would find the
    // system rustc which may lack the windows target's std. Prefer the
    // rustup-managed toolchain's cargo+rustc pair when present.
    let home = std::env::var("HOME").unwrap_or_default();
    let rustup = PathBuf::from(&home)
        .join(".rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin");
    let (cargo, rustc) = if rustup.join("cargo").exists() {
        (rustup.join("cargo"), rustup.join("rustc"))
    } else {
        (PathBuf::from("cargo"), PathBuf::from("rustc"))
    };
    // windows-gnu linking needs a mingw-w64 gcc driver; rust does not
    // ship self-contained mingw libs on non-windows hosts.
    if Command::new("x86_64-w64-mingw32-gcc").arg("--version").output().is_err() {
        return Err(
            "windows target requires a mingw-w64 toolchain \
             (x86_64-w64-mingw32-gcc) to build the host binary"
                .into(),
        );
    }
    let tdir = artifact_cache_dir().join("zap-host-target");
    let o = Command::new(&cargo)
        .args(["build", "--release", "--bin", "zap", "--target",
               "x86_64-pc-windows-gnu", "--target-dir"])
        .arg(&tdir)
        .current_dir(&manifest)
        .env("RUSTC", &rustc)
        .env("RUSTFLAGS", "-C panic=abort -C debuginfo=1")
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .map_err(|e| format!("cannot run cargo for windows host: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "windows host build failed:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ));
    }
    let built = tdir
        .join("x86_64-pc-windows-gnu")
        .join("release")
        .join("zap.exe");
    std::fs::copy(&built, &out).map_err(|e| format!("cannot stage windows host: {e}"))?;
    Ok(out)
}

fn build_inner(p: &Project, _vm: &mut Vm, target: &str, _release: bool, out: &Path) -> Result<(), String> {
    let is_web = matches!(target, "web" | "wasm" | "wasm32");
    if is_web {
        return Err("the web target is not supported by the Rust host yet".into());
    }
    let supported = matches!(target, "linux" | "linux-x86_64" | "windows" | "windows-x86_64");
    if !supported {
        return Err(format!("unknown target '{target}' (expected linux or windows)"));
    }

    // a main fn must exist for the artifact to do anything (§12)
    let graph = CallGraph::build(p.modules.iter().map(|(m, _, prog)| (m.as_str(), prog)));
    let reachable: HashSet<String> = graph.reachable_from_main();
    if reachable.is_empty() {
        return Err("no `main` module with a `main` fn found".into());
    }

    // artifact = host binary + appended project archive
    let host = host_exe(target)?;
    let mut bytes = std::fs::read(&host)
        .map_err(|e| format!("cannot read host {}: {e}", host.display()))?;
    let section = pack::pack_section(p)?;
    bytes.extend_from_slice(&section);

    // sign the artifact (§18.2) — trailer keeps the binary directly runnable
    let artifact = if p.config.sign {
        if let Some(k) = &p.config.sign_key {
            // signKey is project-relative (§18.2)
            crate::linker::sign(&bytes, Some(&p.root.join(k)))?
        } else {
            crate::linker::sign(&bytes, None)?
        }
    } else {
        bytes
    };
    std::fs::write(&out, &artifact).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
    }
    let kind = if target.starts_with("windows") { "PE x86_64" } else { "ELF x86_64" };
    println!("built {} ({} bytes, {}, signed={})",
        out.display(), artifact.len(), kind, p.config.sign);
    Ok(())
}
