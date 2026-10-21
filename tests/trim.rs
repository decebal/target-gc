//! Trimming superseded `deps/` artifacts from a build directory that is kept.

mod common;

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use common::{backdate, cargo_target, scratch, DAY};
use target_gc::trim::{self, split_name, KEEP_PER_STEM};

fn artifact(deps: &Path, name: &str, ago: Duration) {
    let path = deps.join(name);
    fs::write(&path, vec![0u8; 4096]).expect("write artifact");
    backdate(&path, ago);
}

fn names(deps: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(deps)
        .expect("read deps")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn never(_: &Path) -> bool {
    false
}

#[test]
fn a_name_needs_a_sixteen_hex_hash_to_be_planned() {
    assert_eq!(
        split_name("libfoo-0123456789abcdef.rlib"),
        Some(("libfoo", "0123456789abcdef"))
    );
    assert_eq!(
        split_name("my_app-0123456789abcdef"),
        Some(("my_app", "0123456789abcdef"))
    );
    assert_eq!(
        split_name("my_app-0123456789abcdef.dSYM"),
        Some(("my_app", "0123456789abcdef"))
    );
    assert_eq!(split_name("libfoo.rlib"), None);
    assert_eq!(split_name("rustcXyZ123"), None);
    assert_eq!(split_name("foo-bar"), None);
    assert_eq!(split_name("-0123456789abcdef"), None);
}

#[test]
fn old_hashes_go_and_the_newest_ones_stay() {
    let root = scratch("trim-old");
    let target = cargo_target(&root.join("proj/target"));
    let deps = target.join("debug/deps");
    for (i, hash) in [
        "aaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbb",
        "cccccccccccccccc",
        "dddddddddddddddd",
    ]
    .iter()
    .enumerate()
    {
        let ago = (10 - i as u32) * DAY;
        artifact(&deps, &format!("app-{hash}"), ago);
        artifact(&deps, &format!("app-{hash}.d"), ago);
    }

    let plan = trim::plan(&target, SystemTime::now() - 3 * DAY, &never);
    assert_eq!(plan.artifacts.len(), 4 - KEEP_PER_STEM);
    trim::trim(&target, &plan).expect("trim");

    let left = names(&deps);
    assert!(left.contains(&"app-dddddddddddddddd".to_string()));
    assert!(left.contains(&"app-cccccccccccccccc.d".to_string()));
    assert!(
        !left.iter().any(|n| n.starts_with("app-aaaaaaaaaaaaaaaa")),
        "{left:?}"
    );
    assert!(
        !left.iter().any(|n| n.starts_with("app-bbbbbbbbbbbbbbbb")),
        "{left:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_superseded_artifact_built_recently_stays() {
    let root = scratch("trim-recent");
    let target = cargo_target(&root.join("proj/target"));
    let deps = target.join("debug/deps");
    artifact(&deps, "app-aaaaaaaaaaaaaaaa", Duration::from_secs(3600));
    artifact(&deps, "app-bbbbbbbbbbbbbbbb", Duration::from_secs(1800));
    artifact(&deps, "app-cccccccccccccccc", Duration::from_secs(60));

    let plan = trim::plan(&target, SystemTime::now() - 3 * DAY, &never);
    assert!(plan.artifacts.is_empty(), "{:?}", plan.artifacts);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_lone_old_dependency_is_never_touched() {
    let root = scratch("trim-lone");
    let target = cargo_target(&root.join("proj/target"));
    let deps = target.join("debug/deps");
    artifact(&deps, "libserde-aaaaaaaaaaaaaaaa.rlib", 90 * DAY);
    artifact(&deps, "libserde-aaaaaaaaaaaaaaaa.rmeta", 90 * DAY);

    let plan = trim::plan(&target, SystemTime::now() - 3 * DAY, &never);
    assert!(plan.artifacts.is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_running_build_blocks_the_trim_and_nothing_is_removed() {
    let root = scratch("trim-locked");
    let target = cargo_target(&root.join("proj/target"));
    let deps = target.join("debug/deps");
    for (i, hash) in ["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb", "cccccccccccccccc"]
        .iter()
        .enumerate()
    {
        artifact(&deps, &format!("app-{hash}"), (10 - i as u32) * DAY);
    }
    let plan = trim::plan(&target, SystemTime::now() - 3 * DAY, &never);
    assert_eq!(plan.artifacts.len(), 1);

    let build = File::open(target.join("debug/.cargo-lock")).expect("open lock");
    build.lock().expect("lock");
    assert!(trim::trim(&target, &plan).is_err());
    assert!(deps.join("app-aaaaaaaaaaaaaaaa").exists());
    drop(build);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_artifact_the_caller_keeps_survives() {
    let root = scratch("trim-keep");
    let target = cargo_target(&root.join("proj/target"));
    let deps = target.join("debug/deps");
    for (i, hash) in ["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb", "cccccccccccccccc"]
        .iter()
        .enumerate()
    {
        artifact(&deps, &format!("app-{hash}"), (10 - i as u32) * DAY);
    }
    let running = deps.join("app-aaaaaaaaaaaaaaaa");
    let keep = |p: &Path| p == running;

    let plan = trim::plan(&target, SystemTime::now() - 3 * DAY, &keep);
    assert!(plan.artifacts.is_empty(), "{:?}", plan.artifacts);
    let _ = fs::remove_dir_all(&root);
}
