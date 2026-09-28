//! A service running a binary out of a build directory protects it, even on
//! the day the service is stopped and nothing else says it matters.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use target_gc::protect::{protection, referenced_by, Source};
use target_gc::{Hold, Target};

fn source(path: &str, text: &str) -> Source {
    Source {
        path: PathBuf::from(path),
        text: text.to_string(),
    }
}

fn target(path: &str) -> Target {
    let path = PathBuf::from(path);
    Target {
        project: path.parent().expect("parent").to_path_buf(),
        path,
        last_activity: SystemTime::UNIX_EPOCH,
    }
}

const HOME: &str = "/Users/me";

#[test]
fn a_plist_naming_a_binary_inside_protects_the_directory() {
    let plist = source(
        "/Users/me/Library/LaunchAgents/svc.plist",
        "<string>/Users/me/.tools/svc-0.2/target/release/svc</string>",
    );
    let sources = [plist];
    let hit = referenced_by(
        Path::new("/Users/me/.tools/svc-0.2/target"),
        Path::new(HOME),
        &sources,
    );
    assert_eq!(
        hit,
        Some(Path::new("/Users/me/Library/LaunchAgents/svc.plist"))
    );
}

#[test]
fn a_home_relative_spelling_counts() {
    let rc = source("/Users/me/.zshrc", "alias s=~/code/svc/target/debug/svc\n");
    let rc2 = source(
        "/Users/me/.profile",
        "export P=$HOME/code/svc/target/release/x\n",
    );
    let t = Path::new("/Users/me/code/svc/target");
    assert!(referenced_by(t, Path::new(HOME), &[rc]).is_some());
    assert!(referenced_by(t, Path::new(HOME), &[rc2]).is_some());
}

#[test]
fn a_sibling_directorys_path_is_not_a_reference() {
    let plist = source("/x.plist", "/Users/me/code/svc/target-old/release/svc");
    assert!(referenced_by(
        Path::new("/Users/me/code/svc/target"),
        Path::new(HOME),
        &[plist]
    )
    .is_none());
}

#[test]
fn a_configured_prefix_protects_everything_under_it() {
    let hold = protection(
        &target("/Users/me/.codex/tooling/a/target"),
        Path::new(HOME),
        &[PathBuf::from("/Users/me/.codex/tooling")],
        &[],
    );
    assert_eq!(
        hold,
        Some(Hold::Protected(PathBuf::from("/Users/me/.codex/tooling")))
    );
    assert!(protection(
        &target("/Users/me/code/a/target"),
        Path::new(HOME),
        &[],
        &[]
    )
    .is_none());
}
