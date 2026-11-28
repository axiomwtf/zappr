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
    if let Ok(b) = std::fs::read(manifest.join("src/webshim.js")) { b.hash(&mut h); }
    h.finish()
}

/// rustup's cargo proxy does not set RUSTC; cargo would find the
/// system rustc which may lack a cross target's std. Prefer the
/// rustup-managed toolchain's cargo+rustc pair when present.
fn rust_toolchain() -> (PathBuf, PathBuf) {
    let home = std::env::var("HOME").unwrap_or_default();
    let rustup = PathBuf::from(&home)
        .join(".rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin");
    if rustup.join("cargo").exists() {
        (rustup.join("cargo"), rustup.join("rustc"))
    } else {
        (PathBuf::from("cargo"), PathBuf::from("rustc"))
    }
}

/// Fingerprint of the sources this binary was compiled from, baked in
/// by build.rs so it stays valid when the zap binary is installed or
/// copied away from the source tree. Host blobs under src/hosts/ are
/// excluded from the hash (they are outputs, and hashing them would
/// make regeneration unstable).
fn src_fingerprint() -> u64 {
    env!("ZAP_SRC_FP").parse().unwrap_or(0)
}

/* ---------- prebuilt host blobs (zero-toolchain builds) ---------- */
//
// `zap build` never needs a cross toolchain: the wasm and windows
// hosts ship inside the zap binary as stamped blobs under src/hosts/.
// A blob is `<host bytes> + u64 src_fingerprint + "ZAH1"`; when the
// fingerprint matches the running sources it is used verbatim,
// otherwise codegen falls back to a real cross-compile (which needs
// rustup target stds + llvm-mingw for windows). Regenerate blobs with
// `zap __hostblob <target>` on a machine that has the toolchains.

const BLOB_MAGIC: &[u8; 4] = b"ZAH1";
const HOST_LINUX_BLOB: &[u8] = include_bytes!("hosts/linux-x86_64");
const HOST_WIN_BLOB: &[u8] = include_bytes!("hosts/windows-gnu.exe");
const HOST_WASM_BLOB: &[u8] = include_bytes!("hosts/wasm32-wasip1.wasm");

/// Extract host bytes from a stamped blob when its fingerprint
/// matches the current sources.
fn stamped_blob(blob: &[u8]) -> Option<Vec<u8>> {
    let n = blob.len();
    if n < 12 || &blob[n - 4..] != BLOB_MAGIC {
        return None;
    }
    let h = u64::from_le_bytes(blob[n - 12..n - 4].try_into().ok()?);
    if h != src_fingerprint() {
        return None;
    }
    Some(blob[..n - 12].to_vec())
}

/// The zap host binary bytes for a native (non-web) target.
/// Cross-compiling uses the stamped blob for the foreign target;
/// building for the host OS reuses the running binary itself.
fn host_bytes(target: &str) -> Result<Vec<u8>, String> {
    let current_exe = |what: &str| -> Result<Vec<u8>, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("cannot locate zap binary: {e}"))?;
        std::fs::read(&exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))
            .map_err(|e| format!("{what}: {e}"))
    };
    match target {
        "linux" | "linux-x86_64" => {
            if cfg!(target_os = "linux") {
                current_exe("linux host")
            } else if let Some(b) = stamped_blob(HOST_LINUX_BLOB) {
                Ok(b)
            } else {
                Err("no linux host blob: build zap on linux and run \
                     `zap __hostblob linux`, then commit src/hosts/linux-x86_64"
                    .into())
            }
        }
        "windows" | "windows-x86_64" => {
            if cfg!(windows) {
                current_exe("windows host")
            } else if let Some(b) = stamped_blob(HOST_WIN_BLOB) {
                Ok(b)
            } else {
                let p = host_exe_win()?;
                std::fs::read(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))
            }
        }
        _ => Err(format!("no host binary for target {target}")),
    }
}

