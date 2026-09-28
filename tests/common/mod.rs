#![allow(dead_code)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
# This file is a cache directory tag created by cargo.\n\
# For information about cache directory tags see https://bford.info/cachedir/\n";

/// A fresh, empty directory unique to one test.
pub fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("target-gc-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create scratch");
    root.canonicalize().expect("canonicalize scratch")
}

/// A Cargo build directory the way Cargo lays one out: tagged, one profile
/// with its lock and the directories a compile writes into.
pub fn cargo_target(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("debug/deps")).expect("mkdir deps");
    fs::create_dir_all(dir.join("debug/.fingerprint")).expect("mkdir fingerprint");
    fs::write(dir.join("CACHEDIR.TAG"), CARGO_TAG).expect("write tag");
    fs::write(dir.join("debug/.cargo-lock"), "").expect("write lock");
    fs::write(dir.join("debug/deps/libx-abc.rlib"), vec![0u8; 256]).expect("write rlib");
    dir.to_path_buf()
}

/// Set a path's mtime `ago` in the past. Directories too: opening one
/// read-only is enough for `futimens`.
pub fn backdate(path: &Path, ago: Duration) {
    let file = File::open(path).expect("open for backdate");
    file.set_modified(SystemTime::now() - ago)
        .expect("set mtime");
}

/// Backdate a whole tree, deepest entries first, so creating nothing more
/// leaves every mtime in the past.
pub fn backdate_tree(root: &Path, ago: Duration) {
    let mut all = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        all.push(d.clone());
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                } else {
                    all.push(e.path());
                }
            }
        }
    }
    all.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for p in all {
        backdate(&p, ago);
    }
}

pub const DAY: Duration = Duration::from_secs(86_400);
