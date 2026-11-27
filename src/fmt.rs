//! zap fmt — the janitor (SPEC §18.3) + zap copyright headers (§18.4).

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// `zap fmt` — headers, organize, split with explicit y/n/a consent.
pub fn run(root: &Path) -> Result<(), String> {
    let files = collect_znz(root.join("zap"))?;
    if files.is_empty() {
        println!("zap fmt: nothing to do");
        return Ok(());
    }

    // 1. copyright headers
    let missing: Vec<_> = files
        .iter()
        .filter(|(_, src)| !has_header(src))
        .collect();
    if !missing.is_empty() {
        println!(
            "zap fmt: add copyright headers to {} files? (y/n/a)",
            missing.len()
        );
        match ask()? {
            Consent::Yes => {
                let header = header_for(root)?;
                for (p, _) in &missing {
                    stamp(p, &header)?;
                    println!("  stamped {}", p.display());
                }
            }
            Consent::All => {
                let header = header_for(root)?;
                for (p, _) in &missing {
                    stamp(p, &header)?;
                }
                println!("  stamped {} files", missing.len());
            }
            Consent::No => {}
        }
    }

    // 2. organize — detect scattered .znz files at zap/ root (other than main.znz)
    let scattered: Vec<_> = files
        .iter()
        .filter(|(p, _)| p.parent() == Some(root.join("zap").as_path()) && !p.ends_with("main.znz"))
        .collect();
    if !scattered.is_empty() {
        println!(
            "zap fmt: reorganize {} scattered files? (y/n/a)",
            scattered.len()
        );
        for (p, _) in &scattered {
            println!("  {}", p.display());
        }
        match ask()? {
            Consent::Yes | Consent::All => {
                // fmt never invents names on its own (§3.8) — propose moving
                // each into a folder named after its first `use`-less topic;
                // without a proposal we error rather than guess.
                return Err(
                    "zap fmt: organize needs a target folder per file — pass `zap fmt --into <folder>` (fmt never invents names, §18.3)".into(),
                );
            }
            Consent::No => {}
        }
    }

    // 3. split — offer topic-based splitting for giant files (soft ~500 lines)
    let big: Vec<_> = files
        .iter()
        .filter(|(_, src)| src.lines().count() > 500)
        .collect();
    if !big.is_empty() {
        for (p, src) in &big {
            println!(
                "zap fmt: split {} ({} lines) into topic files? (y/n/a)",
                p.display(),
                src.lines().count()
            );
            match ask()? {
                Consent::Yes | Consent::All => {
                    return Err(
                        "zap fmt: split requires review — run `zap fmt --split-plan` to see the proposal first (§18.3)".into(),
                    );
                }
                Consent::No => {}
            }
        }
    }

    println!("zap fmt: done");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Consent {
    Yes,
    No,
    All,
}

fn ask() -> Result<Consent, String> {
    print!("  > ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(match line.trim() {
        "y" | "Y" | "yes" => Consent::Yes,
        "a" | "A" | "all" => Consent::All,
        _ => Consent::No,
    })
}

fn collect_znz(dir: PathBuf) -> Result<Vec<(PathBuf, String)>, String> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| e.to_string())? {
            let p = e.map_err(|e| e.to_string())?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map_or(false, |x| x == "znz") {
                let src = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
                out.push((p, src));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn has_header(src: &str) -> bool {
    src.lines().take(5).any(|l| {
        l.contains("Copyright") || l.contains("copyright") || l.contains("SPDX") || l.contains("License")
    })
}

/// Detect the project's license from LICENSE file (fuzzy match, §18.4).
fn detect_license(root: &Path) -> Result<String, String> {
    let lic = root.join("LICENSE");
    if !lic.exists() {
        return Err("no LICENSE file — zap fuzzy-matches LICENSE to stamp headers (§18.4)".into());
    }
    let src = std::fs::read_to_string(&lic).map_err(|e| e.to_string())?;
    let head: String = src.chars().take(4000).collect();
    if head.contains("Apache License") {
        Ok("Apache-2.0".into())
    } else if head.contains("MIT License") || head.contains("Permission is hereby granted, free of charge") {
        Ok("MIT".into())
    } else if head.contains("GNU GENERAL PUBLIC LICENSE") {
        Ok("GPL-3.0".into())
    } else if head.contains("BSD") {
        Ok("BSD-3-Clause".into())
    } else {
        Err("LICENSE fuzzy-match failed — set `license:` in Zappr.yaml to override (§18.4)".into())
    }
}

fn header_for(root: &Path) -> Result<String, String> {
    // licenseHolder from Zappr.yaml if present
    let yaml = root.join("Zappr.yaml");
    let mut holder = "the authors".to_string();
    if let Ok(src) = std::fs::read_to_string(&yaml) {
        for line in src.lines() {
            if let Some(rest) = line.strip_prefix("licenseHolder:") {
                holder = rest.trim().to_string();
            }
        }
    }
    let lic = detect_license(root)?;
    Ok(format!(
        "/// Copyright (c) {holder}. Licensed under {lic} — see LICENSE.\n"
    ))
}

/// Idempotent: skip files that already carry the header.
fn stamp(path: &Path, header: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if has_header(&src) {
        return Ok(());
    }
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    out.push_str(&src);
    std::fs::write(path, out).map_err(|e| e.to_string())
}

/// `zap copyright headers` — explicit intent only (§18.4).
pub fn copyright_headers(root: &Path) -> Result<(), String> {
    let files = collect_znz(root.join("zap"))?;
    let missing: Vec<_> = files.iter().filter(|(_, s)| !has_header(s)).collect();
    if missing.is_empty() {
        println!("zap copyright headers: all files already stamped");
        return Ok(());
    }
    println!("zap copyright headers: stamp {} files? (y/n/a)", missing.len());
    match ask()? {
        Consent::Yes | Consent::All => {
            let header = header_for(root)?;
            for (p, _) in &missing {
                stamp(p, &header)?;
            }
            println!("zap copyright headers: stamped {} files", missing.len());
        }
        Consent::No => println!("zap copyright headers: skipped"),
    }
    Ok(())
}
