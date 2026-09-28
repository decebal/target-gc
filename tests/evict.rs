//! Removing a build directory: re-verify, hold the locks, rename aside, delete.

mod common;

use std::fs::{self, File};
use std::path::PathBuf;
use std::time::Duration;

use common::{backdate, backdate_tree, cargo_target, scratch, DAY};
use target_gc::discover::target_at;
use target_gc::evict::{evict, remove_trash, Refusal};
use target_gc::liveness::{Evidence, Kind};

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("read")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn an_idle_directory_is_removed_and_leaves_nothing_behind() {
    let root = scratch("evict-ok");
    let target = cargo_target(&root.join("proj/target"));
    fs::write(root.join("proj/Cargo.toml"), "[package]\n").expect("manifest");
    backdate_tree(&target, 40 * DAY);

    evict(&target_at(target.clone()), &[]).expect("evict");
    assert!(!target.exists());
    assert_eq!(leftovers(&root.join("proj")), ["Cargo.toml"]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_build_that_holds_a_lock_keeps_its_directory() {
    let root = scratch("evict-locked");
    let target = cargo_target(&root.join("proj/target"));
    let t = target_at(target.clone());
    let build = File::open(target.join("debug/.cargo-lock")).expect("open");
    build.lock().expect("lock");

    let refused = evict(&t, &[]).expect_err("evicted under a held lock");
    assert!(matches!(refused, Refusal::Skipped(_)), "{refused:?}");
    assert!(target.join("debug/deps").exists());
    drop(build);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_directory_built_after_the_scan_is_left_alone() {
    let root = scratch("evict-rebuilt");
    let target = cargo_target(&root.join("proj/target"));
    backdate_tree(&target, 40 * DAY);
    let scanned = target_at(target.clone());
    backdate(&target.join("debug/deps"), Duration::from_secs(5));

    let refused = evict(&scanned, &[]).expect_err("evicted a rebuilt directory");
    assert_eq!(refused.reason(), "built since the scan");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_process_in_the_project_keeps_it() {
    let root = scratch("evict-in-use");
    let target = cargo_target(&root.join("proj/target"));
    let t = target_at(target.clone());
    let dev_server = Evidence {
        pid: 7,
        command: "dx".into(),
        kind: Kind::Cwd,
        path: root.join("proj"),
    };
    assert!(evict(&t, &[dev_server]).is_err());
    assert!(target.exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_untagged_directory_is_refused_even_if_the_plan_named_it() {
    let root = scratch("evict-untagged");
    let dir = root.join("proj/target");
    fs::create_dir_all(&dir).expect("mkdir");
    fs::write(dir.join("es5.js"), "x").expect("write");
    let t = target_gc::Target {
        path: dir.clone(),
        project: root.join("proj"),
        last_activity: std::time::SystemTime::now(),
    };
    assert!(evict(&t, &[]).is_err());
    assert!(dir.join("es5.js").exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn trash_is_finished_only_when_it_is_ours() {
    let root = scratch("trash-finish");
    let ours = cargo_target(&root.join(".target-gc-trash.target.1.2"));
    let not_ours = cargo_target(&root.join("target"));
    let untagged: PathBuf = root.join(".target-gc-trash.docs.1.2");
    fs::create_dir_all(&untagged).expect("mkdir");

    remove_trash(&ours).expect("remove ours");
    assert!(!ours.exists());
    assert!(remove_trash(&not_ours).is_err());
    assert!(remove_trash(&untagged).is_err());
    assert!(not_ours.exists() && untagged.exists());
    let _ = fs::remove_dir_all(&root);
}