/// Cross-compile a host for `target` and write a stamped blob into
/// src/hosts/. Maintainer command: `zap __hostblob windows|web`.
pub fn make_host_blob(target: &str) -> Result<PathBuf, String> {
    let (bytes, name) = match target {
        "linux" | "linux-x86_64" => {
            let exe = std::env::current_exe()
                .map_err(|e| format!("cannot locate zap binary: {e}"))?;
            (std::fs::read(&exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))?,
             "linux-x86_64")
        }
        "windows" | "windows-x86_64" => {
            let p = host_exe_win()?;
            (std::fs::read(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?,
             "windows-gnu.exe")
        }
        "web" | "wasm" | "wasm32" => {
            let p = wasm_host_exe()?;
            (std::fs::read(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?,
             "wasm32-wasip1.wasm")
        }
        _ => return Err(format!("no host blob for target {target}")),
    };
    let mut out = bytes;
    out.extend_from_slice(&src_fingerprint().to_le_bytes());
    out.extend_from_slice(BLOB_MAGIC);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/hosts");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(name);
    std::fs::write(&path, &out).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("wrote {} ({} bytes)", path.display(), out.len());
    Ok(path)
}

/// Cross-compile the zap host for windows-gnu once per source hash.
fn host_exe_win() -> Result<PathBuf, String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let key = src_fingerprint();
    let out = artifact_cache_dir().join(format!("zap-host-win-{key:016x}.exe"));
    if out.exists() { return Ok(out); }

    // windows-gnu linking needs a mingw-w64 gcc driver; rust does not
    // ship self-contained mingw libs on non-windows hosts.
    let mingw = mingw_gcc()?;
    let (cargo, rustc) = rust_toolchain();
    let tdir = artifact_cache_dir().join("zap-host-target");
    let o = Command::new(&cargo)
        .args(["build", "--release", "--bin", "zap", "--target",
               "x86_64-pc-windows-gnu", "--target-dir"])
        .arg(&tdir)
        .current_dir(&manifest)
        .env("RUSTC", &rustc)
        .env("RUSTFLAGS", "-C panic=abort -C debuginfo=1")
        .env("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER", &mingw)
        .env("PATH", format!("{}:{}", mingw.parent().unwrap().display(),
            std::env::var("PATH").unwrap_or_default()))
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
        return build_web(p, out);
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
    let mut bytes = host_bytes(target)?;
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

/// Locate or provision a mingw-w64 gcc driver for windows-gnu linking.
/// Order: PATH → previously downloaded llvm-mingw toolchain in the
/// artifact cache → download an llvm-mingw release (~80MB, one-time).
fn mingw_gcc() -> Result<PathBuf, String> {
    // already on PATH?
    if Command::new("x86_64-w64-mingw32-gcc").arg("--version").output().is_ok() {
        return Ok(PathBuf::from("x86_64-w64-mingw32-gcc"));
    }
    let dir = artifact_cache_dir().join("mingw");
    if let Some(gcc) = find_mingw_gcc(&dir) {
        return Ok(gcc);
    }
    provision_mingw(&dir)?;
    find_mingw_gcc(&dir).ok_or_else(|| "mingw-w64 toolchain provisioning failed".into())
}

/// llvm-mingw layouts: <root>/bin/x86_64-w64-mingw32-gcc, possibly
/// nested one level under a versioned top dir.
fn find_mingw_gcc(dir: &Path) -> Option<PathBuf> {
    for base in [dir.to_path_buf(), dir.join("llvm-mingw")] {
        for cand in [
            base.join("bin/x86_64-w64-mingw32-gcc"),
            base.join("mingw64/bin/x86_64-w64-mingw32-gcc"),
        ] {
            if cand.exists() { return Some(cand); }
        }
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let p = e.path();
                for rel in ["bin/x86_64-w64-mingw32-gcc", "mingw64/bin/x86_64-w64-mingw32-gcc"] {
                    let cand = p.join(rel);
                    if cand.exists() { return Some(cand); }
                }
            }
        }
    }
    None
}

