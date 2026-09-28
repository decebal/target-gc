//! Finding Cargo build directories under a set of roots.
//!
//! A directory counts only when it holds the `CACHEDIR.TAG` Cargo writes, and
//! the tag says Cargo wrote it. The name is never evidence: a directory called
//! `target` inside `node_modules` is a package's source, and a build directory
//! moved by `CARGO_TARGET_DIR` or `build.target-dir` may be called anything.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::Target;

/// The standard cache-directory signature (bford.info/cachedir).
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";
/// What Cargo writes on the tag's second line. Other tools use the same
/// signature for their own caches, so the signature alone is not enough.
const CARGO_TAG_MARK: &str = "created by cargo";

/// Prefix of a build directory already renamed aside for deletion by an
/// earlier run that was interrupted before the delete finished.
pub const TRASH_PREFIX: &str = ".target-gc-trash.";

/// Never descended into. None of them holds a build directory worth finding,
/// and `node_modules` is where a name-matching cleaner does its damage.
const SKIP_NAMES: [&str; 8] = [
    ".git",
    "node_modules",
    ".Trash",
    ".npm",
    ".bun",
    ".pnpm-store",
    ".rustup",
    ".cargo",
];

/// The files Cargo keeps its per-profile locks in. Their presence marks a
/// profile directory; see [`crate::liveness`] for what holding them means.
pub const LOCK_FILES: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

#[derive(Debug, Default)]
pub struct Found {
    pub targets: Vec<Target>,
    /// Build directories an interrupted run renamed aside and never finished
    /// deleting.
    pub trash: Vec<PathBuf>,
    /// Directories that could not be read (permission, a vanished path).
    pub unreadable: usize,
    /// The walk hit its deadline; what was found is still accurate, the rest
    /// was not looked at.
    pub incomplete: bool,
}

pub struct Walk<'a> {
    pub roots: &'a [PathBuf],
    /// Absolute paths never descended into (e.g. `~/Library`).
    pub skip_paths: &'a [PathBuf],
    pub max_depth: usize,
    pub deadline: Instant,
}

pub fn find(walk: &Walk<'_>) -> Found {
    let mut found = Found::default();
    let mut stack: Vec<(PathBuf, usize)> = walk.roots.iter().map(|r| (r.clone(), 0)).collect();

    while let Some((dir, depth)) = stack.pop() {
        if Instant::now() >= walk.deadline {
            found.incomplete = true;
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            found.unreadable += 1;
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_NAMES.contains(&name.as_ref()) || walk.skip_paths.contains(&path) {
                continue;
            }
            if name.starts_with(TRASH_PREFIX) {
                if is_cargo_build_dir(&path) {
                    found.trash.push(path);
                }
                continue;
            }
            if is_cargo_build_dir(&path) {
                found.targets.push(target_at(path));
                continue;
            }
            if depth + 1 < walk.max_depth {
                stack.push((path, depth + 1));
            }
        }
    }
    found.targets.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

pub fn target_at(path: PathBuf) -> Target {
    let project = path
        .parent()
        .map_or_else(|| path.clone(), Path::to_path_buf);
    let last_activity = last_activity(&path);
    Target {
        path,
        project,
        last_activity,
    }
}

/// `true` when `dir` holds a `CACHEDIR.TAG` that Cargo wrote.
pub fn is_cargo_build_dir(dir: &Path) -> bool {
    let Ok(tag) = fs::read_to_string(dir.join("CACHEDIR.TAG")) else {
        return false;
    };
    tag.starts_with(CACHEDIR_SIGNATURE) && tag.contains(CARGO_TAG_MARK)
}

/// A profile directory is where Cargo keeps its lock files: `<target>/debug`,
/// or `<target>/<triple>/release` for a cross build.
pub fn is_profile_dir(dir: &Path) -> bool {
    LOCK_FILES.iter().any(|lock| dir.join(lock).is_file())
}

/// Every profile directory in a build directory: one level down for host
/// builds, two for cross builds.
pub fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for level1 in subdirs(target) {
        if is_profile_dir(&level1) {
            out.push(level1);
            continue;
        }
        out.extend(subdirs(&level1).into_iter().filter(|d| is_profile_dir(d)));
    }
    out.sort();
    out
}

/// When Cargo last wrote into this build directory.
///
/// Compiling a crate creates files in `<profile>/deps`, `.fingerprint`, `build`
/// and `incremental`, which moves THOSE directories' mtimes; the build
/// directory's own mtime barely moves. So this reads the build directory, its
/// children, their children, and the children of every profile directory —
/// a few hundred stats, never a walk of the tens of thousands of artifacts.
/// Access time is not used: APFS does not maintain it and Linux coalesces it.
pub fn last_activity(target: &Path) -> SystemTime {
    let mut newest = mtime(target);
    for level1 in children(target) {
        newest = newest.max(mtime(&level1));
        if !level1.is_dir() {
            continue;
        }
        for level2 in children(&level1) {
            newest = newest.max(mtime(&level2));
            if level2.is_dir() && is_profile_dir(&level2) {
                for level3 in children(&level2) {
                    newest = newest.max(mtime(&level3));
                }
            }
        }
    }
    newest
}

fn mtime(path: &Path) -> SystemTime {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn children(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

/// Allocated bytes under `dir` — an UPPER bound on what deleting it frees,
/// because copy-on-write clones share blocks. Shown for orientation only; an
/// eviction measures the real number with `df`.
pub fn allocated_bytes(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            total += meta.blocks() * 512;
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}
