mod callcollector;
mod checker;
mod codegen;
mod lexer;
mod linker;
mod parser;
mod uiruntime;

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
    // TODO: scaffold project + Zappr.yaml (SPEC §18)
    println!("zap new {name}: not implemented yet");
    Ok(())
}

fn cmd_run(_rest: &[String]) -> Result<(), String> {
    // TODO: build (in-memory where possible) + execute (SPEC §18)
    Err("zap run: not implemented yet".into())
}

fn cmd_build(_rest: &[String]) -> Result<(), String> {
    // TODO: linux / windows / web targets, Ed25519 signing (SPEC §18.1, §18.2)
    Err("zap build: not implemented yet".into())
}

fn cmd_check(_rest: &[String]) -> Result<(), String> {
    // TODO: type-check only, fast feedback (SPEC §18)
    Err("zap check: not implemented yet".into())
}

fn cmd_test(_rest: &[String]) -> Result<(), String> {
    // TODO: run zap/tests/test.znz and its pipeline (SPEC §17)
    Err("zap test: not implemented yet".into())
}

fn cmd_fmt(_rest: &[String]) -> Result<(), String> {
    // TODO: the janitor — headers, organize, split, with y/n/a consent (SPEC §18.3)
    Err("zap fmt: not implemented yet".into())
}

fn cmd_verify(_rest: &[String]) -> Result<(), String> {
    // TODO: check an artifact's Ed25519 signature (SPEC §18.2)
    Err("zap verify: not implemented yet".into())
}

fn cmd_copyright(rest: &[String]) -> Result<(), String> {
    match rest.first().map(String::as_str) {
        Some("headers") => {
            // TODO: stamp license headers, on explicit intent only (SPEC §18.4)
            Err("zap copyright headers: not implemented yet".into())
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
