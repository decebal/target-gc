//! `node_modules` of long-idle projects: which ones are found, which are held,
//! and that a removal leaves no trace that reads as activity.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use common::{backdate, backdate_tree, scratch, DAY};
use target_gc::discover::{find, Walk};
use target_gc::liveness::{Evidence, Kind};
use target_gc::node::{self, expired, hold, remove, remove_trash};
use target_gc::protect::Source;
use target_gc::Hold;

fn project(root: &Path, rel: &str) -> PathBuf {
    let dir = root.join(rel);
    fs::create_dir_all(dir.join("node_modules/pkg/lib")).expect("mkdir");
    fs::write(dir.join("package.json"), "{}").expect("manifest");
    fs::write(dir.join("node_modules/pkg/lib/index.js"), "").expect("file");
    dir.join("node_modules")
}

fn walk(root: &Path) -> target_gc::discover::Found {
    find(&Walk {
        roots: &[root.to_path_buf()],
        skip_paths: &[],
        max_depth: 12,
        deadline: Instant::now() + Duration::from_secs(30),
    })
}

fn evidence(pid: u32, kind: Kind, path: &Path) -> Evidence {
    Evidence {
        pid,
        command: "node".into(),
        kind,
        path: path.to_path_buf(),
    }
}

#[test]
fn only_project_node_modules_outside_dot_directories_are_found() {
    let root = scratch("node-find");
    let app = project(&root, "app");
    project(&root, ".vscode/extensions/ext");
    fs::create_dir_all(root.join("loose/node_modules")).expect("no manifest");
    let nested = app.join("pkg/node_modules");
    fs::create_dir_all(&nested).expect("nested");
    fs::write(app.join("pkg/package.json"), "{}").expect("nested manifest");

    assert_eq!(walk(&root).node_modules, vec![app]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn activity_anywhere_in_the_checkout_keeps_a_package() {
    let root = scratch("node-activity");
    fs::create_dir_all(root.join(".git")).expect("git");
    let deps = project(&root, "packages/ui");
    fs::create_dir_all(root.join("apps/web/src")).expect("sibling");
    backdate_tree(&root, 90 * DAY);
    let idle = node::at(deps.clone());
    assert!(idle.idle(SystemTime::now()) > 89 * DAY);

    backdate(&root.join("apps/web"), Duration::from_secs(60));
    let busy = node::at(deps);
    assert!(busy.idle(SystemTime::now()) < DAY);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_change_inside_node_modules_is_not_activity() {
    let root = scratch("node-own-writes");
    let deps = project(&root, "app");
    backdate_tree(&root, 90 * DAY);
    backdate(&deps.join("pkg"), Duration::from_secs(60));
    assert!(node::at(deps).idle(SystemTime::now()) > 89 * DAY);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_process_in_the_checkout_or_a_reference_to_the_project_holds_it() {
    let root = scratch("node-hold");
    let deps = node::at(project(&root, "app"));
    let none: [Source; 0] = [];
    assert_eq!(hold(&deps, Path::new("/nohome"), &[], &none, &[]), None);

    let cwd = [evidence(7, Kind::Cwd, &root.join("app/src"))];
    assert!(matches!(
        hold(&deps, Path::new("/nohome"), &[], &none, &cwd),
        Some(Hold::InUse { pid: 7, .. })
    ));

    let exe = [evidence(
        8,
        Kind::Exe,
        &deps.path.join("esbuild/bin/esbuild"),
    )];
    assert!(matches!(
        hold(&deps, Path::new("/nohome"), &[], &none, &exe),
        Some(Hold::InUse { pid: 8, .. })
    ));

    let mcp = [Source {
        path: PathBuf::from("/home/me/.claude.json"),
        text: format!("\"args\": [\"{}/dist/index.js\"]", deps.project.display()),
    }];
    assert!(matches!(
        hold(&deps, Path::new("/nohome"), &[], &mcp, &[]),
        Some(Hold::Referenced(_))
    ));

    let protected = [deps.project.clone()];
    assert!(matches!(
        hold(&deps, Path::new("/nohome"), &protected, &none, &[]),
        Some(Hold::Protected(_))
    ));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn only_idle_unheld_directories_expire_oldest_first() {
    let root = scratch("node-expire");
    let old = project(&root, "old");
    let older = project(&root, "older");
    let fresh = project(&root, "fresh");
    let held = project(&root, "held");
    backdate_tree(&root.join("old"), 70 * DAY);
    backdate_tree(&root.join("older"), 200 * DAY);
    backdate_tree(&root.join("held"), 200 * DAY);
    let busy = Hold::InUse {
        pid: 1,
        command: "node".into(),
    };
    let found = vec![
        (node::at(old.clone()), None),
        (node::at(fresh), None),
        (node::at(older.clone()), None),
        (node::at(held), Some(busy)),
    ];
    let due: Vec<PathBuf> = expired(found, 60 * DAY, SystemTime::now())
        .into_iter()
        .map(|d| d.path)
        .collect();
    assert_eq!(due, vec![older, old]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn removal_deletes_read_only_packages_and_keeps_the_project_idle() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("node-remove");
    fs::create_dir_all(root.join(".git")).expect("git");
    let a = project(&root, "packages/a");
    let b = project(&root, "packages/b");
    let locked = a.join("pkg/lib");
    backdate_tree(&root, 90 * DAY);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("chmod");
    backdate(&locked, 90 * DAY);

    let first = node::at(a.clone());
    let second = node::at(b.clone());
    remove(&first).expect("remove a");
    assert!(!a.exists());
    assert!(root.join("packages/a/package.json").exists());
    remove(&second).expect("a sibling's removal must not read as activity");
    assert!(!b.exists());
    assert!(node::at(root.join("packages/a/node_modules")).idle(SystemTime::now()) > 89 * DAY);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn removal_is_refused_after_the_project_changed() {
    let root = scratch("node-changed");
    let deps = project(&root, "app");
    backdate_tree(&root, 90 * DAY);
    let scanned = node::at(deps.clone());
    fs::write(root.join("app/index.js"), "edit").expect("edit");
    assert!(remove(&scanned).is_err());
    assert!(deps.exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_interrupted_removal_is_found_and_finished() {
    let root = scratch("node-trash");
    let trash = root.join("app/.target-gc-node-trash.1.2");
    fs::create_dir_all(trash.join("pkg")).expect("mkdir");
    assert_eq!(walk(&root).node_trash, vec![trash.clone()]);
    remove_trash(&trash).expect("finish");
    assert!(!trash.exists());
    assert!(remove_trash(&root.join("app")).is_err());
    let _ = fs::remove_dir_all(&root);
}
