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

    // 1. copyright headers — missing or stale (§18.4: updated in place)
    let header = match header_for(root) {
        Ok(h) => Some(h),
        Err(e) => {
            println!("zap fmt: headers skipped ({e})");
            None
        }
    };
    let needs_header: Vec<_> = match &header {
        Some(h) => files
            .iter()
            .filter(|(_, src)| !has_header(src) || stale_header_block(src, h).is_some())
            .collect(),
        None => Vec::new(),
    };
    if !needs_header.is_empty() {
        let h = header.as_deref().unwrap();
        println!(
            "zap fmt: add or update copyright headers in {} files? (y/n/a)",
            needs_header.len()
        );
        match ask()? {
            Consent::Yes => {
                for (p, _) in &needs_header {
                    stamp(p, h)?;
                    println!("  stamped {}", p.display());
                }
            }
            Consent::All => {
                for (p, _) in &needs_header {
                    stamp(p, h)?;
                }
                println!("  stamped {} files", needs_header.len());
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

/// Idempotent AND self-healing (§18.4): files missing the header get it
/// prepended; files carrying a stale header (different license text) get
/// the stale header block replaced in place.
fn stamp(path: &Path, header: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if let Some((start, end)) = stale_header_block(&src, header) {
        let lines: Vec<&str> = src.lines().collect();
        let mut out = String::with_capacity(src.len() + header.len());
        out.push_str(header.trim_end());
        out.push('\n');
        for l in &lines[end..] {
            out.push_str(l);
            out.push('\n');
        }
        let _ = start;
        return std::fs::write(path, out).map_err(|e| e.to_string());
    }
    if has_header(&src) {
        return Ok(());
    }
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    out.push_str(&src);
    std::fs::write(path, out).map_err(|e| e.to_string())
}

/// The file's leading `///` block when it is a copyright/license header
/// that differs from `header` — (first line idx, one-past-last line idx).
fn stale_header_block(src: &str, header: &str) -> Option<(usize, usize)> {
    let lines: Vec<&str> = src.lines().collect();
    let mut end = 0;
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("///") {
            end = i + 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let block = lines[..end].join("\n");
    let expected = header.trim_end();
    // a header from a previous stamping pass, but with old text → stale
    if block != expected && block.contains("Copyright") {
        Some((0, end))
    } else {
        None
    }
}

/// `zap copyright headers` — explicit intent only (§18.4).
pub fn copyright_headers(root: &Path) -> Result<(), String> {
    let files = collect_znz(root.join("zap"))?;
    let header = header_for(root)?;
    let targets: Vec<_> = files
        .iter()
        .filter(|(_, s)| !has_header(s) || stale_header_block(s, &header).is_some())
        .collect();
    if targets.is_empty() {
        println!("zap copyright headers: all files already stamped");
        return Ok(());
    }
    println!("zap copyright headers: stamp or update {} files? (y/n/a)", targets.len());
    match ask()? {
        Consent::Yes | Consent::All => {
            for (p, _) in &targets {
                stamp(p, &header)?;
            }
            println!("zap copyright headers: stamped {} files", targets.len());
        }
        Consent::No => println!("zap copyright headers: skipped"),
    }
    Ok(())
}

/* ================= organize + split (§18.3) ================= */

/// fused module name for a zap/-relative path without extension,
/// e.g. ["gui","widgets"] -> "guiWidgets" (same rule as project.rs).
fn fuse_mod(segs: &[String]) -> String {
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

/// `zap fmt --into <folder>`: move every scattered zap/-root .znz into
/// <folder>/ and mechanically rewrite use-paths + qualified calls.
pub fn organize_pub(root: &Path, folder: &str) -> Result<(), String> {
    let segs: Vec<String> = folder.split('/').map(|s| s.to_string()).collect();
    if segs.iter().any(|s| s.is_empty() || s != &s.to_lowercase() || !s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')) {
        return Err(format!("fmt --into: `{folder}` must be lowercase path segments (letters/digits/_/-)"));
    }
    let zapdir = root.join("zap");
    let scattered: Vec<PathBuf> = collect_znz(zapdir.clone())?
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.parent() == Some(zapdir.as_path()) && !p.ends_with("main.znz"))
        .collect();
    if scattered.is_empty() {
        println!("zap fmt: nothing scattered at zap/ root");
        return Ok(());
    }
    // plan: (old_path, new_path, old_mod, new_mod, use_old, use_new)
    let mut plan = Vec::new();
    for p in &scattered {
        let name = p.file_stem().unwrap().to_string_lossy().to_string();
        let mut dst_segs = segs.clone();
        dst_segs.push(name.clone());
        let new_rel = dst_segs.join("/");
        let new_path = zapdir.join(format!("{new_rel}.znz"));
        if new_path.exists() {
            return Err(format!("fmt --into: {} already exists", new_path.display()));
        }
        let old_mod = fuse_mod(&[name.clone()]);
        let new_mod = fuse_mod(&dst_segs);
        // use-path: dot-joined segments
        let use_old = name.clone();
        let use_new = dst_segs.join(".");
        plan.push((p.clone(), new_path, old_mod, new_mod, use_old, use_new));
    }
    println!("zap fmt: organize — move {} file(s) into `{folder}/`?", plan.len());
    for (o, n, _, _, _, _) in &plan {
        println!("  {} -> {}", o.display(), n.display());
    }
    match ask()? {
        Consent::No => return Ok(()),
        _ => {}
    }
    // apply moves
    for (o, n, _, _, _, _) in &plan {
        if let Some(par) = n.parent() { let _ = std::fs::create_dir_all(par); }
        std::fs::rename(o, n).map_err(|e| format!("move {}: {e}", o.display()))?;
    }
    // mechanical rewrite across all .znz under zap/
    let files = collect_znz(zapdir)?;
    for (p, src) in files {
        let mut out = src.clone();
        for (_, _, old_mod, new_mod, use_old, use_new) in &plan {
            // use.<old> -> use.<new>  (word boundary after)
            out = rewrite_use(&out, use_old, use_new);
            // <oldmod>. -> <newmod>.  qualified calls (boundary: not ident char before, '.' after)
            out = rewrite_ident_dot(&out, old_mod, new_mod);
        }
        if out != src {
            std::fs::write(&p, &out).map_err(|e| e.to_string())?;
            println!("  rewrote {}", p.display());
        }
    }
    println!("zap fmt: organized {} file(s) into `{folder}/`", plan.len());
    Ok(())
}

/// Replace `use.<old>` (whole path token) with `use.<new>`.
fn rewrite_use(src: &str, old: &str, new: &str) -> String {
    let pat = format!("use.{old}");
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find(&pat) {
        let before_ok = i == 0 || !is_ident(rest.as_bytes()[i - 1]);
        let after = &rest[i + pat.len()..];
        let after_ok = after.is_empty() || !is_ident(after.as_bytes()[0]);
        if before_ok && after_ok {
            out.push_str(&rest[..i]);
            out.push_str("use.");
            out.push_str(new);
            rest = after;
        } else {
            out.push_str(&rest[..i + pat.len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Replace `<old>.` qualified-name occurrences with `<new>.`.
fn rewrite_ident_dot(src: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + old.len() + 1 <= bytes.len()
            && &src[i..i + old.len()] == old
            && bytes[i + old.len()] == b'.'
            && (i == 0 || !is_ident(bytes[i - 1]))
        {
            out.push_str(new);
            out.push('.');
            i += old.len() + 1;
        } else {
            // copy one char (utf8-safe enough for source)
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&src[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn is_ident(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'_' }
fn utf8_len(b: u8) -> usize {
    if b < 0x80 { 1 } else if b >> 5 == 0b110 { 2 } else if b >> 4 == 0b1110 { 3 } else { 4 }
}

/// topic key for a fn name: first camelCase segment, lowercased.
fn topic_of(fn_name: &str) -> String {
    let mut end = fn_name.len();
    for (i, c) in fn_name.char_indices() {
        if i > 0 && c.is_ascii_uppercase() { end = i; break; }
    }
    fn_name[..end].to_lowercase()
}

/// Locate each top-level fn's line span via lexer brace matching.
fn fn_spans(src: &str) -> Result<Vec<(String, usize, usize)>, String> {
    let prog = crate::parser::parse(src).map_err(|e| format!("{}:{}: {}", e.line, e.col, e.msg))?;
    let toks = crate::lexer::lex(src).map_err(|e| e.msg)?;
    let mut fns: Vec<(String, usize)> = prog.items.iter().filter_map(|it| {
        if let crate::parser::Item::Fn { name, line, .. } = it { Some((name.clone(), *line)) } else { None }
    }).collect();
    fns.sort_by_key(|(_, l)| *l);
    let mut spans = Vec::new();
    for (name, start) in fns {
        // find first LBrace at/after start line, then match to RBrace
        let mut depth = 0i32; let mut end = start;
        let mut started = false;
        for t in &toks {
            if t.line < start { continue; }
            match t.tok {
                crate::lexer::Tok::LBrace => { depth += 1; started = true; }
                crate::lexer::Tok::RBrace => { depth -= 1; if started && depth == 0 { end = t.line; break; } }
                _ => {}
            }
        }
        spans.push((name, start, end.max(start)));
    }
    Ok(spans)
}

/// `zap fmt --split-plan [file]` — print the topic proposal, no writes.
pub fn split_plan_pub(root: &Path, only: Option<&str>) -> Result<(), String> {
    let files = collect_znz(root.join("zap"))?;
    for (p, src) in &files {
        if src.lines().count() <= 500 { continue; }
        if let Some(f) = only {
            if !p.ends_with(f) { continue; }
        }
        let spans = fn_spans(src)?;
        println!("{} ({} lines, {} fns)", p.display(), src.lines().count(), spans.len());
        let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for (n, _, _) in &spans {
            if n == "main" { continue; }
            groups.entry(topic_of(n)).or_default().push(n.clone());
        }
        for (topic, fns) in &groups {
            println!("  -> {topic}.znz  [{}]", fns.join(", "));
        }
    }
    Ok(())
}

/// `zap fmt --split [--into-consent]` — perform the split shown by --split-plan.
pub fn split_apply_pub(root: &Path) -> Result<(), String> {
    let files = collect_znz(root.join("zap"))?;
    for (p, src) in files {
        if src.lines().count() <= 500 { continue; }
        let spans = fn_spans(&src)?;
        let mut groups: std::collections::BTreeMap<String, Vec<(String, usize, usize)>> = Default::default();
        for sp in &spans {
            if sp.0 == "main" { continue; }
            groups.entry(topic_of(&sp.0)).or_default().push(sp.clone());
        }
        groups.retain(|_, fns| fns.len() >= 2);
        if groups.len() < 1 { continue; }
        println!("zap fmt: split {} into {} topic file(s)?", p.display(), groups.len());
        for (t, fns) in &groups {
            println!("  -> {}.znz [{}]", t, fns.iter().map(|f| f.0.clone()).collect::<Vec<_>>().join(", "));
        }
        match ask()? {
            Consent::No => continue,
            _ => {}
        }
        let dir = p.parent().unwrap().to_path_buf();
        // module name of this file (fused rel path)
        let rel = p.strip_prefix(root.join("zap")).unwrap();
        let old_segs: Vec<String> = rel.with_extension("").iter().map(|s| s.to_string_lossy().to_lowercase()).collect();
        let old_mod = fuse_mod(&old_segs);
        let lines: Vec<&str> = src.lines().collect();
        // blocks to remove from original: each moved fn's [start,end] plus
        // preceding comment/blank gap lines
        let mut moved: Vec<(String, usize, usize, String)> = Vec::new(); // name,start,end,newmod
        for (topic, fns) in &groups {
            let new_path = dir.join(format!("{topic}.znz"));
            if new_path.exists() {
                return Err(format!("fmt --split: {} already exists", new_path.display()));
            }
            let mut new_segs = old_segs.clone();
            // new module: same dir, filename = topic
            let file_segs: Vec<String> = rel.parent().map(|d| d.iter().map(|s| s.to_string_lossy().to_lowercase()).collect()).unwrap_or_default();
            let mut nsegs = file_segs.clone();
            nsegs.push(topic.clone());
            let new_mod = fuse_mod(&nsegs);
            let mut body = String::new();
            for (_, s, e) in fns {
                // include preceding /// comment lines
                let mut gs = *s;
                while gs > 1 && lines[gs - 2].trim_start().starts_with("///") { gs -= 1; }
                for l in &lines[gs - 1..*e] { body.push_str(l); body.push('\n'); }
                moved.push((String::new(), gs, *e, new_mod.clone()));
                let _ = new_segs.pop();
            }
            std::fs::write(&new_path, &body).map_err(|e| e.to_string())?;
            println!("  wrote {}", new_path.display());
        }
        // rewrite qualified calls oldmod.fn -> newmod.fn across all files
        let mut rewrites: Vec<(String, String)> = Vec::new();
        for (topic, fns) in &groups {
            let file_segs: Vec<String> = rel.parent().map(|d| d.iter().map(|s| s.to_string_lossy().to_lowercase()).collect()).unwrap_or_default();
            let mut nsegs = file_segs.clone();
            nsegs.push(topic.clone());
            let new_mod = fuse_mod(&nsegs);
            for (fname, _, _) in fns {
                rewrites.push((format!("{old_mod}.{fname}"), format!("{new_mod}.{fname}")));
            }
        }
        // remove moved blocks from original
        let mut out_lines: Vec<&str> = Vec::new();
        let mut skip_until = 0usize;
        for (idx, l) in lines.iter().enumerate() {
            let ln = idx + 1;
            if ln < skip_until { continue; }
            if let Some((_, _, end, _)) = moved.iter().find(|(_, s, _, _)| ln == *s) {
                skip_until = end + 1;
                continue;
            }
            out_lines.push(l);
        }
        // collapse >2 consecutive blanks
        let mut final_src = String::new();
        let mut blank = 0;
        for l in out_lines {
            if l.trim().is_empty() { blank += 1; if blank > 2 { continue; } }
            else { blank = 0; }
            final_src.push_str(l); final_src.push('\n');
        }
        std::fs::write(&p, &final_src).map_err(|e| e.to_string())?;
        // apply qualified rewrites in every zap file
        let all = collect_znz(root.join("zap"))?;
        for (fp, fsrc) in all {
            let mut o = fsrc.clone();
            for (a, b) in &rewrites { o = replace_ident_seq(&o, a, b); }
            if o != fsrc { std::fs::write(&fp, &o).map_err(|e| e.to_string())?; }
        }
    }
    Ok(())
}

/// replace `a` with `b` where both are dotted ident sequences, on word boundaries.
fn replace_ident_seq(src: &str, a: &str, b: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if src[i..].starts_with(a)
            && (i == 0 || !is_ident(bytes[i - 1]))
            && (i + a.len() >= bytes.len() || !is_ident(bytes[i + a.len()]))
        {
            out.push_str(b);
            i += a.len();
        } else {
            let l = utf8_len(bytes[i]);
            out.push_str(&src[i..i + l]);
            i += l;
        }
    }
    out
}
