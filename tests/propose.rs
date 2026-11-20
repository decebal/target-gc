//! Proposals: unused build directories offered for an approved eviction, and
//! the checks an approval still has to pass.

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{cargo_target, scratch};
use target_gc::config::Settings;
use target_gc::liveness::{Evidence, Kind};
use target_gc::propose::{check, from_json, hook_line, select, to_json, Proposal};
use target_gc::protect::Source;
use target_gc::toml_subset::Config;
use target_gc::Target;

const GIB: u64 = 1024 * 1024 * 1024;

fn target(name: &str) -> Target {
    Target {
        path: PathBuf::from(format!("/p/{name}/target")),
        project: PathBuf::from(format!("/p/{name}")),
        last_activity: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
    }
}

fn cwd(pid: u32, command: &str, path: &Path) -> Evidence {
    Evidence {
        pid,
        command: command.into(),
        kind: Kind::Cwd,
        path: path.to_path_buf(),
    }
}

#[test]
fn only_directories_over_the_size_are_proposed_largest_first() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(4_600);
    let items = select(
        vec![
            (target("small"), GIB),
            (target("big"), 14 * GIB),
            (target("mid"), 6 * GIB),
        ],
        5 * GIB,
        now,
    );
    let paths: Vec<_> = items.iter().map(|p| p.path.clone()).collect();
    assert_eq!(
        paths,
        [
            PathBuf::from("/p/big/target"),
            PathBuf::from("/p/mid/target")
        ]
    );
    assert_eq!(items[0].idle_secs, 3_600);
}

#[test]
fn proposals_round_trip_through_json() {
    let items = vec![Proposal {
        path: PathBuf::from("/p/a/target"),
        allocated: 7 * GIB,
        idle_secs: 120,
    }];
    assert_eq!(from_json(&to_json(42, &items)), Some((42, items)));
}

#[test]
fn the_hook_asks_for_approval_only_when_something_waits() {
    assert_eq!(hook_line(&[]), None);
    let line = hook_line(&[
        Proposal {
            path: PathBuf::from("/p/a/target"),
            allocated: 12 * GIB,
            idle_secs: 60,
        },
        Proposal {
            path: PathBuf::from("/p/b/target"),
            allocated: 6 * GIB,
            idle_secs: 60,
        },
    ])
    .expect("two waiting");
    assert!(line.contains("2 build directories"), "{line}");
    assert!(line.contains("18.0 GiB"), "{line}");
    assert!(line.contains("ask the user"), "{line}");
    assert!(line.contains("target-gc evict"), "{line}");
}

/// The approval replaces the idle floor: a directory built seconds ago that
/// nothing uses may go.
#[test]
fn an_approval_passes_for_a_fresh_directory_nothing_uses() {
    let root = scratch("propose-fresh");
    let t = cargo_target(&root.join("app/target"));
    let checked = check(&t, Path::new("/nohome"), &[], &[], &[], &[]).expect("approved");
    assert_eq!(checked.project, root.join("app"));
}

/// A worktree's `.git` is a file; a gate runner at the worktree root uses a
/// nested crate's build directory without entering the crate.
#[test]
fn a_process_at_the_checkout_root_occupies_a_nested_crate() {
    let root = scratch("propose-checkout");
    std::fs::write(root.join(".git"), "gitdir: /elsewhere\n").expect("write .git");
    let t = cargo_target(&root.join("control-plane/target"));
    let evidence = [cwd(95, "gate-coordinator", &root)];
    let why = check(&t, Path::new("/nohome"), &[], &[], &evidence, &[]).expect_err("occupied");
    assert!(why.contains("pid 95 (gate-coordinator)"), "{why}");

    let sibling = scratch("propose-checkout-sibling");
    let evidence = [cwd(96, "zsh", &sibling)];
    check(&t, Path::new("/nohome"), &[], &[], &evidence, &[]).expect("a sibling holds nothing");
}

#[test]
fn an_approval_is_refused_while_any_process_sits_in_the_project() {
    let root = scratch("propose-occupied");
    let t = cargo_target(&root.join("app/target"));
    let evidence = [cwd(77, "claude", &root.join("app/src"))];
    let why = check(&t, Path::new("/nohome"), &[], &[], &evidence, &[]).expect_err("occupied");
    assert!(why.contains("pid 77 (claude)"), "{why}");
}

#[test]
fn an_approval_is_refused_while_a_build_holds_the_lock() {
    let root = scratch("propose-locked");
    let t = cargo_target(&root.join("app/target"));
    let lock = File::open(t.join("debug/.cargo-lock")).expect("open lock");
    lock.try_lock().expect("take lock");
    let why = check(&t, Path::new("/nohome"), &[], &[], &[], &[]).expect_err("locked");
    assert!(why.contains("build running"), "{why}");
}

#[test]
fn an_approval_is_refused_for_protected_referenced_or_untagged_directories() {
    let root = scratch("propose-held");
    let t = cargo_target(&root.join("app/target"));
    let why =
        check(&t, Path::new("/nohome"), &[root.join("app")], &[], &[], &[]).expect_err("protected");
    assert!(why.contains("protected"), "{why}");

    let plist = Source {
        path: PathBuf::from("/Library/LaunchAgents/x.plist"),
        text: format!("<string>{}/release/svc</string>", t.display()),
    };
    let why = check(&t, Path::new("/nohome"), &[], &[plist], &[], &[]).expect_err("referenced");
    assert!(why.contains("referenced"), "{why}");

    let plain = root.join("src/target");
    std::fs::create_dir_all(&plain).expect("mkdir");
    let why = check(&plain, Path::new("/nohome"), &[], &[], &[], &[]).expect_err("untagged");
    assert!(why.contains("CACHEDIR.TAG"), "{why}");
}

fn settings(toml: &str) -> Result<Settings, String> {
    Settings::from_config(&Config::parse(toml).expect("parse"), Path::new("/Users/me"))
}

#[test]
fn bloat_min_size_defaults_to_5g_and_zero_turns_proposals_off() {
    let d = settings("").expect("defaults");
    assert_eq!(d.bloat_min, Some(5 * GIB));
    assert!(d.skip.contains(&PathBuf::from("/Users/me/OrbStack")));
    assert_eq!(
        settings("[target-gc]\nbloat_min_size = \"0\"\n")
            .expect("off")
            .bloat_min,
        None
    );
    assert_eq!(
        settings("[target-gc]\nbloat_min_size = \"20G\"\n")
            .expect("20G")
            .bloat_min,
        Some(20 * GIB)
    );
    assert!(settings("[target-gc]\nbloat_min_size = \"10%\"\n").is_err());
}
