//! Removing `node_modules` from projects nobody has worked in for a long time.
//!
//! A `node_modules` is rebuilt from the project's manifest and lockfile by one
//! install, so unlike a build directory it goes on age alone, without disk
//! pressure. It still has to be provably unused:
//!
//! - it sits next to a `package.json`, and no directory between the scan root
//!   and it starts with `.` (editor extensions, version managers and package
//!   caches keep the `node_modules` their application runs from there);
//! - nothing in the project's git checkout changed for `node_modules_idle_days`;
//! - no process has its cwd in that checkout or runs an executable from inside
//!   the `node_modules`;
//! - no service definition or scanned config file names the project, which is
//!   how an MCP server or a launchd agent running the project's code shows up.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::evict::Refusal;
use crate::liveness::{occupant, session_scope, Evidence, Kind};
use crate::protect::{protection, referenced_by, Source};
use crate::{is_under, Hold, Target};

pub const DIR_NAME: &str = "node_modules";

/// Prefix of a `node_modules` renamed aside by a run that was interrupted
/// before the delete finished.
pub const TRASH_PREFIX: &str = ".target-gc-node-trash.";

/// `true` for a `node_modules` that belongs to a project: a `package.json`
/// beside it and no dot-directory between its scan root and it.
pub fn is_project_deps(path: &Path, roots: &[PathBuf]) -> bool {
    if path.file_name().is_none_or(|n| n != DIR_NAME) {
        return false;
    }
    let Some(project) = path.parent() else {
        return false;
    };
    if !project.join("package.json").is_file() {
        return false;
    }
    let Some(rest) = roots
        .iter()
        .filter_map(|r| path.strip_prefix(r).ok())
        .min_by_key(|rest| rest.components().count())
    else {
        return false;
    };
    !rest
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
}

/// A `node_modules` with the activity of the checkout it belongs to.
pub fn at(path: PathBuf) -> Target {
    let project = path
        .parent()
        .map_or_else(|| path.clone(), Path::to_path_buf);
    let last_activity = last_activity(&project);
    Target {
        path,
        project,
        last_activity,
    }
}

/// Newest mtime in the project's git checkout (or the project, outside one)
/// and in the project itself: each directory, its children and grandchildren,
/// never descending into a `node_modules`. Saving a file through an editor's
/// rename moves its directory's mtime; an install moves the lockfile's.
pub fn last_activity(project: &Path) -> SystemTime {
    let scope = session_scope(project);
    let mut newest = newest_within(&scope, 2);
    if scope != project {
        newest = newest.max(newest_within(project, 2));
    }
    newest
}

fn newest_within(dir: &Path, depth: usize) -> SystemTime {
    let mut newest = mtime(dir);
    if depth == 0 {
        return newest;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return newest;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == DIR_NAME || name.starts_with(TRASH_PREFIX) {
            continue;
        }
        let path = entry.path();
        newest = newest.max(mtime(&path));
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            newest = newest.max(newest_within(&path, depth - 1));
        }
    }
    newest
}

fn mtime(path: &Path) -> SystemTime {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Why this `node_modules` must stay, if anything says so.
pub fn hold(
    deps: &Target,
    home: &Path,
    prefixes: &[PathBuf],
    sources: &[Source],
    evidence: &[Evidence],
) -> Option<Hold> {
    if let Some(h) = protection(deps, home, prefixes, sources) {
        return Some(h);
    }
    if let Some(file) = referenced_by(&deps.project, home, sources) {
        return Some(Hold::Referenced(file.to_path_buf()));
    }
    evidence
        .iter()
        .find(|e| e.kind == Kind::Exe && is_under(&e.path, &deps.path))
        .or_else(|| occupant(deps, evidence))
        .map(|e| Hold::InUse {
            pid: e.pid,
            command: e.command.clone(),
        })
}

/// `node_modules` idle past `idle` and held by nothing, oldest first.
pub fn expired(found: Vec<(Target, Option<Hold>)>, idle: Duration, now: SystemTime) -> Vec<Target> {
    let mut out: Vec<Target> = found
        .into_iter()
        .filter(|(d, h)| h.is_none() && d.idle(now) >= idle)
        .map(|(d, _)| d)
        .collect();
    out.sort_by_key(|d| d.last_activity);
    out
}

/// Re-verify from disk, rename aside, delete. The caller re-checks [`hold`]
/// against a fresh process snapshot first.
pub fn remove(deps: &Target) -> Result<(), Refusal> {
    let path = &deps.path;
    let meta = fs::symlink_metadata(path)
        .map_err(|e| Refusal::Skipped(format!("no longer readable: {e}")))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Refusal::Skipped("no longer a directory".into()));
    }
    if !deps.project.join("package.json").is_file() {
        return Err(Refusal::Skipped("package.json is gone".into()));
    }
    if last_activity(&deps.project) > deps.last_activity {
        return Err(Refusal::Skipped(
            "the project changed since the scan".into(),
        ));
    }
    let project_mtime = mtime(&deps.project);
    let trash = trash_path(path);
    fs::rename(path, &trash).map_err(|e| Refusal::Failed(format!("rename aside failed: {e}")))?;
    let removed = remove_tree(&trash).map_err(|e| {
        Refusal::Failed(format!(
            "renamed to {} but the delete failed ({e}); the next run finishes it",
            trash.display()
        ))
    });
    // The rename moved the project's mtime. Left there, it would read as
    // activity and keep every sibling package in a monorepo for another
    // idle period.
    let _ = fs::File::open(&deps.project).and_then(|f| f.set_modified(project_mtime));
    removed
}

/// Finish deleting a `node_modules` an interrupted run renamed aside.
pub fn remove_trash(trash: &Path) -> Result<(), String> {
    let is_ours = trash
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with(TRASH_PREFIX));
    if !is_ours {
        return Err(format!(
            "{} is not a target-gc trash directory",
            trash.display()
        ));
    }
    remove_tree(trash).map_err(|e| format!("{}: {e}", trash.display()))
}

/// Packages ship read-only directories, which `remove_dir_all` cannot empty.
/// On failure every real directory (never a symlink's target) is made
/// writable by its owner and the delete is tried once more.
fn remove_tree(dir: &Path) -> std::io::Result<()> {
    if fs::remove_dir_all(dir).is_ok() {
        return Ok(());
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&d) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o700);
        let _ = fs::set_permissions(&d, perms);
        if let Ok(entries) = fs::read_dir(&d) {
            stack.extend(entries.flatten().map(|e| e.path()));
        }
    }
    fs::remove_dir_all(dir)
}

fn trash_path(path: &Path) -> PathBuf {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    path.with_file_name(format!("{TRASH_PREFIX}{secs}.{}", std::process::id()))
}
