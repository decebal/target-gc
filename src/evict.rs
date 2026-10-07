//! Removing one build directory without ever racing a build.
//!
//! The order is the safety property:
//!
//! 1. re-verify, from disk, everything the plan assumed — still a Cargo build
//!    directory, not a symlink, not built since the scan, no build writer in
//!    the project and no process running from it;
//! 2. take every Cargo profile lock and HOLD them, so no build can start;
//! 3. rename the directory aside, under a name Cargo will never look for;
//! 4. release the locks, then delete the renamed copy.
//!
//! A Cargo that starts during step 3 waits on the lock, then finds no build
//! directory and creates a fresh one — a cold build, never a half-deleted
//! one. A run killed during step 4 leaves a `.target-gc-trash.*` directory
//! that the next run finds and finishes.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::discover::{is_cargo_build_dir, last_activity, TRASH_PREFIX};
use crate::liveness::{hold_locks, owner, Evidence};
use crate::Target;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Something changed since the plan; the directory stays.
    Skipped(String),
    /// The directory could not be moved or deleted.
    Failed(String),
}

impl Refusal {
    pub fn reason(&self) -> &str {
        match self {
            Refusal::Skipped(r) | Refusal::Failed(r) => r,
        }
    }
}

pub fn evict(target: &Target, evidence: &[Evidence], writers: &[String]) -> Result<(), Refusal> {
    let path = &target.path;
    let meta = fs::symlink_metadata(path)
        .map_err(|e| Refusal::Skipped(format!("no longer readable: {e}")))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Refusal::Skipped("no longer a directory".into()));
    }
    if !is_cargo_build_dir(path) {
        return Err(Refusal::Skipped(
            "no longer carries Cargo's CACHEDIR.TAG".into(),
        ));
    }
    if last_activity(path) > target.last_activity {
        return Err(Refusal::Skipped("built since the scan".into()));
    }
    if let Some(e) = owner(target, evidence, writers) {
        return Err(Refusal::Skipped(format!(
            "in use by pid {} ({})",
            e.pid, e.command
        )));
    }

    let locks = hold_locks(path).map_err(|lock| {
        Refusal::Skipped(format!("build running (lock held: {})", lock.display()))
    })?;
    let trash = trash_path(path);
    let renamed = fs::rename(path, &trash);
    drop(locks);
    renamed.map_err(|e| Refusal::Failed(format!("rename aside failed: {e}")))?;

    fs::remove_dir_all(&trash).map_err(|e| {
        Refusal::Failed(format!(
            "renamed to {} but the delete failed ({e}); the next run finishes it",
            trash.display()
        ))
    })
}

/// Finish deleting a build directory an interrupted run renamed aside.
pub fn remove_trash(trash: &Path) -> Result<(), String> {
    let is_ours = trash
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with(TRASH_PREFIX));
    if !is_ours || !is_cargo_build_dir(trash) {
        return Err(format!(
            "{} is not a target-gc trash directory",
            trash.display()
        ));
    }
    fs::remove_dir_all(trash).map_err(|e| format!("{}: {e}", trash.display()))
}

fn trash_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "target".into(), |n| n.to_string_lossy().into_owned());
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = format!("{TRASH_PREFIX}{name}.{secs}.{}", std::process::id());
    path.with_file_name(aside)
}
