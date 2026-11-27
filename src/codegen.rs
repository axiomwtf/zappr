//! Native + WASM codegen (SPEC §20, §21).
//! Emits a single-file executable per target; links statically (§20.2).

use crate::callcollector::CallGraph;
use crate::cbackend;
use crate::interp::Vm;
use crate::project::Project;
use std::path::PathBuf;
use std::process::Command;

/// `zap build` entry point.
pub fn build(p: &Project, _vm: &mut Vm, target: &str) -> Result<(), String> {
    let out = match target {
        "linux" | "linux-x86_64" => PathBuf::from("zap-out"),
        "windows" | "windows-x86_64" => PathBuf::from("zap-out.exe"),
        "web" | "wasm" | "wasm32" => PathBuf::from("zap-out.wasm"),
        other => return Err(format!("unknown target `{other}` — targets: linux, windows, web")),
    };
    if target == "web" || target == "wasm" || target == "wasm32" {
        return Err("web target: wasm backend lands with the DOM bridge (§18.1)".into());
    }
    if target == "windows" || target == "windows-x86_64" {
        return Err("windows target needs a mingw cross toolchain — not wired yet".into());
    }

    // call-flattening: only fns reachable from main.main are emitted (§12)
    let graph = CallGraph::build(p.modules.iter().map(|(m, _, prog)| (m.as_str(), prog)));
    let reachable = graph.reachable_from_main();
    if reachable.is_empty() {
        return Err("no `main` module with a `main` fn found".into());
    }

    let csrc = cbackend::emit(p, &reachable)?;
    let cfile = std::env::temp_dir().join(format!("zap-{}-{}.c", p.config.name.clone().unwrap_or_else(|| "app".into()), std::process::id()));
    std::fs::write(&cfile, &csrc).map_err(|e| format!("cannot write {}: {e}", cfile.display()))?;

    let exe = std::env::temp_dir().join(format!("zap-{}", std::process::id()));
    let status = Command::new("cc")
        .args(["-O2", "-std=c11", "-pthread", "-w", "-o"])
        .arg(&exe)
        .arg(&cfile)
        .status()
        .map_err(|e| format!("cannot run cc: {e} (install a C11 compiler)"))?;
    let _ = std::fs::remove_file(&cfile);
    if !status.success() {
        let _ = std::fs::remove_file(&exe);
        return Err(format!("cc failed compiling {}", cfile.display()));
    }

    // sign the artifact (§18.2) — trailer keeps the ELF directly runnable
    let bytes = std::fs::read(&exe).map_err(|e| format!("cannot read {}: {e}", exe.display()))?;
    let artifact = crate::linker::sign(&bytes)?;
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
