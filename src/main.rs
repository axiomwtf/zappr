mod callcollector;
mod checker;
mod codegen;
mod config;
mod deps;
mod fmt;
mod guibridge;
mod interp;
mod lexer;
mod linker;
mod pack;
mod parser;
mod project;
mod uiruntime;
mod value;

use std::path::Path;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    // artifact mode: a `zap build` output carries an embedded project
    // archive at its tail — run it directly instead of the CLI.
    if let Some(root) = pack::embedded_project_root() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return match run_embedded(&root, &args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(msg) => {
                eprintln!("zap: {msg}");
                ExitCode::FAILURE
            }
        };
    }
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
        "__hostblob" => codegen::make_host_blob(
            rest.first().map(String::as_str).unwrap_or(""),
        ).map(|_| ()),
        other => cmd_custom(other, rest),
    }
}

/// Custom zap commands (§19.2): `cli:` files in Zappr.yaml are parsed and
/// their functions callable as `zap <fn>`. Only the app's own cli list
/// registers — dep cli: entries never do (§19 note).
fn cmd_custom(cmd: &str, rest: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let yaml = cwd.join("Zappr.yaml");
    if !yaml.exists() {
        return Err(format!("unknown command `{cmd}` — see `zap --help`"));
    }
    let p = std::sync::Arc::new(project::load(&cwd, false)?);
    if p.config.cli.is_empty() {
        return Err(format!("unknown command `{cmd}` — see `zap --help`"));
    }
    let mut vm = project::vm_for(&p)?;
    // parse each cli file; find a fn named `cmd`
    for rel in &p.config.cli {
        let path = p.root.join(rel);
        let src = std::fs::read_to_string(&path)
            .map_err(|e| format!("cli file {}: {e}", path.display()))?;
        let prog = parser::parse(&src).map_err(|e| {
            format!("{}:{}:{}: parse error: {}", path.display(), e.line, e.col, e.msg)
        })?;
        if !prog.items.iter().any(|it| {
            matches!(it, parser::Item::Fn { name, .. } if name == cmd)
        }) {
            continue;
        }
        let stem = Path::new(rel)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("cmd");
        let modname = format!(
            "cli{}",
            stem.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        );
        // the cli module gets checked like any module — its `val.` writes
        // are the one legal mutation path (§19.2)
        let mut progs: Vec<(&str, &parser::Program)> = p
            .modules
            .iter()
            .map(|(m, _, pr)| (m.as_str(), pr))
            .collect();
        progs.push((modname.as_str(), &prog));
        let globals = crate::checker::GlobalSyms::from_programs(progs.into_iter());
        let out = crate::checker::check_program(&prog, &modname, &globals);
        for w in &out.warnings {
            eprintln!("zap: {w}");
        }
        if !out.errors.is_empty() {
            return Err(format!(
                "{}: check failed:\n  {}",
                path.display(),
                out.errors.join("\n  ")
            ));
        }
        vm.load_module(&modname, &prog)
            .map_err(|e| format!("cli module `{modname}`: {}", e.msg))?;
        vm.set_cli_args(rest.to_vec());
        return match vm.call_named(cmd, Vec::new()) {
            Ok(_) => {
                // §19.2: the command ran with build privileges — persist
                // any `val.` writes back into Zappr.yaml
                write_back_vals(&p.root, &vm.prog.zap_yaml)?;
                Ok(())
            }
            Err(e) => Err(format!("`{cmd}` failed: {}", e.msg)),
        };
    }
    Err(format!("unknown command `{cmd}` — see `zap --help`"))
}

/// §19.2: a custom zap CLI runs with build privileges — `val.` writes
/// persist back into Zappr.yaml's `val:` block when the command finishes.
fn write_back_vals(root: &Path, vals: &crate::value::Value) -> Result<(), String> {
    let yv = value_to_yval(vals);
    let path = root.join("Zappr.yaml");
    let src = std::fs::read_to_string(&path).map_err(|e| format!("Zappr.yaml: {e}"))?;
    let out = splice_val_block(&src, &yv);
    if out != src {
        std::fs::write(&path, out).map_err(|e| format!("Zappr.yaml: {e}"))?;
    }
    Ok(())
}

