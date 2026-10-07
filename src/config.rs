//! Machine-level settings, read with the workspace's `gates-config` reader.
//!
//! ```toml
//! [target-gc]
//! roots         = ["~"]
//! keep_free     = "10%"    # below this, pressure eviction starts
//! target_free   = "20%"    # ...and continues until this
//! min_idle_days = "2"      # never evict anything built more recently
//! max_idle_days = "30"     # evict past this whatever the free space; "0" = never
//! protect       = []       # paths never touched
//! scan_files    = [...]    # files that may name a binary inside a build dir
//! writers       = []       # extra process names that write into target/
//! ```
//!
//! Numbers are quoted: the reader supports strings and lists only, on purpose
//! (see [`crate::toml_subset`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::disk::Level;
use crate::liveness::DEFAULT_WRITERS;
use crate::toml_subset::Config;
use crate::DAY;

const TABLE: &str = "target-gc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub roots: Vec<PathBuf>,
    pub skip: Vec<PathBuf>,
    pub keep_free: Level,
    pub target_free: Level,
    pub min_idle: Duration,
    pub max_idle: Option<Duration>,
    pub protect: Vec<PathBuf>,
    pub scan_files: Vec<PathBuf>,
    /// Process names whose cwd in a project holds its build directory.
    pub writers: Vec<String>,
    pub max_depth: usize,
    pub deadline: Duration,
}

/// `None` when `HOME` is unset or not absolute. Callers stop rather than fall
/// back to `/`, which would make the whole filesystem the scan root.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.parent().is_some())
}

pub fn expand(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        return home.to_path_buf();
    }
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(raw),
    }
}

/// `$TARGET_GC_CONFIG`, else `$XDG_CONFIG_HOME/target-gc/config.toml`, else
/// `~/.config/target-gc/config.toml`.
pub fn default_path(home: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("TARGET_GC_CONFIG") {
        return PathBuf::from(p);
    }
    let base =
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    base.join("target-gc/config.toml")
}

/// `$XDG_STATE_HOME/target-gc`, else `~/.local/state/target-gc`.
pub fn state_dir(home: &Path) -> PathBuf {
    let base =
        std::env::var_os("XDG_STATE_HOME").map_or_else(|| home.join(".local/state"), PathBuf::from);
    base.join("target-gc")
}

impl Settings {
    pub fn defaults(home: &Path) -> Settings {
        let paths = |raw: &[&str]| raw.iter().map(|r| expand(r, home)).collect::<Vec<_>>();
        Settings {
            roots: vec![home.to_path_buf()],
            skip: paths(&["~/Library", "~/Pictures", "~/Music", "~/Movies"]),
            keep_free: Level::Percent(10),
            target_free: Level::Percent(20),
            min_idle: 2 * DAY,
            max_idle: Some(30 * DAY),
            protect: Vec::new(),
            scan_files: paths(&[
                "~/.claude.json",
                "~/.claude/settings.json",
                "~/.codex/config.toml",
                "~/.zshrc",
                "~/.bashrc",
                "~/.profile",
                "~/.config/fish/config.fish",
            ]),
            writers: DEFAULT_WRITERS.iter().map(|w| (*w).to_string()).collect(),
            max_depth: 10,
            deadline: Duration::from_secs(240),
        }
    }

    /// Defaults overlaid with the file at `path`. A missing file means
    /// defaults; a malformed one, or a value that does not parse, is an error
    /// rather than a silently ignored line.
    pub fn load(path: &Path, home: &Path) -> Result<Settings, String> {
        let config = Config::read(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .unwrap_or_default();
        Settings::from_config(&config, home)
    }

    pub fn from_config(config: &Config, home: &Path) -> Result<Settings, String> {
        let mut s = Settings::defaults(home);
        let key = |k: &str| format!("{TABLE}.{k}");
        let paths = |list: Vec<String>| list.iter().map(|r| expand(r, home)).collect::<Vec<_>>();

        if let Some(v) = config.list(&key("roots")) {
            s.roots = paths(v);
        }
        if let Some(v) = config.list(&key("skip")) {
            s.skip = paths(v);
        }
        if let Some(v) = config.list(&key("protect")) {
            s.protect = paths(v);
        }
        if let Some(v) = config.list(&key("scan_files")) {
            s.scan_files = paths(v);
        }
        if let Some(v) = config.list(&key("writers")) {
            for w in v {
                if !s.writers.contains(&w) {
                    s.writers.push(w);
                }
            }
        }
        if let Some(v) = config.string(&key("keep_free")) {
            s.keep_free = Level::parse(v).ok_or(format!("keep_free: cannot read {v:?}"))?;
        }
        if let Some(v) = config.string(&key("target_free")) {
            s.target_free = Level::parse(v).ok_or(format!("target_free: cannot read {v:?}"))?;
        }
        if let Some(v) = config.string(&key("min_idle_days")) {
            s.min_idle = days(v, "min_idle_days")?;
        }
        if let Some(v) = config.string(&key("max_idle_days")) {
            let d = days(v, "max_idle_days")?;
            s.max_idle = (!d.is_zero()).then_some(d);
        }
        if let Some(v) = config.string(&key("max_depth")) {
            s.max_depth = v
                .parse()
                .map_err(|_| format!("max_depth: cannot read {v:?}"))?;
        }
        if let Some(v) = config.string(&key("deadline_secs")) {
            let secs: u64 = v
                .parse()
                .map_err(|_| format!("deadline_secs: cannot read {v:?}"))?;
            s.deadline = Duration::from_secs(secs.min(270));
        }
        s.validate()?;
        Ok(s)
    }

    /// A directory built in the last day may be between two Cargo invocations
    /// of a running script, with no lock held at the instant we look. One day
    /// is the floor, like `target-sweep`'s `--age`.
    pub fn validate(&self) -> Result<(), String> {
        if self.min_idle < DAY {
            return Err("min_idle_days must be at least 1".into());
        }
        if self.max_idle.is_some_and(|max| max < self.min_idle) {
            return Err("max_idle_days must not be below min_idle_days".into());
        }
        if self.roots.is_empty() {
            return Err("roots is empty: nothing would be scanned".into());
        }
        Ok(())
    }
}

fn days(raw: &str, name: &str) -> Result<Duration, String> {
    let n: u64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("{name}: cannot read {raw:?}"))?;
    Ok(DAY * u32::try_from(n).map_err(|_| format!("{name}: {raw} is too large"))?)
}
