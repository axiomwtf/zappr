//! Native + WASM codegen (SPEC §20, §21).
//! Emits a single-file executable per target; links statically (§20.2).

use crate::callcollector::CallGraph;
use crate::cbackend;
use crate::interp::Vm;
use crate::project::Project;
use std::path::PathBuf;
use std::process::Command;
use std::collections::HashSet;
use std::path::Path;

/// Scratch dir for intermediate artifacts — /tmp can be tiny on some
/// systems, so prefer $XDG_CACHE_HOME/zap/build, then ~/.cache.
fn zap_tmp_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME").map(PathBuf::from).ok()
        .or_else(|| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")).ok());
    let d = base.unwrap_or_else(std::env::temp_dir).join("zap").join("build");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// `zap build` entry point — content-hash artifact cache (§21).
/// `release` selects dist/release (-O2) vs dist/debug (-O0 -g).
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
fn artifact_paths(p: &Project, target: &str, release: bool) -> (PathBuf, PathBuf) {
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
/// sources, Zappr.yaml, cli files, sign key, target, tool version (§21).
fn build_cache_key(p: &Project, target: &str, release: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
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
    // backend/runtime sources shape the artifact too (§21)
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for f in ["src/zaprt.c", "src/lib.rs", "src/wayland.rs", "src/font8x8.inc",
              "src/cbackend.rs", "src/codegen.rs", "src/callcollector.rs"] {
        if let Ok(b) = std::fs::read(manifest.join(f)) { b.hash(&mut h); }
    }
    h.finish()
}

/// Build (or reuse) the statically-linked native GUI host (§16).
/// `libzapgui` is a Rust staticlib compiled by cargo for the requested
/// target; the result is cached by content hash of its sources.
/// Returns the path to `libzapgui.a` (linux) or `libzapgui.a` (windows-gnu).
fn gui_lib(target: &str) -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for f in ["src/lib.rs", "src/wayland.rs", "src/win32.rs", "src/font8x8.inc", "Cargo.toml"] {
        let b = std::fs::read(manifest.join(f))
            .map_err(|e| format!("cannot read {f} for libzapgui: {e}"))?;
        b.hash(&mut h);
    }
    let key = h.finish();
    let triple = match target {
        "linux" | "linux-x86_64" => "x86_64-unknown-linux-gnu",
        "windows" | "windows-x86_64" => "x86_64-pc-windows-gnu",
        _ => return Err(format!("no gui lib for target {target}")),
    };
    let dir = artifact_cache_dir();
    let out = dir.join(format!("libzapgui-{target}-{key:016x}.a"));
    if out.exists() { return Ok(out); }

    // rustup's cargo proxy does not set RUSTC; cargo would find the
    // system rustc which may lack the windows target's std. Prefer the
    // rustup-managed toolchain's cargo+rustc pair when present.
    let home = std::env::var("HOME").unwrap_or_default();
    let rustup_cargo = PathBuf::from(&home)
        .join(".rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin");
    let (cargo, rustc) = if rustup_cargo.join("cargo").exists() {
        (rustup_cargo.join("cargo"), rustup_cargo.join("rustc"))
    } else {
        (PathBuf::from("cargo"), PathBuf::from("rustc"))
    };
    let tdir = dir.join("zapgui-target");
    let o = Command::new(&cargo)
        .args(["build", "--release", "--lib", "--target", triple,
               "--target-dir"])
        .arg(&tdir)
        .current_dir(&manifest)
        .env("RUSTC", &rustc)
        .env("RUSTFLAGS", "-C panic=abort -C debuginfo=1")
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .map_err(|e| format!("cannot run cargo for libzapgui: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "libzapgui build failed:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ));
    }
    let built = tdir.join(triple).join("release").join("libzapgui.a");
    std::fs::copy(&built, &out).map_err(|e| format!("cannot stage libzapgui: {e}"))?;
    Ok(out)
}

