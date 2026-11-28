use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

fn hash_file(h: &mut DefaultHasher, p: &Path) {
    if let Ok(b) = std::fs::read(p) {
        p.hash(h);
        b.hash(h);
    }
}

fn walk(h: &mut DefaultHasher, dir: &Path, skip: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p == skip || p.starts_with(skip) {
            continue;
        }
        if p.is_dir() {
            walk(h, &p, skip);
        } else {
            hash_file(h, &p);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut h = DefaultHasher::new();
    walk(&mut h, &manifest.join("src"), &manifest.join("src/hosts"));
    hash_file(&mut h, &manifest.join("Cargo.toml"));
    hash_file(&mut h, &manifest.join("build.rs"));
    println!("cargo:rustc-env=ZAP_SRC_FP={}", h.finish());
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
}
