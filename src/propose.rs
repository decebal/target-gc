//! Build directories the policy will not evict on its own, offered to a person
//! instead of being left alone.
//!
//! A directory qualifies when nothing holds it (no lock, no build writer, no
//! binary running from it, no service naming it), no process at all has its
//! working directory in the project, and it is at least `bloat_min_size`
//! allocated. Age is not a condition: on a machine where every worktree was
//! built today, `min_idle` keeps all of them, and the person approving stands
//! in for it. [`check`] re-runs every one of those conditions at the moment of
//! the eviction, so an approval given an hour ago cannot remove a directory a
//! session has since moved into.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::discover::{is_cargo_build_dir, target_at};
use crate::liveness::{self, Evidence};
use crate::protect::{self, Source};
use crate::{human_age, human_bytes, Target};

pub const FILE: &str = "proposals.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub path: PathBuf,
    pub allocated: u64,
    pub idle_secs: u64,
}

/// At or above `bloat_min`, largest first, so the first approval frees the
/// most.
pub fn select(sized: Vec<(Target, u64)>, bloat_min: u64, now: SystemTime) -> Vec<Proposal> {
    let mut out: Vec<Proposal> = sized
        .into_iter()
        .filter(|(_, allocated)| *allocated >= bloat_min)
        .map(|(t, allocated)| Proposal {
            idle_secs: t.idle(now).as_secs(),
            path: t.path,
            allocated,
        })
        .collect();
    out.sort_by_key(|p| std::cmp::Reverse(p.allocated));
    out
}

pub fn to_json(made_at: u64, items: &[Proposal]) -> Value {
    json!({
        "made_at": made_at,
        "proposals": items.iter().map(|p| json!({
            "path": p.path,
            "allocated": p.allocated,
            "idle_secs": p.idle_secs,
        })).collect::<Vec<_>>(),
    })
}

pub fn from_json(v: &Value) -> Option<(u64, Vec<Proposal>)> {
    let items = v["proposals"]
        .as_array()?
        .iter()
        .filter_map(|p| {
            Some(Proposal {
                path: PathBuf::from(p["path"].as_str()?),
                allocated: p["allocated"].as_u64()?,
                idle_secs: p["idle_secs"].as_u64()?,
            })
        })
        .collect();
    Some((v["made_at"].as_u64()?, items))
}

pub fn describe(p: &Proposal) -> String {
    format!(
        "{:>10}  {} (built {} ago)",
        human_bytes(p.allocated),
        p.path.display(),
        human_age(std::time::Duration::from_secs(p.idle_secs))
    )
}

/// What a session should be told about waiting proposals, or `None`.
pub fn hook_line(items: &[Proposal]) -> Option<String> {
    let largest = items.first()?;
    let total: u64 = items.iter().map(|p| p.allocated).sum();
    Some(format!(
        "target-gc: {} build directories no session is using hold {} (largest {}, {}). They \
         wait for approval: ask the user, then run `target-gc evict <path>…` for the ones they \
         approve. `target-gc propose` lists them.",
        items.len(),
        human_bytes(total),
        largest.path.display(),
        human_bytes(largest.allocated),
    ))
}

/// Whether an approved eviction may go ahead right now. The approval replaces
/// the idle floor and nothing else.
pub fn check(
    path: &Path,
    home: &Path,
    protect_prefixes: &[PathBuf],
    sources: &[Source],
    evidence: &[Evidence],
    writers: &[String],
) -> Result<Target, String> {
    if !is_cargo_build_dir(path) {
        return Err("not a Cargo build directory (no CACHEDIR.TAG written by Cargo)".into());
    }
    let target = target_at(path.to_path_buf());
    if let Some(hold) = protect::protection(&target, home, protect_prefixes, sources)
        .or_else(|| liveness::hold(&target, evidence, writers))
    {
        return Err(hold.describe());
    }
    if let Some(e) = liveness::occupant(&target, evidence) {
        return Err(format!(
            "pid {} ({}) has its working directory in {}",
            e.pid,
            e.command,
            target.project.display()
        ));
    }
    Ok(target)
}
