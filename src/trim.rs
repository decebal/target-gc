//! Superseded artifacts inside a build directory that is kept.
//!
//! Cargo names every unit's outputs `<stem>-<16 hex>` in `<profile>/deps/`,
//! and a change to a unit's inputs gives it a new hash without deleting the
//! old outputs. A long-lived or cloned build directory therefore carries every
//! build it ever did: one held sixteen 230 MB copies of the same test binary.
//!
//! The invariant: removing an artifact can cost a rebuild of that unit, never
//! a broken build. Cargo rebuilds a unit whose output is missing, so the only
//! way to break one is to delete while it runs, and [`trim`] holds every
//! profile lock while it deletes. Within each stem the newest hashes always
//! stay, so a dependency compiled once long ago is never touched.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::discover::{allocated_bytes, profile_dirs};
use crate::liveness::hold_locks;

/// All files in one `deps/` directory that share a stem and a hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub stem: String,
    pub hash: String,
    pub paths: Vec<PathBuf>,
    /// Newest modification among `paths`.
    pub modified: SystemTime,
}

/// One `deps/` entry, as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub modified: SystemTime,
}

/// `libfoo-0123456789abcdef.rlib` → `("libfoo", "0123456789abcdef")`. A name
/// without a 16-hex hash (an uplifted `libfoo.rlib`, rustc scratch) is `None`
/// and never planned.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    let head = name.split_once('.').map_or(name, |(h, _)| h);
    let (stem, hash) = head.rsplit_once('-')?;
    let is_hash = hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit());
    (is_hash && !stem.is_empty()).then_some((stem, hash))
}

/// Group entries into artifacts. Pure.
pub fn artifacts(entries: &[Entry]) -> Vec<Artifact> {
    let mut by_key: BTreeMap<(String, String), Artifact> = BTreeMap::new();
    for e in entries {
        let Some(name) = e.path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let Some((stem, hash)) = split_name(&name) else {
            continue;
        };
        let a = by_key
            .entry((stem.to_string(), hash.to_string()))
            .or_insert_with(|| Artifact {
                stem: stem.to_string(),
                hash: hash.to_string(),
                paths: Vec::new(),
                modified: SystemTime::UNIX_EPOCH,
            });
        a.paths.push(e.path.clone());
        a.modified = a.modified.max(e.modified);
    }
    by_key.into_values().collect()
}

/// Hashes always kept per stem. Cargo does not touch a fresh unit's outputs, so
/// mtime cannot tell a live feature variant from a dead one; two covers a crate
/// built both with and without a feature set (`--features test-support`).
pub const KEEP_PER_STEM: usize = 2;

/// The artifacts that may go: behind [`KEEP_PER_STEM`] newer hashes of the
/// same stem, and not modified since `cutoff`. Pure.
pub fn superseded(artifacts: Vec<Artifact>, cutoff: SystemTime) -> Vec<Artifact> {
    let mut by_stem: BTreeMap<String, Vec<Artifact>> = BTreeMap::new();
    for a in artifacts {
        by_stem.entry(a.stem.clone()).or_default().push(a);
    }
    let mut out = Vec::new();
    for (_, mut group) in by_stem {
        group.sort_by_key(|a| std::cmp::Reverse(a.modified));
        out.extend(
            group
                .into_iter()
                .skip(KEEP_PER_STEM)
                .filter(|a| a.modified < cutoff),
        );
    }
    out
}

/// What a trim of one build directory would remove.
#[derive(Debug, Default)]
pub struct TrimPlan {
    pub artifacts: Vec<Artifact>,
    /// Allocated bytes: an upper bound, for the same copy-on-write reason as
    /// [`allocated_bytes`].
    pub allocated: u64,
}

/// Plan a trim of every profile's `deps/` in `target`, leaving out any
/// artifact with a path `keep` claims (a running executable, a file a service
/// names).
pub fn plan(target: &Path, cutoff: SystemTime, keep: &dyn Fn(&Path) -> bool) -> TrimPlan {
    let mut out = TrimPlan::default();
    for profile in profile_dirs(target) {
        let entries = read_entries(&profile.join("deps"));
        for a in superseded(artifacts(&entries), cutoff) {
            if a.paths.iter().any(|p| keep(p)) {
                continue;
            }
            out.allocated += a.paths.iter().map(|p| allocated_bytes_of(p)).sum::<u64>();
            out.artifacts.push(a);
        }
    }
    out
}

/// Remove what `plan` named, holding every Cargo lock in `target` throughout.
/// `Err` names the lock a build holds; nothing is removed then. Returns the
/// number of artifacts removed and the paths that could not be.
pub fn trim(target: &Path, plan: &TrimPlan) -> Result<(usize, Vec<String>), PathBuf> {
    let _locks = hold_locks(target)?;
    let mut removed = 0;
    let mut errors = Vec::new();
    for a in &plan.artifacts {
        let mut ok = true;
        for p in &a.paths {
            let result = if p.is_dir() {
                fs::remove_dir_all(p)
            } else {
                fs::remove_file(p)
            };
            match result {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    ok = false;
                    errors.push(format!("{}: {e}", p.display()));
                }
            }
        }
        if ok {
            removed += 1;
        }
    }
    Ok((removed, errors))
}

fn read_entries(deps: &Path) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(deps) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let meta = fs::symlink_metadata(e.path()).ok()?;
            Some(Entry {
                path: e.path(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect()
}

fn allocated_bytes_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if path.is_dir() {
        return allocated_bytes(path);
    }
    fs::symlink_metadata(path).map_or(0, |m| m.blocks() * 512)
}
