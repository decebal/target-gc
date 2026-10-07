//! A build directory in use is never evicted.
//!
//! Cargo's lock is per profile. Two cleaners checked on 2026-09-28 test
//! `<target>/.cargo-lock`, which Cargo never creates, so their "a build is
//! running" branch is unreachable. These tests hold the lock where Cargo
//! holds it.

mod common;

use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use common::{cargo_target, scratch};
use target_gc::liveness::{
    held_lock, hold_locks, owner, parse_lsof, snapshot_proc, Evidence, Kind, DEFAULT_WRITERS,
};
use target_gc::Target;

#[test]
fn a_held_profile_lock_holds_the_directory() {
    let root = scratch("held-lock");
    let target = cargo_target(&root.join("target"));
    let lock = target.join("debug/.cargo-lock");
    assert_eq!(held_lock(&target), None);

    let build = File::open(&lock).expect("open lock");
    build.lock().expect("take lock as a build would");
    assert_eq!(held_lock(&target), Some(lock));
    drop(build);

    assert_eq!(
        held_lock(&target),
        None,
        "released lock still reported held"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_cross_builds_newer_lock_file_counts() {
    let root = scratch("cross-lock");
    let target = cargo_target(&root.join("target"));
    let profile = target.join("aarch64-apple-darwin/release");
    fs::create_dir_all(&profile).expect("mkdir");
    let lock = profile.join(".cargo-build-lock");
    fs::write(&lock, "").expect("write");

    let build = File::open(&lock).expect("open");
    build.lock().expect("lock");
    assert_eq!(held_lock(&target), Some(lock));
    drop(build);
    let _ = fs::remove_dir_all(&root);
}

/// While eviction holds the locks, a build that starts cannot get them.
#[test]
fn holding_the_locks_keeps_a_build_out() {
    let root = scratch("hold-all");
    let target = cargo_target(&root.join("target"));
    let held = hold_locks(&target).expect("free target");
    assert!(held_lock(&target).is_some(), "a second holder got in");
    drop(held);
    assert!(held_lock(&target).is_none());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hold_locks_refuses_when_a_build_has_one() {
    let root = scratch("hold-refused");
    let target = cargo_target(&root.join("target"));
    let build = File::open(target.join("debug/.cargo-lock")).expect("open");
    build.lock().expect("lock");
    assert!(hold_locks(&target).is_err());
    drop(build);
    let _ = fs::remove_dir_all(&root);
}

fn target_at(path: &str) -> Target {
    let path = PathBuf::from(path);
    Target {
        project: path.parent().expect("parent").to_path_buf(),
        path,
        last_activity: SystemTime::UNIX_EPOCH,
    }
}

fn writers() -> Vec<String> {
    DEFAULT_WRITERS.iter().map(|w| (*w).to_string()).collect()
}

fn ev(kind: Kind, path: &str) -> Evidence {
    run_by("dx", kind, path)
}

fn run_by(command: &str, kind: Kind, path: &str) -> Evidence {
    Evidence {
        pid: 42,
        command: command.into(),
        kind,
        path: PathBuf::from(path),
    }
}

#[test]
fn a_writer_in_the_project_or_an_executable_in_the_build_dir_holds_it() {
    let t = target_at("/p/app/target");
    let w = writers();
    assert!(owner(&t, &[ev(Kind::Cwd, "/p/app")], &w).is_some());
    assert!(owner(&t, &[ev(Kind::Cwd, "/p/app/src/bin")], &w).is_some());
    assert!(owner(&t, &[ev(Kind::Exe, "/p/app/target/release/svc")], &w).is_some());
    assert!(owner(&t, &[ev(Kind::Cwd, "/p")], &w).is_none());
    assert!(owner(&t, &[ev(Kind::Exe, "/usr/bin/svc")], &w).is_none());
}

/// 2026-10-07: two agent sessions and one service sat in project roots for
/// four to six days and held 225 GiB that nothing was building.
#[test]
fn a_process_that_only_sits_in_the_project_holds_nothing() {
    let t = target_at("/p/app/target");
    let w = writers();
    for command in ["2.1.287", "claude", "zsh", "api", "node"] {
        assert!(
            owner(&t, &[run_by(command, Kind::Cwd, "/p/app")], &w).is_none(),
            "{command} sitting in the project held the build dir"
        );
    }
}

#[test]
fn a_service_running_from_the_build_dir_holds_it_whatever_its_name() {
    let t = target_at("/p/app/target");
    let svc = run_by("api", Kind::Exe, "/p/app/target/release/api");
    assert!(owner(&t, &[svc], &writers()).is_some());
}

#[test]
fn a_linux_comm_cut_at_15_bytes_still_matches_its_writer() {
    let t = target_at("/p/app/target");
    let cut = run_by("rust-analyzer-p", Kind::Cwd, "/p/app");
    let w = vec!["rust-analyzer-proc-macro-srv".to_string()];
    assert!(owner(&t, std::slice::from_ref(&cut), &w).is_some());
    assert!(
        owner(&t, &[cut], &writers()).is_none(),
        "prefix of a short writer matched"
    );
    let short = run_by("car", Kind::Cwd, "/p/app");
    assert!(
        owner(&t, &[short], &writers()).is_none(),
        "short name matched cargo"
    );
}

#[test]
fn a_sibling_with_a_longer_name_is_not_inside() {
    let t = target_at("/p/app/target");
    let w = writers();
    assert!(owner(&t, &[ev(Kind::Exe, "/p/app/target-old/release/svc")], &w).is_none());
    assert!(owner(&t, &[ev(Kind::Cwd, "/p/app-v2")], &w).is_none());
}

#[test]
fn lsof_field_output_is_parsed_per_process() {
    let out = "p101\ncdx\nfcwd\nn/p/app\nftxt\nn/Users/me/.cargo/bin/dx\n\
               p202\ncsvc\nftxt\nn/p/app/target/release/svc\nf3\nn/p/app/log.txt\n";
    let ev = parse_lsof(out);
    assert_eq!(ev.len(), 3, "the fd-3 file is neither cwd nor txt: {ev:?}");
    assert_eq!(ev[0].pid, 101);
    assert_eq!(ev[0].command, "dx");
    assert_eq!(ev[0].kind, Kind::Cwd);
    assert_eq!(ev[2].pid, 202);
    assert_eq!(ev[2].kind, Kind::Exe);
    assert_eq!(ev[2].path, PathBuf::from("/p/app/target/release/svc"));
}

#[test]
fn proc_style_links_are_read() {
    let root = scratch("fake-proc");
    let pid = root.join("4242");
    fs::create_dir_all(&pid).expect("mkdir");
    fs::write(pid.join("comm"), "bacon\n").expect("comm");
    symlink("/p/app", pid.join("cwd")).expect("cwd link");
    symlink("/usr/bin/bacon", pid.join("exe")).expect("exe link");
    fs::create_dir_all(root.join("self")).expect("non-numeric entry");

    let ev = snapshot_proc(&root);
    assert_eq!(ev.len(), 2);
    assert!(ev.iter().all(|e| e.pid == 4242 && e.command == "bacon"));
    assert!(ev
        .iter()
        .any(|e| e.kind == Kind::Cwd && e.path == Path::new("/p/app")));
    let _ = fs::remove_dir_all(&root);
}
