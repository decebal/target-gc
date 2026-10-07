//! Free space, levels, settings, the session line, and the schedule files.

use std::path::Path;
use std::time::Duration;

use target_gc::config::Settings;
use target_gc::disk::{parse_df, Level, Space};
use target_gc::install::{launchd_plist, systemd_service, LABEL};
use target_gc::state::{hook_line, Record};
use target_gc::toml_subset::Config;

const GIB: u64 = 1024 * 1024 * 1024;

#[test]
fn df_is_read_back_from_the_capacity_column() {
    let mac = "Filesystem     1024-blocks      Used Available Capacity  Mounted on\n\
               /dev/disk3s1s1   971350180  11906676 139481968     8%    /\n";
    assert_eq!(
        parse_df(mac),
        Some(Space {
            total: 971_350_180 * 1024,
            available: 139_481_968 * 1024
        })
    );
    let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                  map auto_home 100 40 60 40% /System/Volumes/Data/home dir\n";
    assert_eq!(parse_df(spaced).map(|s| s.available), Some(60 * 1024));
    assert_eq!(parse_df("garbage"), None);
}

#[test]
fn levels_parse_in_binary_units_and_percentages() {
    assert_eq!(Level::parse("50G"), Some(Level::Bytes(50 * GIB)));
    assert_eq!(Level::parse("50GiB"), Some(Level::Bytes(50 * GIB)));
    assert_eq!(Level::parse("512M"), Some(Level::Bytes(512 * 1024 * 1024)));
    assert_eq!(Level::parse("10%"), Some(Level::Percent(10)));
    assert_eq!(Level::parse("101%"), None);
    assert_eq!(Level::parse("lots"), None);
    assert_eq!(Level::Percent(10).resolve(1000 * GIB), 100 * GIB);
}

fn settings(toml: &str) -> Result<Settings, String> {
    Settings::from_config(&Config::parse(toml).expect("parse"), Path::new("/Users/me"))
}

#[test]
fn the_shipped_example_config_loads_to_the_defaults() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
    let loaded = Settings::load(&example, Path::new("/Users/me")).expect("example parses");
    let mut defaults = Settings::defaults(Path::new("/Users/me"));
    defaults.scan_files.retain(|p| !p.ends_with("config.fish"));
    assert_eq!(
        loaded, defaults,
        "the example drifted from the defaults it documents"
    );
}

#[test]
fn settings_default_and_override() {
    let d = settings("").expect("defaults");
    assert_eq!(d.roots, vec![Path::new("/Users/me").to_path_buf()]);
    assert_eq!(d.max_idle, Some(Duration::from_secs(30 * 86_400)));

    let s =
        settings("[target-gc]\nroots = [\"~/code\"]\nkeep_free = \"40G\"\nmax_idle_days = \"0\"\n")
            .expect("override");
    assert_eq!(s.roots, vec![Path::new("/Users/me/code").to_path_buf()]);
    assert_eq!(s.keep_free, Level::Bytes(40 * GIB));
    assert_eq!(s.max_idle, None, "\"0\" must disable expiry");
}

/// A configured writer adds to the defaults; listing one must not drop `dx`.
#[test]
fn writers_extend_the_defaults() {
    let s = settings("[target-gc]\nwriters = [\"my-watcher\", \"dx\"]\n").expect("writers");
    assert!(s.writers.iter().any(|w| w == "my-watcher"));
    assert_eq!(s.writers.iter().filter(|w| *w == "dx").count(), 1);
    assert!(s.writers.iter().any(|w| w == "bacon"));
}

/// A build seconds old may sit between two Cargo invocations with no lock
/// held; a floor of zero would put it in scope.
#[test]
fn a_zero_idle_floor_is_refused() {
    assert!(settings("[target-gc]\nmin_idle_days = \"0\"\n").is_err());
    assert!(settings("[target-gc]\nkeep_free = \"plenty\"\n").is_err());
    assert!(settings("[target-gc]\nmin_idle_days = \"5\"\nmax_idle_days = \"3\"\n").is_err());
}

fn record(finished_at: u64) -> Record {
    Record {
        finished_at,
        exit_code: 0,
        available_before: 30 * GIB,
        available_after: 30 * GIB,
        evicted: 0,
        held: 4,
        error: None,
    }
}

#[test]
fn the_session_hook_is_silent_only_when_everything_is_fine() {
    let now = 1_000_000;
    let fine = record(now - 600);
    assert_eq!(hook_line(Some(&fine), Some(200 * GIB), 50 * GIB, now), None);

    let low = hook_line(Some(&fine), Some(30 * GIB), 50 * GIB, now).expect("low disk");
    assert!(low.contains("under the"), "{low}");

    let stale = hook_line(
        Some(&record(now - 5 * 3600)),
        Some(200 * GIB),
        50 * GIB,
        now,
    )
    .expect("stale schedule");
    assert!(stale.contains("no run for"), "{stale}");

    let mut broken = record(now - 60);
    broken.error = Some("could not list running processes".into());
    assert!(hook_line(Some(&broken), Some(200 * GIB), 50 * GIB, now)
        .expect("error")
        .contains("could not complete"));

    assert!(hook_line(None, Some(200 * GIB), 50 * GIB, now)
        .expect("never ran")
        .contains("never completed"));
}

#[test]
fn a_record_round_trips_through_json() {
    let r = record(123);
    let v = r.to_json(serde_json::Value::Null);
    assert_eq!(Record::from_json(&v), Some(r));
}

#[test]
fn the_schedule_runs_the_installed_binary_hourly() {
    let plist = launchd_plist(Path::new("/Users/me/.cargo/bin/target-gc"), Path::new("/s"));
    assert!(plist.contains(&format!("<string>{LABEL}</string>")));
    assert!(plist.contains("<string>/Users/me/.cargo/bin/target-gc</string><string>run</string>"));
    assert!(plist.contains("<integer>3600</integer>"));
    let amp = launchd_plist(Path::new("/a&b/target-gc"), Path::new("/s"));
    assert!(amp.contains("/a&amp;b/target-gc"));
    assert!(systemd_service(Path::new("/usr/local/bin/target-gc"))
        .contains("ExecStart=/usr/local/bin/target-gc run --quiet"));
}
