//! What may go, and in what order. Pure decisions, no disk.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use target_gc::plan::{keep_evicting, plan, Candidate, Policy};
use target_gc::{Hold, Target};

const DAY: Duration = Duration::from_secs(86_400);
const GIB: u64 = 1024 * 1024 * 1024;

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
}

fn cand(name: &str, idle_days: u64, hold: Option<Hold>) -> Candidate {
    Candidate {
        target: Target {
            path: PathBuf::from(format!("/p/{name}/target")),
            project: PathBuf::from(format!("/p/{name}")),
            last_activity: now() - DAY * u32::try_from(idle_days).expect("small"),
        },
        hold,
    }
}

fn policy() -> Policy {
    Policy {
        keep_free: 50 * GIB,
        target_free: 100 * GIB,
        min_idle: 2 * DAY,
        max_idle: Some(30 * DAY),
    }
}

fn names(ts: &[Target]) -> Vec<String> {
    ts.iter()
        .map(|t| {
            t.project
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn a_held_directory_is_never_planned_however_old() {
    let p = plan(
        vec![cand(
            "svc",
            400,
            Some(Hold::Referenced(PathBuf::from("/Library/x.plist"))),
        )],
        &policy(),
        GIB,
        now(),
    );
    assert!(p.expired.is_empty() && p.queue.is_empty());
    assert_eq!(p.held.len(), 1);
}

#[test]
fn a_directory_built_within_min_idle_is_never_planned_even_under_pressure() {
    let p = plan(vec![cand("fresh", 1, None)], &policy(), GIB, now());
    assert!(p.pressure);
    assert!(p.queue.is_empty() && p.expired.is_empty());
    assert_eq!(p.young.len(), 1);
}

#[test]
fn expiry_applies_without_pressure_and_pressure_queues_oldest_first() {
    let p = plan(
        vec![
            cand("mid", 10, None),
            cand("ancient", 90, None),
            cand("old", 20, None),
            cand("stale", 45, None),
        ],
        &policy(),
        500 * GIB,
        now(),
    );
    assert!(!p.pressure);
    assert_eq!(names(&p.expired), ["ancient", "stale"]);
    assert_eq!(names(&p.queue), ["old", "mid"]);
}

#[test]
fn with_expiry_off_only_pressure_evicts() {
    let mut pol = policy();
    pol.max_idle = None;
    let p = plan(vec![cand("ancient", 900, None)], &pol, 500 * GIB, now());
    assert!(p.expired.is_empty());
    assert_eq!(p.queue.len(), 1);
}

/// Enter below `keep_free`, continue until `target_free`: one run does the
/// whole job instead of trimming one directory per hour.
#[test]
fn pressure_has_hysteresis() {
    let pol = policy();
    assert!(keep_evicting(&pol, true, 40 * GIB));
    assert!(
        keep_evicting(&pol, true, 70 * GIB),
        "stopped between the two levels"
    );
    assert!(!keep_evicting(&pol, true, 100 * GIB));
    assert!(
        !keep_evicting(&pol, false, 40 * GIB),
        "evicting without pressure"
    );
}