fn build_inner(p: &Project, _vm: &mut Vm, target: &str, release: bool, out: &Path) -> Result<(), String> {
    let opt: &[&str] = if release { &["-O2"] } else { &["-O0", "-g"] };
    let is_web = matches!(target, "web" | "wasm" | "wasm32");
    let is_win = matches!(target, "windows" | "windows-x86_64");

    // call-flattening: only fns reachable from main.main are emitted (§12)
    let graph = CallGraph::build(p.modules.iter().map(|(m, _, prog)| (m.as_str(), prog)));
    let reachable = graph.reachable_from_main();
    if reachable.is_empty() {
        return Err("no `main` module with a `main` fn found".into());
    }

    if is_web {
        return build_web(p, &reachable, out, release);
    }
    if is_win {
        return build_win(p, &reachable, out, release);
    }

    let csrc = cbackend::emit(p, &reachable)?;
    let cfile = zap_tmp_dir().join(format!("zap-{}-{}.c", p.config.name.clone().unwrap_or_else(|| "app".into()), std::process::id()));
    std::fs::write(&cfile, &csrc).map_err(|e| format!("cannot write {}: {e}", cfile.display()))?;

    let exe = zap_tmp_dir().join(format!("zap-{}", std::process::id()));
    let gui = gui_lib("linux")?;
    let mut cargs: Vec<&str> = vec!["-std=c11", "-pthread", "-w"];
    cargs.extend_from_slice(opt);
    cargs.push("-o");
    let ccout = Command::new("cc")
        .args(&cargs)
        .arg(&exe)
        .arg(&cfile)
        .arg(&gui)
        .arg("-ldl")
        .env("TMPDIR", zap_tmp_dir())
        .output()
        .map_err(|e| format!("cannot run cc: {e} (install a C11 compiler)"))?;
    let _ = std::fs::remove_file(&cfile);
    if !ccout.status.success() {
        let _ = std::fs::remove_file(&exe);
        return Err(format!(
            "cc failed compiling generated C:\n{}",
            String::from_utf8_lossy(&ccout.stderr)
        ));
    }

    // sign the artifact (§18.2) — trailer keeps the ELF directly runnable
    let bytes = std::fs::read(&exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))?;
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
    let _ = std::fs::remove_file(&exe);
    std::fs::write(&out, &artifact).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
    }
    println!("built {} ({} bytes, signed)", out.display(), artifact.len());
    Ok(())
}

