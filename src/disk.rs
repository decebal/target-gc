//! Free space, read the way `df` reports it.
//!
//! `df` is the number to trust on a copy-on-write filesystem: a build
//! directory seeded with `cp -c` shares most of its blocks, so summing file
//! sizes overstates what deleting it frees. Measured once: 50 G by `du`, 2 GiB
//! back. So the eviction loop asks `df` after every deletion instead of
//! subtracting an estimate.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::bounded;

const DF: &str = "/bin/df";
const DF_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    pub available: u64,
}

/// A free-space level: absolute bytes, or a share of the volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Bytes(u64),
    Percent(u8),
}

impl Level {
    pub fn resolve(self, total: u64) -> u64 {
        match self {
            Level::Bytes(b) => b,
            Level::Percent(p) => total / 100 * u64::from(p),
        }
    }

    /// `50G`, `50GiB`, `512M`, `1T`, `10%`. Units are binary; a bare number is
    /// bytes.
    pub fn parse(raw: &str) -> Option<Level> {
        let raw = raw.trim();
        if let Some(pct) = raw.strip_suffix('%') {
            let p: u8 = pct.trim().parse().ok()?;
            return (p <= 100).then_some(Level::Percent(p));
        }
        let split = raw
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(raw.len());
        let (num, unit) = raw.split_at(split);
        let num: f64 = num.parse().ok()?;
        let mult: f64 = match unit.trim().to_ascii_uppercase().as_str() {
            "" | "B" => 1.0,
            "K" | "KB" | "KIB" => 1024.0,
            "M" | "MB" | "MIB" => 1024.0 * 1024.0,
            "G" | "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
            "T" | "TB" | "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
            _ => return None,
        };
        Some(Level::Bytes((num * mult) as u64))
    }
}

/// Parse `df -Pk <path>` output. The capacity column is the one token ending
/// in `%`; available and total are counted back from it, because the
/// filesystem name before them and the mount point after them can both
/// contain spaces.
pub fn parse_df(output: &str) -> Option<Space> {
    let line = output.lines().nth(1)?;
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let cap = tokens.iter().position(|t| t.ends_with('%'))?;
    let available: u64 = tokens.get(cap.checked_sub(1)?)?.parse().ok()?;
    let total: u64 = tokens.get(cap.checked_sub(3)?)?.parse().ok()?;
    Some(Space {
        total: total * 1024,
        available: available * 1024,
    })
}

pub fn space(path: &Path) -> Option<Space> {
    let mut cmd = Command::new(DF);
    cmd.arg("-Pk").arg(path);
    let out = bounded::run(cmd, DF_TIMEOUT)?;
    if !out.success {
        return None;
    }
    parse_df(&out.stdout)
}
