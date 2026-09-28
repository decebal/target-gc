//! Build directories that must survive even when nothing is running from them.
//!
//! A service can execute a binary straight out of a build directory — a
//! launchd agent pointing at `…/target/release/<bin>`. While it runs, the
//! process evidence in [`crate::liveness`] holds the directory. The day it is
//! stopped, crashed or between restarts, every signal says "idle", and a
//! deletion then kills the service at its next start with nothing to rebuild
//! it. So service definitions and a configured list of files are read, and a
//! build directory whose path appears in any of them is protected.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{is_under, Hold, Target};

/// Directories whose files are service definitions, per platform. Missing
/// directories are skipped.
pub fn service_dirs(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchDaemons"),
        home.join(".config/systemd/user"),
        PathBuf::from("/etc/systemd/system"),
        PathBuf::from("/etc/systemd/user"),
    ]
}

/// A file that may name a binary, with its bytes read once.
pub struct Source {
    pub path: PathBuf,
    pub text: String,
}

/// Read every regular file in `dirs` plus every file in `files`. Binary plists
/// store paths as plain bytes, so a lossy read still finds them.
pub fn load_sources(dirs: &[PathBuf], files: &[PathBuf]) -> Vec<Source> {
    let mut paths: Vec<PathBuf> = files.to_vec();
    for dir in dirs {
        if let Ok(entries) = fs::read_dir(dir) {
            paths.extend(
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .map(|e| e.path()),
            );
        }
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let bytes = fs::read(&path).ok()?;
            Some(Source {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                path,
            })
        })
        .collect()
}

/// The first source that names a path inside this build directory, written
/// out in full or with `~` / `$HOME` for the home directory.
pub fn referenced_by<'a>(target: &Path, home: &Path, sources: &'a [Source]) -> Option<&'a Path> {
    let full = format!("{}/", target.display());
    let mut spellings = vec![full];
    if let Ok(rest) = target.strip_prefix(home) {
        spellings.push(format!("~/{}/", rest.display()));
        spellings.push(format!("$HOME/{}/", rest.display()));
    }
    sources
        .iter()
        .find(|s| spellings.iter().any(|sp| s.text.contains(sp.as_str())))
        .map(|s| s.path.as_path())
}

/// Why this build directory is protected, if it is.
pub fn protection(
    target: &Target,
    home: &Path,
    prefixes: &[PathBuf],
    sources: &[Source],
) -> Option<Hold> {
    if let Some(prefix) = prefixes.iter().find(|p| is_under(&target.path, p)) {
        return Some(Hold::Protected(prefix.clone()));
    }
    referenced_by(&target.path, home, sources).map(|file| Hold::Referenced(file.to_path_buf()))
}
