//! Append-only JSONL event log (events/YYYY-MM-DD.jsonl), and readers for the
//! tray/status: last run summary, and the "needs attention" set.

use crate::paths::Paths;
use chrono::{SecondsFormat, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

fn machine() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

fn append_line(path: &Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Append a structured event to events/YYYY-MM-DD.jsonl (UTC date).
pub fn write_event(paths: &Paths, event: &str, fields: serde_json::Value) {
    let _ = std::fs::create_dir_all(paths.events_dir());
    let mut obj = serde_json::Map::new();
    obj.insert(
        "ts".into(),
        serde_json::Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)),
    );
    obj.insert("event".into(), serde_json::Value::String(event.to_string()));
    obj.insert("machine".into(), serde_json::Value::String(machine()));
    if let serde_json::Value::Object(m) = fields {
        for (k, v) in m {
            obj.insert(k, v);
        }
    }
    let f = paths
        .events_dir()
        .join(format!("{}.jsonl", Utc::now().format("%Y-%m-%d")));
    append_line(&f, &serde_json::Value::Object(obj).to_string());
}

/// Append a timestamped line to sync.log and echo it to stderr (port of Write-OdsLog).
pub fn log(paths: &Paths, level: &str, msg: &str) {
    let _ = std::fs::create_dir_all(paths.logs_dir());
    let line = format!(
        "{} [{level}] {msg}",
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
    append_line(&paths.log_file(), &line);
    eprintln!("{line}");
}

#[derive(Debug, Deserialize)]
pub struct Event {
    pub ts: String,
    pub event: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub code: Option<i32>,
    #[serde(default)]
    pub dryrun: Option<bool>,
    #[serde(default)]
    pub summary: Option<String>,
    /// Tail of rclone's ERROR-level log lines from a failed "bisync" event (see
    /// `engine::tail_error_lines`); empty on success or on older events written
    /// before this field existed.
    #[serde(default)]
    pub error: Option<String>,
}

/// Events from today + yesterday (UTC), oldest-first read order.
pub fn recent_events(paths: &Paths) -> Vec<Event> {
    let mut out = Vec::new();
    let today = Utc::now().date_naive();
    let days = [today.pred_opt().unwrap_or(today), today];
    for d in days {
        let f = paths.events_dir().join(format!("{}.jsonl", d.format("%Y-%m-%d")));
        if let Ok(text) = std::fs::read_to_string(&f) {
            for line in text.lines() {
                if let Ok(e) = serde_json::from_str::<Event>(line) {
                    out.push(e);
                }
            }
        }
    }
    out
}

/// Most recent run-end (ISO-8601 ts sorts chronologically).
pub fn last_run_end(paths: &Paths) -> Option<Event> {
    let mut ends: Vec<Event> = recent_events(paths)
        .into_iter()
        .filter(|e| e.event == "run-end")
        .collect();
    ends.sort_by(|a, b| a.ts.cmp(&b.ts));
    ends.pop()
}

/// Most-recent bisync timestamp per project id (today + yesterday), for the GUI's
/// "last sync" column.
pub fn last_sync_per_project(paths: &Paths) -> BTreeMap<String, String> {
    let mut last: BTreeMap<String, String> = BTreeMap::new();
    for e in recent_events(paths) {
        if e.event == "bisync" {
            if let Some(id) = e.id {
                last.insert(id, e.ts);
            }
        }
    }
    last
}

/// Ids whose most-recent real (non-dry-run) bisync did not succeed (code != 0).
pub fn attention_ids(paths: &Paths) -> Vec<String> {
    attention_reasons(paths).into_keys().collect()
}

/// Ids needing attention, mapped to a human-readable reason drawn from the
/// failing bisync's captured rclone error text (falling back to the exit code
/// when rclone logged nothing, e.g. a launch failure or watchdog kill). This is
/// what the GUI shows next to the Attention badge and in "View conflicts" so a
/// failure that isn't an unresolved conflict copy still explains itself.
pub fn attention_reasons(paths: &Paths) -> BTreeMap<String, String> {
    let mut last: BTreeMap<String, (i32, String)> = BTreeMap::new();
    for e in recent_events(paths) {
        if e.event == "bisync" && !e.dryrun.unwrap_or(false) {
            if let (Some(id), Some(code)) = (e.id, e.code) {
                last.insert(id, (code, e.error.unwrap_or_default()));
            }
        }
    }
    last.into_iter()
        .filter(|(_, (code, _))| *code != 0)
        .map(|(id, (code, err))| {
            let reason = if err.trim().is_empty() { describe_exit_code(code) } else { err };
            (id, reason)
        })
        .collect()
}

/// Fallback gloss used only when rclone's own log carried no ERROR line for a
/// failed bisync (e.g. it never started, or code 9 — this tool's own watchdog
/// kill on a timeout, not an rclone-native code).
fn describe_exit_code(code: i32) -> String {
    if code == 9 {
        "sync exceeded its time budget and was killed (watchdog)".to_string()
    } else {
        format!("rclone exited with code {code} (see sync.log for details)")
    }
}
