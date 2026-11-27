mod callcollector;
mod cbackend;
mod checker;
mod codegen;
mod config;
mod fmt;
mod interp;
mod lexer;
mod linker;
mod parser;
mod project;
mod uiruntime;
mod value;

use std::path::Path;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some("--version") | Some("-V") => {
            println!("zap {VERSION}");
            ExitCode::SUCCESS
        }
        Some(cmd) => match dispatch(cmd, &args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(msg) => {
                eprintln!("zap: {msg}");
                ExitCode::FAILURE
            }
        },
    }
}

fn dispatch(cmd: &str, rest: &[String]) -> Result<(), String> {
    match cmd {
        "new" => cmd_new(rest),
        "run" => cmd_run(rest),
        "build" => cmd_build(rest),
        "check" => cmd_check(rest),
        "test" => cmd_test(rest),
        "fmt" => cmd_fmt(rest),
        "verify" => cmd_verify(rest),
        "copyright" => cmd_copyright(rest),
        other => Err(format!("unknown command `{other}` — see `zap --help`")),
    }
}

fn cmd_new(rest: &[String]) -> Result<(), String> {
    let name = rest
        .first()
        .ok_or("usage: zap new <name>")?;
    let dir = rest.get(1).map(String::as_str).unwrap_or(".");
    project::scaffold(name, Path::new(dir))
}

fn cmd_run(_rest: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = project::load(&cwd, false)?;
    let mut vm = project::vm_for(&p)?;
    match vm.run_main() {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("runtime error: {}", e.msg)),
    }
}

fn cmd_build(rest: &[String]) -> Result<(), String> {
    // target selection: flag > Zappr.yaml targets > host
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = project::load(&cwd, false)?;
    let target = rest
        .iter()
        .find(|a| a.starts_with("--target="))
        .map(|a| a.trim_start_matches("--target=").to_string())
        .or_else(|| rest.iter().find(|a| a.starts_with('-')).cloned())
        .or_else(|| p.config.targets.first().cloned())
        .unwrap_or_else(|| "linux".into());
    codegen::build(&p, &mut project::vm_for(&p)?, &target)
}

fn cmd_check(_rest: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = project::load(&cwd, true)?;
    let notes = project::check_project(&p)?;
    for n in &notes {
        eprintln!("zap check: {n}");
    }
    println!("zap check: {} module(s) clean", p.modules.len());
    Ok(())
}

fn cmd_test(_rest: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    // tests live in zap/tests/ — load everything including tests
    let p = project::load(&cwd, true)?;
    let mut vm = project::vm_for(&p)?;
    // run test.znz's entry: prefer `test`, then `main`, in the testsTest module
    for entry in ["testsTest.test", "testsTest.main", "test", "main"] {
        if let Some(()) = try_call(&mut vm, entry) {
            return Ok(());
        }
    }
    Err("no test entry point — expected zap/tests/test.znz with test() or main()".into())
}

fn try_call(vm: &mut interp::Vm, name: &str) -> Option<()> {
    // reach into fn_map via a public shim: run a named function if it exists
    match vm.call_named(name, Vec::new()) {
        Ok(_) => Some(()),
        Err(_) => None,
    }
}

fn cmd_fmt(_rest: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = project::load(&cwd, true)?;
    crate::fmt::run(&p.root)
}

fn cmd_verify(rest: &[String]) -> Result<(), String> {
    let artifact = rest
        .first()
        .ok_or("usage: zap verify <artifact>")?;
    linker::verify(std::path::Path::new(artifact))?;
    println!("{artifact}: signature OK");
    Ok(())
}

fn cmd_copyright(rest: &[String]) -> Result<(), String> {
    match rest.first().map(String::as_str) {
        Some("headers") => {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            crate::fmt::copyright_headers(&cwd)
        }
        _ => Err("usage: zap copyright headers".into()),
    }
}

fn print_help() {
    println!(
        "zap {VERSION} — the one way into Zappr

usage: zap <command>

  zap new <name>            scaffold project + Zappr.yaml
  zap run                   build (in-memory where possible) + execute
  zap build                 linux / windows / web — via flags or targets in Zappr.yaml
  zap check                 type-check only, fast feedback
  zap test                  run zap/tests/test.znz and its pipeline
  zap fmt                   the janitor (§18.3)
  zap verify                check an artifact's Ed25519 signature
  zap copyright headers     stamp license headers, on explicit intent only"
    );
}
