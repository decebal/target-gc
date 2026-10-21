//! What may go, and in what order. Pure decisions, no disk.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use target_gc::plan::{keep_evicting, plan, Candidate, Policy};
use target_gc::{Hold, Target};

const HOUR: Duration = Duration::from_secs(3_600);
const DAY: Duration = Duration::from_secs(86_400);
const GIB: u64 = 1024 * 1024 * 1024;

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
}

fn cand(name: &str, idle_days: u64, hold: Option<Hold>) -> Candidate {
    cand_idle(name, DAY * u32::try_from(idle_days).expect("small"), hold)
}

fn cand_idle(name: &str, idle: Duration, hold: Option<Hold>) -> Candidate {
    Candidate {
        target: Target {
            path: PathBuf::from(format!("/p/{name}/target")),
            project: PathBuf::from(format!("/p/{name}")),
            last_activity: now() - idle,
        },
        hold,
        occupied: false,
    }
}

fn policy() -> Policy {
    Policy {
        keep_free: 50 * GIB,
        target_free: 100 * GIB,
        min_idle: 2 * DAY,
        pressure_min_idle: HOUR,
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
fn a_directory_built_within_the_pressure_floor_is_never_planned() {
    let p = plan(
        vec![cand_idle("building", Duration::from_secs(30 * 60), None)],
        &policy(),
        GIB,
        now(),
    );
    assert!(p.pressure);
    assert!(p.queue.is_empty() && p.expired.is_empty());
    assert_eq!(p.young.len(), 1);
}

/// 2026-10-21: 45 GiB free under a 93 GiB floor, and every directory was a
/// worktree built hours ago. With `min_idle` as the only floor the run
/// evicted nothing and exited 1.
#[test]
fn under_pressure_a_directory_idle_past_the_pressure_floor_is_queued() {
    let candidates = || {
        vec![
            cand_idle("hours", 5 * HOUR, None),
            cand_idle("yesterday", 23 * HOUR, None),
            cand_idle("minutes", Duration::from_secs(10 * 60), None),
        ]
    };
    let p = plan(candidates(), &policy(), GIB, now());
    assert!(p.pressure);
    assert_eq!(names(&p.queue), ["yesterday", "hours"]);
    assert_eq!(names(&p.young), ["minutes"]);

    let calm = plan(candidates(), &policy(), 500 * GIB, now());
    assert!(!calm.pressure);
    assert!(
        calm.queue.is_empty(),
        "the pressure floor applied without pressure"
    );
    assert_eq!(calm.young.len(), 3);
}

/// A shell or agent session in the project may run `./target/release/x`
/// between builds; nothing in the snapshot holds the directory between runs.
#[test]
fn under_pressure_an_occupied_project_keeps_min_idle() {
    let mut sat_in = cand_idle("sat-in", 5 * HOUR, None);
    sat_in.occupied = true;
    let mut long_gone = cand_idle("long-gone", 3 * DAY, None);
    long_gone.occupied = true;
    let p = plan(vec![sat_in, long_gone], &policy(), GIB, now());
    assert_eq!(names(&p.young), ["sat-in"]);
    assert_eq!(names(&p.queue), ["long-gone"]);
}

/// Queued directories are not all evicted: the run stops at `target_free`,
/// skips other volumes and refusals. Those built within `min_idle` keep their
/// trim and scratch sweep.
#[test]
fn recent_spans_young_and_queued_within_min_idle() {
    let p = plan(
        vec![
            cand_idle("minutes", Duration::from_secs(10 * 60), None),
            cand_idle("hours", 5 * HOUR, None),
            cand_idle("old", 10 * DAY, None),
            cand_idle(
                "svc",
                5 * HOUR,
                Some(Hold::Referenced(PathBuf::from("/Library/x.plist"))),
            ),
        ],
        &policy(),
        GIB,
        now(),
    );
    assert_eq!(names(&p.queue), ["old", "hours"]);
    assert_eq!(names(&p.recent), ["minutes", "hours"]);
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
