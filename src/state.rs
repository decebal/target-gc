//! The record each run leaves behind, and the one line the session hook
//! derives from it.
//!
//! A scheduled job that stops running is silent, and silence reads exactly
//! like "nothing needed doing". So the hook speaks up when the record is
//! missing or stale, not only when the disk is low.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::{human_age, human_bytes};

pub const RECORD: &str = "last-run.json";

/// A scheduled run older than this means the job is not running. The job is
/// installed hourly, so three missed runs.
pub const STALE_AFTER: Duration = Duration::from_secs(3 * 3600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub finished_at: u64,
    pub exit_code: i32,
    pub available_before: u64,
    pub available_after: u64,
    pub evicted: usize,
    pub held: usize,
    pub error: Option<String>,
}

impl Record {
    pub fn to_json(&self, detail: Value) -> Value {
        json!({
            "finished_at": self.finished_at,
            "exit_code": self.exit_code,
            "available_before": self.available_before,
            "available_after": self.available_after,
            "evicted": self.evicted,
            "held": self.held,
            "error": self.error,
            "detail": detail,
        })
    }

    pub fn from_json(v: &Value) -> Option<Record> {
        Some(Record {
            finished_at: v["finished_at"].as_u64()?,
            exit_code: i32::try_from(v["exit_code"].as_i64()?).ok()?,
            available_before: v["available_before"].as_u64()?,
            available_after: v["available_after"].as_u64()?,
            evicted: usize::try_from(v["evicted"].as_u64()?).ok()?,
            held: usize::try_from(v["held"].as_u64()?).ok()?,
            error: v["error"].as_str().map(str::to_string),
        })
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn write(dir: &Path, value: &Value) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!("{RECORD}.tmp"));
    let body = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, dir.join(RECORD)).map_err(|e| e.to_string())
}

pub fn read(dir: &Path) -> Option<Record> {
    let text = fs::read_to_string(dir.join(RECORD)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Record::from_json(&value)
}

/// What a session should be told, or `None` for nothing. Pure, so every
/// branch is testable without a disk or a clock.
pub fn hook_line(
    record: Option<&Record>,
    available: Option<u64>,
    keep_free: u64,
    now: u64,
) -> Option<String> {
    let Some(record) = record else {
        return Some(
            "target-gc has never completed a run on this machine. Install the schedule with \
             `target-gc install`."
                .into(),
        );
    };
    let age = Duration::from_secs(now.saturating_sub(record.finished_at));
    if let Some(error) = &record.error {
        return Some(format!(
            "target-gc: the last run ({} ago) could not complete: {error}",
            human_age(age)
        ));
    }
    if age > STALE_AFTER {
        return Some(format!(
            "target-gc: no run for {}. The schedule is not running; `target-gc install` \
             reinstalls it.",
            human_age(age)
        ));
    }
    let available = available?;
    if available >= keep_free {
        return None;
    }
    Some(format!(
        "target-gc: {} free, under the {} floor. {} build directories are in use or protected; \
         the last run ({} ago) removed {}. `target-gc scan` lists them.",
        human_bytes(available),
        human_bytes(keep_free),
        record.held,
        human_age(age),
        record.evicted,
    ))
}