/// WebAssembly target (§18.1): the same C output compiled freestanding to
/// wasm32 with our shim libc, linked by wasm-ld via clang's lld driver.
/// Emits `zap-out.wasm` plus `zap-out.html` (browser loader).
fn build_web(p: &Project, reachable: &HashSet<String>, out: &Path, release: bool) -> Result<(), String> {
    let opt: &[&str] = if release { &["-O2"] } else { &["-O0", "-g"] };
    let csrc = cbackend::emit(p, reachable)?;
    let pid = std::process::id();
    let cfile = zap_tmp_dir().join(format!("zap-{}-{}.c", p.config.name.clone().unwrap_or_else(|| "app".into()), pid));
    let shim = concat!(env!("CARGO_MANIFEST_DIR"), "/src/wasmshim");
    let wshim = concat!(env!("CARGO_MANIFEST_DIR"), "/src/zaprt_wasm.c");
    std::fs::write(&cfile, &csrc).map_err(|e| format!("cannot write {}: {e}", cfile.display()))?;

    let wasm = out;
    // TMPDIR: clang's wasm driver writes temp objects; keep them in /tmp
    // (or ZAP_TMP) and clean up after.
    let cout = Command::new("clang")
        .args([
            "--target=wasm32-unknown-unknown", "-ffreestanding", "-nostdlib",
            "-fno-builtin", "-std=c11", "-w", "-I", shim,
        ])
        .args(opt)
        .args([
            "-Wl,--no-entry", "-Wl,--export=main", "-Wl,--export-memory", "-o",
        ])
        .arg(&wasm)
        .arg(&cfile)
        .arg(wshim)
        .env("TMPDIR", zap_tmp_dir())
        .output()
        .map_err(|e| format!("clang wasm32 failed to launch: {e}"))?;
    if !cout.status.success() {
        let _ = std::fs::remove_file(&cfile);
        return Err(format!(
            "wasm32 codegen failed:\n{}",
            String::from_utf8_lossy(&cout.stderr)
        ));
    }
    let _ = std::fs::remove_file(&cfile);

    // sign the module (§18.2); the html glue strips the trailer before
    // instantiation since wasm binaries reject trailing bytes.
    let bytes = std::fs::read(&wasm).map_err(|e| format!("cannot read {}: {e}", wasm.display()))?;
    let artifact = if p.config.sign {
        if let Some(k) = &p.config.sign_key {
            crate::linker::sign(&bytes, Some(&p.root.join(k)))?
        } else {
            crate::linker::sign(&bytes, None)?
        }
    } else {
        bytes
    };
    std::fs::write(&wasm, &artifact).map_err(|e| format!("cannot write {}: {e}", wasm.display()))?;

    // loader page: fetch the .wasm next to this html, slice off the 100-byte
    // ZAPR trailer if present, instantiate, wire the env imports.
    let html = format!(
        r##"<!doctype html>
<html><head><meta charset="utf-8"><title>{title}</title>
<style>body{{background:#111;color:#ddd;font:14px/1.5 monospace;padding:1em;white-space:pre-wrap}}</style>
</head><body><div id="out"></div><script>
const WASM_NAME = '{wasm_name}';
const out = document.getElementById('out');
const log = (s, err) => {{ const d = document.createElement('div');
  if (err) d.style.color = '#f66'; d.textContent = s; out.appendChild(d); }};
let mem8;
const imports = {{ env: {{
  zp_write: (p, n) => {{ const s = new TextDecoder().decode(mem8.slice(p, p+n)); log(s.replace(/\n$/, '')); }},
  zp_write_err: (p, n) => {{ const s = new TextDecoder().decode(mem8.slice(p, p+n)); log(s.replace(/\n$/, ''), true); }},
  zp_readline: (buf) => {{ /* stdin is a no-op on web */ return 0; }},
  zp_time: () => Date.now() / 1000,
  zp_exit: (code) => {{ log('[exit ' + code + ']'); }},
  zp_getenv: (np, op, cap) => {{
    /* browser has no env; read a name anyway for future host bridges */
    const s = mem8 ? new TextDecoder().decode(mem8.slice(np, np + 256)).split('\0')[0] : '';
    return -1;
  }},
}} }};
(async () => {{
  try {{
    let buf = await (await fetch(WASM_NAME)).arrayBuffer();
    /* strip ZAPR signature trailer if present (4 magic + 32 hash + 64 sig) */
    const b = new Uint8Array(buf);
    const TRAILER = 100;
    const isSigned = buf.byteLength > TRAILER &&
      b[buf.byteLength-TRAILER] === 0x5a && b[buf.byteLength-TRAILER+1] === 0x41 &&
      b[buf.byteLength-TRAILER+2] === 0x50 && b[buf.byteLength-TRAILER+3] === 0x52;
    if (isSigned) buf = buf.slice(0, buf.byteLength - TRAILER);
    const mod = await WebAssembly.compile(buf);
    const inst = await WebAssembly.instantiate(mod, imports);
    mem8 = new Uint8Array(inst.exports.memory.buffer);
    inst.exports.main(0, 0);
  }} catch (e) {{ log('wasm error: ' + e, true); }}
}})();
</script></body></html>"##,
        title = p.config.name.clone().unwrap_or_else(|| "zap app".into()),
        wasm_name = wasm.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "app.wasm".into())
    );
    let html_path = out.with_extension("html");
    std::fs::write(&html_path, html).map_err(|e| format!("cannot write {}: {e}", html_path.display()))?;

    println!("built {} + {} (wasm32, {} bytes, signed={})",
        wasm.display(), html_path.display(), artifact.len(), p.config.sign);
    Ok(())
}

/// Windows PE target: same generated C compiled freestanding for
/// x86_64-windows-gnu, linked by ld.lld into a native .exe. kernel32
/// imports come from an import lib we generate with llvm-dlltool.
fn build_win(p: &Project, reachable: &HashSet<String>, out: &Path, release: bool) -> Result<(), String> {
    let opt: &[&str] = if release { &["-O2"] } else { &["-O0", "-g"] };
    const KERNEL32_DEF: &str = "LIBRARY kernel32.dll\nEXPORTS\n    GetStdHandle\n    WriteFile\n    ReadFile\n    GetEnvironmentVariableA\n    GetSystemTimeAsFileTime\n    ExitProcess\n    GetCommandLineA\n    GetProcessHeap\n    HeapAlloc\n    HeapFree\n    GetModuleHandleW\n";
    const USER32_DEF: &str = "LIBRARY user32.dll\nEXPORTS\n    RegisterClassW\n    CreateWindowExW\n    DefWindowProcW\n    GetMessageW\n    TranslateMessage\n    DispatchMessageW\n    PostQuitMessage\n    ShowWindow\n    UpdateWindow\n    GetDC\n    ReleaseDC\n    SetWindowLongPtrW\n    BeginPaint\n    EndPaint\n    LoadCursorW\n";
    const GDI32_DEF: &str = "LIBRARY gdi32.dll\nEXPORTS\n    CreateDIBSection\n    CreateCompatibleDC\n    SelectObject\n    DeleteObject\n    DeleteDC\n    BitBlt\n    CreateFontW\n    SetBkMode\n    SetTextColor\n    ExtTextOutW\n    GetTextExtentPoint32W\n";
    const MCRT_DEF: &str = "LIBRARY msvcrt.dll\nEXPORTS\n    malloc\n    free\n";
    let csrc = cbackend::emit(p, reachable)?;
    let pid = std::process::id();
    let name = p.config.name.clone().unwrap_or_else(|| "app".into());
    let cfile = zap_tmp_dir().join(format!("zap-{name}-{pid}.c"));
    let wshim = concat!(env!("CARGO_MANIFEST_DIR"), "/src/winshim");
    let rt = concat!(env!("CARGO_MANIFEST_DIR"), "/src/zaprt_win.c");
    std::fs::write(&cfile, &csrc).map_err(|e| format!("cannot write {}: {e}", cfile.display()))?;

    let clang = |args: &[&str], inputs: &[&PathBuf], outp: &PathBuf| -> Result<(), String> {
        let mut c = Command::new("clang");
        c.args([
            "--target=x86_64-w64-windows-gnu", "-O2", "-ffreestanding", "-nostdlib",
            "-fno-builtin", "-std=c11", "-w", "-I", wshim,
        ]);
        c.args(args).args(inputs).arg("-o").arg(outp)
         .env("TMPDIR", zap_tmp_dir());
        let o = c.output().map_err(|e| format!("clang windows failed to launch: {e}"))?;
        if !o.status.success() {
            return Err(format!("windows codegen failed:\n{}", String::from_utf8_lossy(&o.stderr)));
        }
        Ok(())
    };

    let app_o = zap_tmp_dir().join(format!("zap-{name}-{pid}-app.o"));
    let rt_o = zap_tmp_dir().join(format!("zap-{name}-{pid}-rt.o"));
    clang(&["-c"], &[&cfile], &app_o)?;
    let rtp = PathBuf::from(rt);
    clang(&["-c"], &[&rtp], &rt_o)?;
    let _ = std::fs::remove_file(&cfile);

    // kernel32 import library via llvm-dlltool
    let mkimp = |defname: &str, dll: &str, tag: &str| -> Result<PathBuf, String> {
        let def = zap_tmp_dir().join(format!("zap-{name}-{pid}-{tag}.def"));
        let lib = zap_tmp_dir().join(format!("zap-{name}-{pid}-{tag}.a"));
        std::fs::write(&def, defname).map_err(|e| e.to_string())?;
        let o = Command::new("llvm-dlltool")
            .args(["-m", "i386:x86-64", "-d"])
            .arg(&def)
            .args(["-l"])
            .arg(&lib)
            .args(["-D", dll])
            .output()
            .map_err(|e| format!("llvm-dlltool failed to launch: {e}"))?;
        if !o.status.success() {
            return Err(format!("llvm-dlltool failed:\n{}", String::from_utf8_lossy(&o.stderr)));
        }
        let _ = std::fs::remove_file(&def);
        Ok(lib)
    };
    let lib = mkimp(KERNEL32_DEF, "kernel32.dll", "k32")?;
    let mlib = mkimp(MCRT_DEF, "msvcrt.dll", "crt")?;
    let ulib = mkimp(USER32_DEF, "user32.dll", "u32")?;
    let glib = mkimp(GDI32_DEF, "gdi32.dll", "g32")?;

    let gui = gui_lib("windows")?;
    let lout = Command::new("ld.lld")
        .args(["-m", "i386pep", "-e", "mainCRTStartup", "--subsystem=console",
               "--no-insert-timestamp", "--error-limit=0"])
        .arg(&app_o).arg(&rt_o).arg(&lib).arg(&mlib).arg(&ulib).arg(&glib).arg(&gui)
        .arg("-o").arg(out)
        .output()
        .map_err(|e| format!("ld.lld failed to launch: {e}"))?;
    let _ = std::fs::remove_file(&app_o);
    let _ = std::fs::remove_file(&rt_o);
    let _ = std::fs::remove_file(&lib);
    let _ = std::fs::remove_file(&mlib);
    let _ = std::fs::remove_file(&ulib);
    let _ = std::fs::remove_file(&glib);
    if !lout.status.success() {
        return Err(format!("PE link failed:\n{}", String::from_utf8_lossy(&lout.stderr)));
    }

    let bytes = std::fs::read(out).map_err(|e| format!("cannot read {}: {e}", out.display()))?;
    let artifact = if p.config.sign {
        if let Some(k) = &p.config.sign_key {
            crate::linker::sign(&bytes, Some(&p.root.join(k)))?
        } else {
            crate::linker::sign(&bytes, None)?
        }
    } else {
        bytes
    };
    std::fs::write(out, &artifact).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    println!("built {} ({} bytes, PE x86_64, signed={})", out.display(), artifact.len(), p.config.sign);
    Ok(())
}
