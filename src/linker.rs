//! Static linking + Ed25519 artifact signing (SPEC §20.2, §18.2).
//!
//! Artifact layout: `<payload><"ZAPR"><pubkey:32><sig:64>` — the trailer
//! keeps native executables directly runnable (ELF ignores trailing data).
//! Signing is pure Rust (ed25519-dalek); `zap verify` checks magic +
//! signature before you run something you downloaded. No external tools.

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

const TRAILER_LEN: usize = 4 + 32 + 64;
const MAGIC: &[u8; 4] = b"ZAPR";

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
        // an unsigned artifact (sign:false) is a real artifact with no
        // trailer — say so instead of a misleading "bad magic"
        if crate::pack::find_embedded(data).is_some() {
            return Err("artifact is unsigned (sign: false in Zappr.yaml) — nothing to verify".into());
        }
        return Err("not a Zappr artifact (bad magic)".into());
    }
    let payload = &data[..data.len() - TRAILER_LEN];
    let pk_raw: &[u8; 32] = t[4..36].try_into().unwrap();
    let sig: &[u8; 64] = t[36..100].try_into().unwrap();

    let vk = VerifyingKey::from_bytes(pk_raw)
        .map_err(|e| format!("bad public key: {e}"))?;
    let sig = Signature::from_bytes(sig);
    vk.verify(payload, &sig)
        .map_err(|_| "signature verification FAILED".to_string())
}

/// Sign `payload`, returning payload+trailer. `key` overrides the
/// `signKey`/ZAP_SIGN_KEY/./signing.key resolution (project-relative paths
/// are resolved by the caller).
pub fn sign(payload: &[u8], key: Option<&Path>) -> Result<Vec<u8>, String> {
    let key = match key {
        Some(k) => {
            let k = k.to_path_buf();
            if !k.exists() {
                return Err(format!("signKey not found: {}", k.display()));
            }
            k
        }
        None => ensure_key()?,
    };
    let seed = seed_from_key(&key)?;
    let sk = SigningKey::from_bytes(&seed);
    let sig: [u8; 64] = sk.sign(payload).to_bytes();
    let pk_raw = sk.verifying_key().to_bytes();

    let mut out_b = Vec::with_capacity(payload.len() + TRAILER_LEN);
    out_b.extend_from_slice(payload);
    out_b.extend_from_slice(MAGIC);
    out_b.extend_from_slice(&pk_raw);
    out_b.extend_from_slice(&sig);
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
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|e| format!("cannot generate signing key: {e}"))?;
    std::fs::write(&path, seed).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    eprintln!("zap: generated signing key at {}", path.display());
    Ok(path)
}

/// Extract the 32-byte Ed25519 seed from a key file. Accepts:
/// - raw 32-byte seed files (what `ensure_key` writes)
/// - 64-byte seed||pub concatenations (first 32 bytes)
/// - PEM PKCS8 (openssl `genpkey -algorithm ED25519`) — base64 body is
///   DER whose last 32 bytes are the seed
/// - raw PKCS8 DER — same last-32-bytes rule
fn seed_from_key(key: &Path) -> Result<[u8; 32], String> {
    let data = std::fs::read(key).map_err(|e| format!("cannot read {}: {e}", key.display()))?;
    let der = if data.starts_with(b"-----") {
        let text = String::from_utf8_lossy(&data);
        let body: String = text
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        b64decode(&body).ok_or("malformed PEM key")?
    } else {
        data
    };
    let seed_src: &[u8] = match der.len() {
        32 => &der,
        64 => &der[..32],
        n if n > 32 => &der[n - 32..],
        _ => return Err(format!("{}: key too short ({} bytes)", key.display(), der.len())),
    };
    let mut seed = [0u8; 32];
    seed.copy_from_slice(seed_src);
    Ok(seed)
}

/// Standard-alphabet base64 decode; whitespace is skipped.
fn b64decode(s: &str) -> Option<Vec<u8>> {
    let mut tbl = [255u8; 256];
    for (i, c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".iter().enumerate() {
        tbl[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for &b in s.as_bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        let v = tbl[b as usize];
        if v == 255 {
            return None;
        }
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}
