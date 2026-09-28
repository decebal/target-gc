//! A build directory is identified by Cargo's tag, never by its name.
//!
//! The failure this pins was observed, not imagined: a name-matching cleaner
//! planned to delete `node_modules/.pnpm/airbnb-js-shims@2.2.1/node_modules/
//! airbnb-js-shims/target/`, which holds that package's `es2015.js` … `es5.js`.

mod common;

use std::fs;
use std::time::{Duration, Instant, SystemTime};

use common::{backdate, backdate_tree, cargo_target, scratch, DAY};
use target_gc::discover::{find, is_cargo_build_dir, last_activity, profile_dirs, Walk};

fn walk_all(root: &std::path::Path) -> target_gc::discover::Found {
    find(&Walk {
        roots: &[root.to_path_buf()],
        skip_paths: &[],
        max_depth: 12,
        deadline: Instant::now() + Duration::from_secs(30),
    })
}

#[test]
fn a_directory_named_target_without_cargos_tag_is_never_found() {
    let root = scratch("name-only");
    let real = cargo_target(&root.join("crate/target"));
    let js_source = root.join("app/lib/target");
    fs::create_dir_all(&js_source).expect("mkdir");
    fs::write(js_source.join("es2015.js"), "module.exports = {}").expect("write");
    let in_node_modules = root.join("app/node_modules/pkg/target");
    cargo_target(&in_node_modules);

    let found = walk_all(&root);
    let paths: Vec<_> = found.targets.iter().map(|t| t.path.clone()).collect();
    assert_eq!(
        paths,
        vec![real],
        "only the Cargo-tagged directory may be found"
    );
    assert!(js_source.join("es2015.js").exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn another_tools_cache_tag_is_not_cargos() {
    let root = scratch("other-tag");
    let dir = root.join("proj/target");
    fs::create_dir_all(&dir).expect("mkdir");
    fs::write(
        dir.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# created by some other tool\n",
    )
    .expect("write");
    assert!(!is_cargo_build_dir(&dir));
    assert!(walk_all(&root).targets.is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_build_directory_moved_under_another_name_is_found() {
    let root = scratch("custom-name");
    let custom = cargo_target(&root.join("proj/build-output"));
    let found = walk_all(&root);
    assert_eq!(found.targets.len(), 1);
    assert_eq!(found.targets[0].path, custom);
    assert_eq!(found.targets[0].project, root.join("proj"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn discovery_stops_at_a_build_directory() {
    let root = scratch("no-descend");
    let outer = cargo_target(&root.join("proj/target"));
    cargo_target(&outer.join("debug/build/nested/target"));
    assert_eq!(walk_all(&root).targets.len(), 1);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_interrupted_evictions_trash_is_reported_separately() {
    let root = scratch("trash");
    let trash = cargo_target(&root.join("proj/.target-gc-trash.target.1.2"));
    let found = walk_all(&root);
    assert!(found.targets.is_empty());
    assert_eq!(found.trash, vec![trash]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn profiles_are_found_for_host_and_cross_builds() {
    let root = scratch("profiles");
    let target = cargo_target(&root.join("target"));
    let cross = target.join("aarch64-apple-darwin/release");
    fs::create_dir_all(&cross).expect("mkdir");
    fs::write(cross.join(".cargo-build-lock"), "").expect("lock");
    assert_eq!(profile_dirs(&target), vec![cross, target.join("debug")]);
    let _ = fs::remove_dir_all(&root);
}

/// Compiling writes into `<profile>/deps`; that must count as activity even
/// when the build directory's own mtime is old.
#[test]
fn activity_is_read_from_the_directories_a_compile_writes() {
    let root = scratch("activity");
    let target = cargo_target(&root.join("target"));
    backdate_tree(&target, 40 * DAY);
    let old = last_activity(&target);
    assert!(SystemTime::now().duration_since(old).expect("past") > 39 * DAY);

    backdate(&target.join("debug/deps"), Duration::from_secs(60));
    let fresh = last_activity(&target);
    assert!(SystemTime::now().duration_since(fresh).expect("past") < DAY);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_cross_builds_activity_counts_too() {
    let root = scratch("activity-cross");
    let target = cargo_target(&root.join("target"));
    let deps = target.join("x86_64-unknown-linux-gnu/release/deps");
    fs::create_dir_all(&deps).expect("mkdir");
    fs::write(deps.parent().expect("profile").join(".cargo-lock"), "").expect("lock");
    backdate_tree(&target, 40 * DAY);
    backdate(&deps, Duration::from_secs(60));
    let fresh = last_activity(&target);
    assert!(SystemTime::now().duration_since(fresh).expect("past") < DAY);
    let _ = fs::remove_dir_all(&root);
}