fn value_to_yval(v: &crate::value::Value) -> crate::config::YVal {
    use crate::config::YVal;
    match v {
        crate::value::Value::Str(s) => YVal::Str(s.lock().unwrap().clone()),
        crate::value::Value::Int(i) => YVal::Int(*i as i64),
        crate::value::Value::Float(f) => YVal::Float(*f),
        crate::value::Value::Bool(b) => YVal::Bool(*b),
        crate::value::Value::List(l) => {
            YVal::Seq(l.lock().unwrap().iter().map(value_to_yval).collect())
        }
        crate::value::Value::Map(m) => {
            let mut out = std::collections::BTreeMap::new();
            for (k, v) in m.lock().unwrap().iter() {
                if let crate::value::Value::Str(ks) = k {
                    out.insert(ks.lock().unwrap().clone(), value_to_yval(v));
                }
            }
            YVal::Map(out)
        }
        _ => YVal::Null,
    }
}

/// Replace (or append) the `val:` block in Zappr.yaml, preserving the rest.
fn splice_val_block(src: &str, vals: &crate::config::YVal) -> String {
    let block = emit_val_block(vals);
    let mut out = String::new();
    let mut in_val = false;
    let mut replaced = false;
    for line in src.lines() {
        let top = !line.starts_with([' ', '\t', '-']) && !line.trim().is_empty();
        if top && line.trim_start().starts_with("val:") {
            in_val = true;
            out.push_str(&block);
            replaced = true;
            continue;
        }
        if in_val {
            if top {
                in_val = false;
                out.push_str(line);
                out.push('\n');
            }
            continue; // drop the old val block
        }
        out.push_str(line);
        out.push('\n');
    }
    if !replaced {
        out.push_str(&block);
    }
    out
}

fn emit_val_block(vals: &crate::config::YVal) -> String {
    let mut s = String::from("val:\n");
    match vals {
        crate::config::YVal::Map(m) => {
            if m.is_empty() {
                return "val: {}\n".into();
            }
            for (k, v) in m {
                emit_yval(&mut s, k, v, 2);
            }
        }
        crate::config::YVal::Null => return "val: {}\n".into(),
        _ => s.push_str(" {}\n"),
    }
    s
}

fn emit_yval(out: &mut String, key: &str, v: &crate::config::YVal, indent: usize) {
    let pad = " ".repeat(indent);
    match v {
        crate::config::YVal::Map(m) => {
            if m.is_empty() {
                out.push_str(&format!("{pad}{key}: {{}}\n"));
                return;
            }
            out.push_str(&format!("{pad}{key}:\n"));
            for (k, v) in m {
                emit_yval(out, k, v, indent + 2);
            }
        }
        crate::config::YVal::Seq(s) => {
            if s.is_empty() {
                out.push_str(&format!("{pad}{key}: []\n"));
                return;
            }
            out.push_str(&format!("{pad}{key}:\n"));
            for item in s {
                emit_seq_item(out, item, indent + 2);
            }
        }
        _ => out.push_str(&format!("{pad}{key}: {}\n", scalar_yaml(v))),
    }
}

fn emit_seq_item(out: &mut String, v: &crate::config::YVal, indent: usize) {
    let pad = " ".repeat(indent);
    match v {
        crate::config::YVal::Map(m) => {
            let mut it = m.iter();
            match it.next() {
                Some((k, first)) => match first {
                    crate::config::YVal::Map(_) | crate::config::YVal::Seq(_) => {
                        out.push_str(&format!("{pad}- {k}:\n"));
                        emit_yval(out, k, first, indent + 4);
                    }
                    _ => out.push_str(&format!("{pad}- {k}: {}\n", scalar_yaml(first))),
                },
                None => out.push_str(&format!("{pad}- {{}}\n")),
            }
            for (k, v) in it {
                emit_yval(out, k, v, indent + 2);
            }
        }
        crate::config::YVal::Seq(s) => {
            out.push_str(&format!("{pad}-\n"));
            for item in s {
                emit_seq_item(out, item, indent + 2);
            }
        }
        _ => out.push_str(&format!("{pad}- {}\n", scalar_yaml(v))),
    }
}

