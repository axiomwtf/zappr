//! Static linking + Ed25519 artifact signing (SPEC §20.2, §18.2).
//!
//! Artifact layout: `<payload><"ZAPR"><pubkey:32><sig:64>` — the trailer
//! keeps native executables directly runnable (ELF ignores trailing data).
//! Signing uses the system OpenSSL CLI; `zap verify` checks magic +
//! signature before you run something you downloaded.

use std::path::{Path, PathBuf};
use std::process::Command;

const TRAILER_LEN: usize = 4 + 32 + 64;
const MAGIC: &[u8; 4] = b"ZAPR";
/// Ed25519 SubjectPublicKeyInfo DER prefix; raw pubkey appends after it.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// `zap verify <artifact>` — check the Ed25519 signature.
pub fn verify(artifact: &Path) -> Result<(), String> {
    let data = std::fs::read(artifact)
        .map_err(|e| format!("cannot read {}: {e}", artifact.display()))?;
    check_magic_and_signature(&data)
        .map_err(|e| format!("{}: {e}", artifact.display()))
}

fn check_magic_and_signature(data: &[u8]) -> Result<(), String> {
    if data.len() < TRAILER_LEN + 1 {
        return Err("artifact too small for signature block".into());
    }
    let t = &data[data.len() - TRAILER_LEN..];
    if &t[..4] != MAGIC {
        return Err("not a Zappr artifact (bad magic)".into());
    }
    let payload = &data[..data.len() - TRAILER_LEN];
    let pk_raw = &t[4..36];
    let sig = &t[36..100];

    // raw pubkey → SPKI DER for openssl
    let mut spki = Vec::with_capacity(44);
    spki.extend_from_slice(&ED25519_SPKI_PREFIX);
    spki.extend_from_slice(pk_raw);
    let dir = std::env::temp_dir();
    let tag = format!("zapverify-{}", std::process::id());
    let key = dir.join(format!("{tag}.der"));
    let msg = dir.join(format!("{tag}.msg"));
    let sf = dir.join(format!("{tag}.sig"));
    std::fs::write(&key, &spki).map_err(|e| e.to_string())?;
    std::fs::write(&msg, payload).map_err(|e| e.to_string())?;
    std::fs::write(&sf, sig).map_err(|e| e.to_string())?;
    let out = Command::new("openssl")
        .args(["pkeyutl", "-verify", "-pubin", "-keyform", "DER", "-inkey"])
        .arg(&key)
        .args(["-rawin", "-in"])
        .arg(&msg)
        .args(["-sigfile"])
        .arg(&sf)
        .output();
    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&msg);
    let _ = std::fs::remove_file(&sf);
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "signature verification FAILED{}",
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .map(|l| format!(": {l}"))
                .unwrap_or_default()
        )),
        Err(e) => Err(format!("cannot run openssl: {e}")),
    }
}

/// Sign `payload`, returning payload+trailer. Key: `signKey` in
/// Zappr.yaml (read via ZAP_SIGN_KEY env by caller), else ./signing.key.
pub fn sign(payload: &[u8]) -> Result<Vec<u8>, String> {
    let key = ensure_key()?;
    let pk_raw = pubkey_raw(&key)?;

    let dir = std::env::temp_dir();
    let tag = format!("zapsign-{}", std::process::id());
    let msg = dir.join(format!("{tag}.msg"));
    let sig = dir.join(format!("{tag}.sig"));
    std::fs::write(&msg, payload).map_err(|e| e.to_string())?;
    let out = Command::new("openssl")
        .args(["pkeyutl", "-sign", "-inkey", key.to_str().unwrap_or("signing.key"), "-rawin", "-in"])
        .arg(&msg)
        .args(["-out"])
        .arg(&sig)
        .output()
        .map_err(|e| format!("cannot run openssl: {e}"))?;
    let _ = std::fs::remove_file(&msg);
    if !out.status.success() {
        let _ = std::fs::remove_file(&sig);
        return Err(format!(
            "openssl sign failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let sig_bytes = std::fs::read(&sig).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&sig);
    if sig_bytes.len() != 64 {
        return Err(format!("unexpected signature length {}", sig_bytes.len()));
    }

    let mut out_b = Vec::with_capacity(payload.len() + TRAILER_LEN);
    out_b.extend_from_slice(payload);
    out_b.extend_from_slice(MAGIC);
    out_b.extend_from_slice(&pk_raw);
    out_b.extend_from_slice(&sig_bytes);
    Ok(out_b)
}

fn key_path() -> PathBuf {
    if let Ok(p) = std::env::var("ZAP_SIGN_KEY") {
        return PathBuf::from(p);
    }
    PathBuf::from("signing.key")
}

fn ensure_key() -> Result<PathBuf, String> {
    let path = key_path();
    if path.exists() {
        return Ok(path);
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    if let Err(e) = std::fs::create_dir_all(parent) {
        return Err(format!("cannot create {}: {e}", parent.display()));
    }
    let out = Command::new("openssl")
        .args(["genpkey", "-algorithm", "ED25519", "-out"])
        .arg(&path)
        .output()
        .map_err(|e| format!("cannot run openssl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "openssl genpkey failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    eprintln!("zap: generated signing key at {}", path.display());
    Ok(path)
}

/// Extract the raw 32-byte public key from an Ed25519 PEM private key.
fn pubkey_raw(key: &Path) -> Result<[u8; 32], String> {
    let out = Command::new("openssl")
        .args(["pkey", "-in"])
        .arg(key)
        .args(["-pubout", "-outform", "DER"])
        .output()
        .map_err(|e| format!("cannot run openssl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "openssl pkey failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let der = out.stdout;
    if der.len() < 44 {
        return Err("unexpected public key DER length".into());
    }
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&der[der.len() - 32..]);
    Ok(pk)
}