/// Download the latest llvm-mingw (mstorsjo) ucrt tarball: a
/// self-contained clang/lld + mingw-w64 sysroot that runs on linux,
/// and extract it into `dir`. rustc's windows-gnu spec hardcodes
/// -lgcc/-lgcc_eh which llvm-mingw does not ship (it uses compiler-rt
/// + libunwind), so stub archives are synthesized in the sysroot:
/// libgcc.a carries libunwind's objects (provides _Unwind_*),
/// libgcc_eh.a stays empty.
fn provision_mingw(dir: &Path) -> Result<(), String> {
    eprintln!("zap: downloading llvm-mingw toolchain (one-time, ~80MB)…");
    let api = Command::new("curl")
        .args(["-fsSL", "https://api.github.com/repos/mstorsjo/llvm-mingw/releases/latest"])
        .output()
        .map_err(|e| format!("cannot query llvm-mingw releases: {e}"))?;
    if !api.status.success() {
        return Err("cannot query llvm-mingw releases (network?)".into());
    }
    let json = String::from_utf8_lossy(&api.stdout);
    let mut url = None;
    for (m, _) in json.match_indices("\"browser_download_url\"") {
        let rest = &json[m + "\"browser_download_url\"".len()..];
        if let Some(q1) = rest.find('"') {
            let rest = &rest[q1 + 1..];
            if let Some(q2) = rest.find('"') {
                let u = &rest[..q2];
                // linux-hosted x86_64 ucrt build
                if u.starts_with("https://github.com/mstorsjo/llvm-mingw/releases/download/")
                    && u.contains("ucrt")
                    && u.contains("ubuntu-22.04-x86_64.tar.xz")
                {
                    url = Some(u.to_string());
                    break;
                }
            }
        }
    }
    let url = url.ok_or("no suitable llvm-mingw asset found")?;
    eprintln!("zap: fetching {url}");
    let tarball = artifact_cache_dir().join("llvm-mingw.tar.xz");
    let st = Command::new("curl")
        .args(["-fSL", "-o"])
        .arg(&tarball)
        .arg(&url)
        .status()
        .map_err(|e| format!("cannot download llvm-mingw: {e}"))?;
    if !st.success() {
        return Err("llvm-mingw download failed".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let st = Command::new("tar")
        .args(["-xJf"])
        .arg(&tarball)
        .arg("-C")
        .arg(dir)
        .status()
        .map_err(|e| format!("cannot extract llvm-mingw: {e}"))?;
    let _ = std::fs::remove_file(&tarball);
    if !st.success() {
        return Err("llvm-mingw extraction failed".into());
    }
    // flatten a single nested top-level dir if present
    if find_mingw_gcc(dir).is_none() {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() && find_mingw_gcc(&p).is_some() {
                    for sub in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                        let _ = std::fs::rename(sub.path(), dir.join(sub.file_name()));
                    }
                    break;
                }
            }
        }
    }
    let Some(root) = find_mingw_gcc(dir).and_then(|g| g.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf())) else {
        return Err("llvm-mingw layout unexpected".into());
    };
    make_gcc_stubs(&root)?;
    Ok(())
}

/// Synthesize libgcc.a / libgcc_eh.a in the mingw sysroot so rustc's
/// hardcoded -lgcc/-lgcc_eh resolve. libgcc.a is seeded with
/// libunwind's objects for the _Unwind_* personality symbols rust
/// std references on windows-gnu.
fn make_gcc_stubs(root: &Path) -> Result<(), String> {
    let libdir = root.join("x86_64-w64-mingw32/lib");
    let unwind = libdir.join("libunwind.a");
    if !unwind.exists() {
        return Err(format!("{} missing", unwind.display()));
    }
    if libdir.join("libgcc.a").exists() {
        return Ok(());
    }
    let tmp = artifact_cache_dir().join("gccstub-tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let st = Command::new("ar")
        .args(["x"])
        .arg(&unwind)
        .current_dir(&tmp)
        .status()
        .map_err(|e| format!("cannot run ar: {e}"))?;
    if !st.success() {
        return Err("cannot extract libunwind.a".into());
    }
    let objs: Vec<PathBuf> = std::fs::read_dir(&tmp)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "obj").unwrap_or(false))
        .collect();
    if objs.is_empty() {
        return Err("libunwind.a has no objects".into());
    }
    let st = Command::new("ar")
        .arg("rcs")
        .arg(libdir.join("libgcc.a"))
        .args(&objs)
        .status()
        .map_err(|e| format!("cannot run ar: {e}"))?;
    if !st.success() {
        return Err("cannot create libgcc.a".into());
    }
    let st = Command::new("ar")
        .args(["rcs"])
        .arg(libdir.join("libgcc_eh.a"))
        .status()
        .map_err(|e| format!("cannot run ar: {e}"))?;
    if !st.success() {
        return Err("cannot create libgcc_eh.a".into());
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/* ---------------- web target (wasm32-wasip1) ---------------- */

fn u32leb(n: u32) -> Vec<u8> {
    let mut n = n;
    let mut out = Vec::new();
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 { b |= 0x80; }
        out.push(b);
        if n == 0 { break; }
    }
    out
}