fn scalar_yaml(v: &crate::config::YVal) -> String {
    match v {
        crate::config::YVal::Str(s) => {
            let needs_quote = s.is_empty()
                || s.contains(": ")
                || s.contains(" #")
                || s.trim() != s
                || s.contains('\n')
                || s.starts_with([
                    '{', '[', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`', '-',
                ])
                || s.parse::<f64>().is_ok()
                || matches!(s.as_str(), "true" | "false" | "null" | "yes" | "no");
            if needs_quote {
                format!(
                    "\"{}\"",
                    s.replace('\\', "\\\\").replace('"', "\\\"")
                )
            } else {
                s.clone()
            }
        }
        crate::config::YVal::Int(i) => i.to_string(),
        crate::config::YVal::Float(f) => format!("{f}"),
        crate::config::YVal::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Run an embedded project extracted from the artifact tail.
fn run_embedded(root: &Path, cli_args: &[String]) -> Result<(), String> {
    let p = std::sync::Arc::new(project::load(root, false)?);
    let mut vm = project::vm_for(&p)?;
    vm.set_cli_args(cli_args.to_vec());
    #[cfg(target_os = "wasi")]
    {
        // on web, ui.run returns after attaching the GUI tree and the
        // JS shim keeps calling back into the Vm (zap_gui_step → click
        // handlers) after _start has returned; the Vm must be leaked
        // so it outlives this stack frame.
        let vm: &'static mut interp::Vm = Box::leak(Box::new(vm));
        return match vm.run_main() {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("runtime error: {}", e.msg)),
        };
    }
    #[cfg(not(target_os = "wasi"))]
    match vm.run_main() {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("runtime error: {}", e.msg)),
    }
}

fn cmd_new(rest: &[String]) -> Result<(), String> {
    let name = rest
        .first()
        .ok_or("usage: zap new <name>")?;
    let dir = rest.get(1).map(String::as_str).unwrap_or(".");
    project::scaffold(name, Path::new(dir))
}

fn cmd_run(rest: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = std::sync::Arc::new(project::load(&cwd, false)?);
    // the checker gates execution too — compile errors never reach runtime
    let notes = project::check_project(&p)?;
    for n in &notes {
        eprintln!("zap: {n}");
    }
    let mut vm = project::vm_for(&p)?;
    // argv for args.* (§9.5): everything after `--`, else all of rest
    let args: Vec<String> = if let Some(i) = rest.iter().position(|a| a == "--") {
        rest[i + 1..].to_vec()
    } else {
        rest.to_vec()
    };
    vm.set_cli_args(args);
    match vm.run_main() {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("runtime error: {}", e.msg)),
    }
}

fn cmd_build(rest: &[String]) -> Result<(), String> {
    // target selection: flag > Zappr.yaml targets > host
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = std::sync::Arc::new(project::load(&cwd, false)?);
    let release = rest.iter().any(|a| a == "--release" || a == "-r");
    let target = rest
        .iter()
        .find_map(|a| a.strip_prefix("--target="))
        .map(str::to_string)
        .or_else(|| {
            rest.iter()
                .find(|a| a.starts_with('-') && !a.starts_with("--") && *a != "-r")
                .cloned()
        })
        .or_else(|| p.config.targets.first().cloned())
        .unwrap_or_else(|| {
            if cfg!(windows) { "windows".into() } else { "linux".into() }
        });
    codegen::build(&p, &mut project::vm_for(&p)?, &target, release)
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
    let p = std::sync::Arc::new(project::load(&cwd, true)?);
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

fn cmd_fmt(rest: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = project::find_root(&cwd)?;
    if let Some(i) = rest.iter().position(|a| a == "--into") {
        let folder = rest.get(i + 1).ok_or("usage: zap fmt --into <folder>")?;
        return crate::fmt::organize_pub(&root, folder);
    }
    if rest.iter().any(|a| a == "--split-plan") {
        let only = rest.iter().position(|a| a == "--split-plan").and_then(|i| rest.get(i + 1)).map(String::as_str);
        return crate::fmt::split_plan_pub(&root, only);
    }
    if rest.iter().any(|a| a == "--split") {
        return crate::fmt::split_apply_pub(&root);
    }
    crate::fmt::run(&root)
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
