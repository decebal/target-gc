//! Evicting idle Cargo build directories when the disk needs the space.
//!
//! Three properties this exists to hold, each pinned by a test:
//!
//! 1. **A directory is a Cargo build directory only if Cargo tagged it.** Cargo
//!    writes `CACHEDIR.TAG` into every build directory it creates. A directory
//!    that is merely NAMED `target` is somebody's source — one JavaScript
//!    package ships its runtime shims in a `target/` folder — and is never
//!    touched. See [`discover`].
//! 2. **A build directory in use is never evicted.** Cargo holds a lock per
//!    profile (`<target>/[<triple>/]<profile>/.cargo-lock`, and on newer Cargo
//!    `.cargo-build-lock` / `.cargo-artifact-lock`). A process whose executable
//!    lives in the build directory also counts, and so does a known build
//!    writer (`dx`, `bacon`, `cargo`, …) whose working directory is inside the
//!    project. Missing evidence fails closed. See [`liveness`].
//! 3. **What was freed is measured, not estimated.** Directories seeded as
//!    copy-on-write clones share blocks, so their apparent size overstates what
//!    deleting them returns — one reported 50 G and freed 2 GiB. The eviction
//!    loop re-reads free space after every deletion and stops at the target.
//!    See [`disk`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub mod bounded;
pub mod config;
pub mod discover;
pub mod disk;
pub mod evict;
pub mod install;
pub mod liveness;
pub mod plan;
pub mod protect;
pub mod scratch;
pub mod skill;
pub mod state;
pub mod toml_subset;

pub const DAY: Duration = Duration::from_secs(86_400);

/// One Cargo build directory found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The build directory itself.
    pub path: PathBuf,
    /// Where a process working on this project would have its cwd. For the
    /// ordinary layout that is the directory holding `target/`.
    pub project: PathBuf,
    /// Newest modification among the directories Cargo rewrites when it
    /// compiles. See [`discover::last_activity`].
    pub last_activity: SystemTime,
}

impl Target {
    pub fn idle(&self, now: SystemTime) -> Duration {
        now.duration_since(self.last_activity)
            .unwrap_or(Duration::ZERO)
    }
}

/// Why a build directory is off-limits for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// A Cargo lock is held: a build is running right now.
    Locked(PathBuf),
    /// A running build writer has its cwd in the project, or a process has its
    /// executable in the build directory.
    InUse { pid: u32, command: String },
    /// A service definition or config file points at a binary inside it.
    Referenced(PathBuf),
    /// Configured as protected.
    Protected(PathBuf),
}

impl Hold {
    pub fn describe(&self) -> String {
        match self {
            Hold::Locked(lock) => format!("build running (lock held: {})", lock.display()),
            Hold::InUse { pid, command } => format!("in use by pid {pid} ({command})"),
            Hold::Referenced(file) => format!("binary referenced by {}", file.display()),
            Hold::Protected(prefix) => format!("under protected path {}", prefix.display()),
        }
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MiB", b / MIB)
    } else {
        format!("{bytes} B")
    }
}

pub fn human_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs / 60)
    }
}

/// `true` when `path` is `prefix` or lies beneath it, component-wise, so
/// `/a/target-old` is not "under" `/a/target`.
pub fn is_under(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}