/// Cross-compile the zap host for wasm32-wasip1 once per source hash.
/// Only reached when the stamped blob is missing or stale.
fn wasm_host_exe() -> Result<PathBuf, String> {
    let key = src_fingerprint();
    let tdir = artifact_cache_dir().join("zap-wasm-target");
    let out = tdir
        .join("wasm32-wasip1")
        .join("release")
        .join(format!("zap-{key:016x}.wasm"));
    if out.exists() { return Ok(out); }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (cargo, rustc) = rust_toolchain();
    let o = Command::new(&cargo)
        .args(["build", "--release", "--bin", "zap", "--target",
               "wasm32-wasip1", "--target-dir"])
        .arg(&tdir)
        .current_dir(&manifest)
        .env("RUSTC", &rustc)
        .env("RUSTFLAGS", "-C panic=abort")
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .map_err(|e| format!("cannot run cargo for wasm host: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "wasm host build failed:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ));
    }
    let built = tdir
        .join("wasm32-wasip1")
        .join("release")
        .join("zap.wasm");
    std::fs::copy(&built, &out).map_err(|e| format!("cannot stage wasm host: {e}"))?;
    Ok(out)
}

/// Web target (§18.1): the zap host compiled to wasm32-wasip1, with the
/// packed project archive embedded as a `zappr` custom section (legal
/// anywhere in a wasm module, ignored by engines). Emits `<name>.wasm`
/// plus `<name>.html`, a self-contained loader carrying a minimal WASI
/// preview1 shim over an in-memory filesystem.
fn build_web(p: &Project, out: &Path) -> Result<(), String> {
    // prefer the stamped prebuilt host blob; cross-compile only when
    // it is missing or stale relative to the current sources.
    let wasm = match stamped_blob(HOST_WASM_BLOB) {
        Some(b) => b,
        None => {
            let p = wasm_host_exe()?;
            std::fs::read(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?
        }
    };

    // embed the project archive as a `zappr` custom section right after
    // the 8-byte wasm header; valid position for custom sections.
    let archive = pack::pack(p)?;
    let name = b"zappr";
    let mut payload_len = u32leb(name.len() as u32);
    payload_len.extend_from_slice(name);
    payload_len.extend_from_slice(&archive);
    let mut section = vec![0x00u8]; // custom section id
    section.extend_from_slice(&u32leb(payload_len.len() as u32));
    section.extend_from_slice(&payload_len);

    let mut artifact = Vec::with_capacity(wasm.len() + section.len() + 100);
    artifact.extend_from_slice(&wasm[..8]);
    artifact.extend_from_slice(&section);
    artifact.extend_from_slice(&wasm[8..]);

    // sign (§18.2): trailer is stripped by the html loader before compile
    let artifact = if p.config.sign {
        if let Some(k) = &p.config.sign_key {
            crate::linker::sign(&artifact, Some(&p.root.join(k)))?
        } else {
            crate::linker::sign(&artifact, None)?
        }
    } else {
        artifact
    };
    std::fs::write(out, &artifact)
        .map_err(|e| format!("cannot write {}: {e}", out.display()))?;

    let wasm_name = out
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app.wasm".into());
    let title = p.config.name.clone().unwrap_or_else(|| "zap app".into());
    let js = include_str!("webshim.js")
        .replace("__ZAP_WASM_NAME__", &wasm_name)
        .replace("__ZAP_TITLE__", &title);
    let html = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{title}</title>\n\
         <style>body{{background:#111;color:#ddd;font:14px/1.5 monospace;padding:1em;white-space:pre-wrap}}</style>\n\
         </head><body><div id=\"out\"></div><script>\n{js}\n</script></body></html>\n"
    );
    let html_path = out.with_extension("html");
    std::fs::write(&html_path, html)
        .map_err(|e| format!("cannot write {}: {e}", html_path.display()))?;

    println!("built {} + {} (wasm32-wasip1, {} bytes, signed={})",
        out.display(), html_path.display(), artifact.len(), p.config.sign);
    Ok(())
}
