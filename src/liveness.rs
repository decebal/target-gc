//! Is anything using this build directory right now?
//!
//! Two independent kinds of evidence, and either one holds a directory:
//!
//! - **Cargo's own locks.** Cargo locks every profile it builds, at
//!   `<target>/[<triple>/]<profile>/.cargo-lock` (plus `.cargo-build-lock` and
//!   `.cargo-artifact-lock` on newer Cargo). Nothing ever creates
//!   `<target>/.cargo-lock`; a cleaner that tests that path sees "no build" on
//!   every run. A non-blocking exclusive `flock` attempt that fails means a
//!   build holds the profile.
//! - **Processes.** A process whose executable lives in the build directory (a
//!   service running a `target/release` binary), or a known build writer whose
//!   cwd is inside the project (a dev server such as `dx serve` recreates
//!   `target/` within seconds of a delete). Any other process with its cwd in
//!   the project holds nothing: an agent session or a service sitting in the
//!   repo root pinned 225 GiB for days on 2026-10-07 without writing a byte,
//!   and `min_idle` already protects a directory that is actually being built.
//!
//! A process snapshot that cannot be taken is not "nobody is using it": the
//! run refuses to evict anything.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::discover::{profile_dirs, LOCK_FILES};
use crate::{bounded, is_under, Hold, Target};

const LSOF: &str = "/usr/sbin/lsof";
const LSOF_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cwd,
    Exe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub pid: u32,
    pub command: String,
    pub kind: Kind,
    pub path: PathBuf,
}

/// Every Cargo lock file present in this build directory.
pub fn lock_files(target: &Path) -> Vec<PathBuf> {
    profile_dirs(target)
        .into_iter()
        .flat_map(|profile| LOCK_FILES.iter().map(move |name| profile.join(name)))
        .filter(|p| p.is_file())
        .collect()
}

/// The first Cargo lock another process holds, if any. Each lock is taken
/// and released immediately; a Cargo that starts in that instant waits a few
/// microseconds and carries on.
pub fn held_lock(target: &Path) -> Option<PathBuf> {
    lock_files(target).into_iter().find(|lock| {
        let Ok(file) = File::open(lock) else {
            return false;
        };
        file.try_lock().is_err()
    })
}

/// Take every Cargo lock in the build directory and keep them. While the
/// returned files are alive no Cargo can start building here, which is what
/// makes the rename that follows safe. `Err` names the lock that was busy.
pub fn hold_locks(target: &Path) -> Result<Vec<File>, PathBuf> {
    let mut held = Vec::new();
    for lock in lock_files(target) {
        let file = File::open(&lock).map_err(|_| lock.clone())?;
        if file.try_lock().is_err() {
            return Err(lock);
        }
        held.push(file);
    }
    Ok(held)
}

/// Processes that write into `target/` while running, so deleting it under them
/// frees nothing for long. Extended by the `writers` config key.
pub const DEFAULT_WRITERS: &[&str] = &[
    "cargo",
    "cargo-watch",
    "cargo-leptos",
    "bacon",
    "dx",
    "trunk",
    "watchexec",
    "rust-analyzer",
];

/// Linux `comm` is cut at 15 bytes, so a truncated name matches the writer it
/// is a prefix of.
fn is_writer(command: &str, writers: &[String]) -> bool {
    writers
        .iter()
        .any(|w| w == command || (command.len() == 15 && w.starts_with(command)))
}

/// The process evidence that holds this build directory, if any.
pub fn owner<'a>(
    target: &Target,
    evidence: &'a [Evidence],
    writers: &[String],
) -> Option<&'a Evidence> {
    evidence.iter().find(|e| match e.kind {
        Kind::Cwd => is_under(&e.path, &target.project) && is_writer(&e.command, writers),
        Kind::Exe => is_under(&e.path, &target.path),
    })
}

/// Everything that holds this build directory right now, locks first.
pub fn hold(target: &Target, evidence: &[Evidence], writers: &[String]) -> Option<Hold> {
    if let Some(lock) = held_lock(&target.path) {
        return Some(Hold::Locked(lock));
    }
    owner(target, evidence, writers).map(|e| Hold::InUse {
        pid: e.pid,
        command: e.command.clone(),
    })
}

/// A snapshot of every process's cwd and executable. `None` when it could not
/// be taken, which callers treat as "cannot prove anything is free".
pub fn snapshot() -> Option<Vec<Evidence>> {
    let me = std::process::id();
    let evidence = if Path::new("/proc/self/cwd").exists() {
        snapshot_proc(Path::new("/proc"))
    } else {
        let mut cmd = Command::new(LSOF);
        // Run from `/` so lsof's own cwd never holds the project it was
        // started from.
        cmd.current_dir("/")
            .args(["-w", "-n", "-P", "-F", "pcfn", "-d", "cwd,txt"]);
        let out = bounded::run(cmd, LSOF_TIMEOUT)?;
        parse_lsof(&out.stdout)
    };
    let evidence: Vec<Evidence> = evidence.into_iter().filter(|e| e.pid != me).collect();
    // lsof exits non-zero when it could not inspect every process, which is
    // routine for other users' processes. An EMPTY listing is the failure.
    (!evidence.is_empty()).then_some(evidence)
}

/// Parse `lsof -F pcfn` output: `p<pid>`, `c<command>`, `f<fd>`, `n<path>`
/// lines, where each `n` belongs to the `f` before it and each `f` to the `p`.
pub fn parse_lsof(output: &str) -> Vec<Evidence> {
    let mut out = Vec::new();
    let mut pid = 0u32;
    let mut command = String::new();
    let mut kind = None;
    for line in output.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = value.parse().unwrap_or(0);
                command.clear();
                kind = None;
            }
            'c' => command = value.to_string(),
            'f' => {
                kind = match value {
                    "cwd" => Some(Kind::Cwd),
                    "txt" => Some(Kind::Exe),
                    _ => None,
                }
            }
            'n' => {
                if let (Some(kind), true) = (kind, value.starts_with('/')) {
                    out.push(Evidence {
                        pid,
                        command: command.clone(),
                        kind,
                        path: PathBuf::from(value),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// Linux: read `/proc/<pid>/cwd` and `/proc/<pid>/exe` for every process we
/// may inspect. Other users' processes are unreadable and skipped.
pub fn snapshot_proc(proc_root: &Path) -> Vec<Evidence> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(proc_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let dir = entry.path();
        let command = fs::read_to_string(dir.join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        for (link, kind) in [("cwd", Kind::Cwd), ("exe", Kind::Exe)] {
            if let Ok(path) = fs::read_link(dir.join(link)) {
                out.push(Evidence {
                    pid,
                    command: command.clone(),
                    kind,
                    path,
                });
            }
        }
    }
    out
}
