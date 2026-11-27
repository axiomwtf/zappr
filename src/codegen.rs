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

/// `zap build` entry point.
pub fn build(p: &Project, _vm: &mut Vm, target: &str) -> Result<(), String> {
    let out = match target {
        "linux" | "linux-x86_64" => PathBuf::from("zap-out"),
        "windows" | "windows-x86_64" => PathBuf::from("zap-out.exe"),
        "web" | "wasm" | "wasm32" => PathBuf::from("zap-out.wasm"),
        other => return Err(format!("unknown target `{other}` — targets: linux, windows, web")),
    };
    let is_web = matches!(target, "web" | "wasm" | "wasm32");
    let is_win = matches!(target, "windows" | "windows-x86_64");

    // call-flattening: only fns reachable from main.main are emitted (§12)
    let graph = CallGraph::build(p.modules.iter().map(|(m, _, prog)| (m.as_str(), prog)));
    let reachable = graph.reachable_from_main();
    if reachable.is_empty() {
        return Err("no `main` module with a `main` fn found".into());
    }

    if is_web {
        return build_web(p, &reachable, &out);
    }
    if is_win {
        return build_win(p, &reachable, &out);
    }

    let csrc = cbackend::emit(p, &reachable)?;
    let cfile = zap_tmp_dir().join(format!("zap-{}-{}.c", p.config.name.clone().unwrap_or_else(|| "app".into()), std::process::id()));
    std::fs::write(&cfile, &csrc).map_err(|e| format!("cannot write {}: {e}", cfile.display()))?;

    let exe = zap_tmp_dir().join(format!("zap-{}", std::process::id()));
    let ccout = Command::new("cc")
        .args(["-O2", "-std=c11", "-pthread", "-w", "-o"])
        .arg(&exe)
        .arg(&cfile)
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
fn build_web(p: &Project, reachable: &HashSet<String>, out: &PathBuf) -> Result<(), String> {
    let csrc = cbackend::emit(p, reachable)?;
    let pid = std::process::id();
    let cfile = zap_tmp_dir().join(format!("zap-{}-{}.c", p.config.name.clone().unwrap_or_else(|| "app".into()), pid));
    let shim = concat!(env!("CARGO_MANIFEST_DIR"), "/src/wasmshim");
    let wshim = concat!(env!("CARGO_MANIFEST_DIR"), "/src/zaprt_wasm.c");
    std::fs::write(&cfile, &csrc).map_err(|e| format!("cannot write {}: {e}", cfile.display()))?;

    let wasm = out.with_extension("wasm");
    // TMPDIR: clang's wasm driver writes temp objects; keep them in /tmp
    // (or ZAP_TMP) and clean up after.
    let cout = Command::new("clang")
        .args([
            "--target=wasm32-unknown-unknown", "-O2", "-ffreestanding", "-nostdlib",
            "-fno-builtin", "-std=c11", "-w", "-I", shim,
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
    let buf = await (await fetch('zap-out.wasm')).arrayBuffer();
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
        title = p.config.name.clone().unwrap_or_else(|| "zap app".into())
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
fn build_win(p: &Project, reachable: &HashSet<String>, out: &PathBuf) -> Result<(), String> {
    const KERNEL32_DEF: &str = "LIBRARY kernel32.dll\nEXPORTS\n    GetStdHandle\n    WriteFile\n    ReadFile\n    GetEnvironmentVariableA\n    GetSystemTimeAsFileTime\n    ExitProcess\n    GetCommandLineA\n    GetProcessHeap\n    HeapAlloc\n    HeapFree\n";
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
    let def = zap_tmp_dir().join(format!("zap-{name}-{pid}.def"));
    let lib = zap_tmp_dir().join(format!("zap-{name}-{pid}-kernel32.a"));
    std::fs::write(&def, KERNEL32_DEF).map_err(|e| e.to_string())?;
    let o = Command::new("llvm-dlltool")
        .args(["-m", "i386:x86-64", "-d"])
        .arg(&def)
        .args(["-l"])
        .arg(&lib)
        .args(["-D", "kernel32.dll"])
        .output()
        .map_err(|e| format!("llvm-dlltool failed to launch: {e}"))?;
    if !o.status.success() {
        return Err(format!("llvm-dlltool failed:\n{}", String::from_utf8_lossy(&o.stderr)));
    }

    let lout = Command::new("ld.lld")
        .args(["-m", "i386pep", "-e", "mainCRTStartup", "--subsystem=console",
               "--no-insert-timestamp"])
        .arg(&app_o).arg(&rt_o).arg(&lib)
        .arg("-o").arg(out)
        .output()
        .map_err(|e| format!("ld.lld failed to launch: {e}"))?;
    let _ = std::fs::remove_file(&app_o);
    let _ = std::fs::remove_file(&rt_o);
    let _ = std::fs::remove_file(&def);
    let _ = std::fs::remove_file(&lib);
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
