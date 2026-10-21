//! Deciding what may go, in what order. Pure: no filesystem, no clock.
//!
//! Two reasons to evict, and they compose:
//!
//! - **Expired.** Idle longer than `max_idle`. Goes whatever the free space,
//!   because a build directory nobody has touched in a month is a rebuild
//!   nobody is waiting for, and waiting for pressure lets them pile up.
//! - **Pressure.** Free space is below `keep_free`. The least recently built
//!   directories go first, until free space reaches `target_free`. The gap
//!   between the two levels is the hysteresis: without it every run would
//!   trim one directory, cross the line, and trim again next hour.
//!
//! Neither reason reaches a directory that is held (a build running, a process
//! inside, a service pointing at it), or one built within `min_idle`. Under
//! pressure the floor drops to `pressure_min_idle` for a directory whose
//! project no process sits in: a full disk fails every build on the machine,
//! and an evicted directory nobody is near costs one rebuild. A shell or agent
//! session in the project may be running its binaries between builds, so an
//! occupied directory keeps `min_idle`.

use std::time::{Duration, SystemTime};

use crate::{Hold, Target};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub keep_free: u64,
    pub target_free: u64,
    pub min_idle: Duration,
    /// Replaces `min_idle` while free space is below `keep_free`.
    pub pressure_min_idle: Duration,
    /// `None` disables expiry: eviction happens only under pressure.
    pub max_idle: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub target: Target,
    pub hold: Option<Hold>,
    /// Some process has its cwd in the project without holding the directory.
    pub occupied: bool,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub expired: Vec<Target>,
    /// Evictable under pressure, least recently built first. The executor
    /// takes from the front until free space reaches `target_free`.
    pub queue: Vec<Target>,
    pub held: Vec<(Target, Hold)>,
    pub young: Vec<Target>,
    /// Not held and built within `min_idle`, whichever floor applied: the
    /// directories a trim or a scratch sweep may touch. Includes queued ones,
    /// which are not all evicted once free space reaches `target_free`.
    pub recent: Vec<Target>,
    pub pressure: bool,
}

pub fn plan(candidates: Vec<Candidate>, policy: &Policy, available: u64, now: SystemTime) -> Plan {
    let mut out = Plan {
        pressure: available < policy.keep_free,
        ..Plan::default()
    };
    for c in candidates {
        if let Some(hold) = c.hold {
            out.held.push((c.target, hold));
            continue;
        }
        let idle = c.target.idle(now);
        let floor = if out.pressure && !c.occupied {
            policy.pressure_min_idle
        } else {
            policy.min_idle
        };
        if idle < policy.min_idle {
            out.recent.push(c.target.clone());
        }
        if idle < floor {
            out.young.push(c.target);
        } else if policy.max_idle.is_some_and(|max| idle >= max) {
            out.expired.push(c.target);
        } else {
            out.queue.push(c.target);
        }
    }
    out.expired.sort_by_key(|t| t.last_activity);
    out.queue.sort_by_key(|t| t.last_activity);
    out
}

/// Whether the executor should take the next queued directory.
pub fn keep_evicting(policy: &Policy, pressure: bool, available: u64) -> bool {
    pressure && available < policy.target_free
}
