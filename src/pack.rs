//! Project packing: `zap build` embeds the whole project source tree
//! into the host binary so the artifact is one self-contained file.
//!
//! Layout appended to the host executable:
//!   [archive: u32 count, per-file (u32 path_len, path, u64 len, bytes)]
//!   [u64 archive_len][u32 magic "ZAPP"]
//! An optional ZAPR signature trailer (§18.2) may follow the footer;
//! both are ignored by ELF/PE loaders.

use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"ZAPP";
const FOOTER_LEN: usize = 8 + 4;
/// ZAPR signature trailer length (linker.rs TRAILER_LEN)
const SIG_TRAILER_LEN: usize = 100;

/// Collect every file the artifact needs: Zappr.yaml, all of zap/,
/// and any cli:/signKey-adjacent files referenced from the config.
/// The private signing key is never embedded.
pub fn collect_files(p: &crate::project::Project) -> Result<Vec<(String, PathBuf)>, String> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let yaml = p.root.join("Zappr.yaml");
    out.push(("Zappr.yaml".into(), yaml));

    // §18.4: the license travels with the artifact — LICENSE rides inside
    // the embedded archive, so every artifact carries its license text.
    let lic = p.root.join("LICENSE");
    if lic.is_file() {
        out.push(("LICENSE".into(), lic));
    }

    let zap_dir = p.root.join("zap");
    collect_dir(&zap_dir, &p.root, &mut out)?;

    // cli: entries are root-relative paths that may live outside zap/
    for rel in &p.config.cli {
        let path = p.root.join(rel);
        if path.is_file() {
            out.push((rel.replace('\\', "/"), path));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

fn collect_dir(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect_dir(&p, base, out)?;
        } else if p.is_file() {
            let rel = p.strip_prefix(base).unwrap_or(&p);
            out.push((rel.to_string_lossy().replace('\\', "/"), p));
        }
    }
    Ok(())
}

/// Serialize the project into the archive blob (without host/footer).
pub fn pack(p: &crate::project::Project) -> Result<Vec<u8>, String> {
    let files = collect_files(p)?;
    let mut buf = Vec::new();
    buf.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (rel, path) in &files {
        let data = std::fs::read(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let pb = rel.as_bytes();
        buf.extend_from_slice(&(pb.len() as u32).to_le_bytes());
        buf.extend_from_slice(pb);
        buf.extend_from_slice(&(data.len() as u64).to_le_bytes());
        buf.extend_from_slice(&data);
    }
    Ok(buf)
}

/// Full appended section: archive + length footer.
pub fn pack_section(p: &crate::project::Project) -> Result<Vec<u8>, String> {
    let archive = pack(p)?;
    let mut out = Vec::with_capacity(archive.len() + FOOTER_LEN);
    out.extend_from_slice(&archive);
    out.extend_from_slice(&(archive.len() as u64).to_le_bytes());
    out.extend_from_slice(MAGIC);
    Ok(out)
}

/// Locate an embedded project archive at the tail of `data`.
/// Returns the archive bytes (count+files, no footer) if present.
pub fn find_embedded(data: &[u8]) -> Option<Vec<u8>> {
    // footer may sit at EOF (unsigned) or before a 100-byte ZAPR trailer
    for back in [0usize, SIG_TRAILER_LEN] {
        let end = data.len().checked_sub(back)?;
        if end < FOOTER_LEN {
            continue;
        }
        let f = &data[end - FOOTER_LEN..end];
        if &f[8..12] != MAGIC {
            continue;
        }
        let len = u64::from_le_bytes(f[..8].try_into().ok()?) as usize;
        if len > end - FOOTER_LEN {
            continue;
        }
        let start = end - FOOTER_LEN - len;
        return Some(data[start..start + len].to_vec());
    }
    None
}

/// Extract an archive blob into `dir`, returning the project root.
pub fn extract(archive: &[u8], dir: &Path) -> Result<(), String> {
    if archive.len() < 4 {
        return Err("embedded project archive is truncated".into());
    }
    let mut pos = 0usize;
    let rd32 = |pos: &mut usize| -> Result<u32, String> {
        if *pos + 4 > archive.len() {
            return Err("embedded project archive is truncated".into());
        }
        let v = u32::from_le_bytes(archive[*pos..*pos + 4].try_into().unwrap());
        *pos += 4;
        Ok(v)
    };
    let count = rd32(&mut pos)? as usize;
    for _ in 0..count {
        let plen = rd32(&mut pos)? as usize;
        if pos + plen + 8 > archive.len() {
            return Err("embedded project archive is truncated".into());
        }
        let rel = std::str::from_utf8(&archive[pos..pos + plen])
            .map_err(|_| "embedded project archive has invalid path".to_string())?;
        pos += plen;
        // path safety: relative, no traversal
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("embedded project archive has unsafe path `{rel}`"));
        }
        let dlen = u64::from_le_bytes(
            archive[pos..pos + 8].try_into().unwrap(),
        ) as usize;
        pos += 8;
        if pos + dlen > archive.len() {
            return Err("embedded project archive is truncated".into());
        }
        let dest = dir.join(rel_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&dest, &archive[pos..pos + dlen]).map_err(|e| e.to_string())?;
        pos += dlen;
    }
    Ok(())
}

/// Cache dir for an extracted embedded project, keyed by archive hash.
pub fn extract_dir(archive: &[u8]) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    archive.hash(&mut h);
    let base = std::env::var("XDG_CACHE_HOME").map(PathBuf::from).ok()
        .or_else(|| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")).ok());
    base.unwrap_or_else(std::env::temp_dir)
        .join("zap")
        .join("host")
        .join(format!("{:016x}", h.finish()))
}

/// Read this executable's tail looking for an embedded project.
/// Returns the extracted project root when running as an artifact.
#[cfg(not(target_os = "wasi"))]
pub fn embedded_project_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let data = std::fs::read(&exe).ok()?;
    let archive = find_embedded(&data)?;
    let dir = extract_dir(&archive);
    let marker = dir.join(".zap-host-ok");
    if !marker.exists() {
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        std::fs::create_dir_all(&dir).ok()?;
        extract(&archive, &dir).ok()?;
        std::fs::write(&marker, b"").ok()?;
    }
    Some(dir)
}

/// On wasi there is no readable exe and no real filesystem: the web
/// loader stashes the packed project archive at `/.zap-archive` inside
/// the WASI filesystem before `_start`, and we extract it to
/// `/.zap-host`.
#[cfg(target_os = "wasi")]
pub fn embedded_project_root() -> Option<PathBuf> {
    let data = std::fs::read("/.zap-archive").ok()?;
    let dir = PathBuf::from("/.zap-host");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    extract(&data, &dir).ok()?;
    Some(dir)
}
