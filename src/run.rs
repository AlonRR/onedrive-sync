//! The scheduled/default run — the Rust port of Invoke-OdsRun's core: pause gate,
//! run lock, run-start/run-end events, and the time-ordered active-sync loop with the
//! missing-side guard and the git health check. Summary = per-project results grouped
//! by status in first-appearance order ("ok=5", "error=1 ok=2 warn=2").

use crate::config::Config;
use crate::discovery::{discover, Project};
use crate::engine::{bisync, BisyncOpts};
use crate::paths::Paths;
use crate::state::{Catalog, MachineState, Status};
use crate::{events, git};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOpts {
    pub dry_run: bool,
    pub ignore_pause: bool,
}

struct LockGuard {
    path: PathBuf,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Atomic run lock; breaks a presumed-stale lock older than an hour rather than deadlock.
fn acquire_lock(paths: &Paths) -> Option<LockGuard> {
    let _ = std::fs::create_dir_all(&paths.local_root);
    let p = paths.lock_file();
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
        Ok(mut f) => {
            let _ = write!(f, "{}", std::process::id());
            Some(LockGuard { path: p })
        }
        Err(_) => {
            let stale = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|e| e.as_secs() > 3600).unwrap_or(false))
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(&p);
                return acquire_lock(paths);
            }
            None
        }
    }
}

fn tally(order: &mut Vec<String>, counts: &mut HashMap<String, u32>, status: &str) {
    *counts.entry(status.to_string()).or_insert(0) += 1;
    if !order.iter().any(|s| s == status) {
        order.push(status.to_string());
    }
}

/// Execute a run; returns the summary string (empty if paused/locked-out).
pub fn run(paths: &Paths, config: &Config, opts: RunOpts) -> String {
    if !opts.ignore_pause && paths.paused_flag().exists() {
        events::log(paths, "INFO", "Sync paused (paused.flag present); skipping run.");
        return String::new();
    }
    let _lock = match acquire_lock(paths) {
        Some(l) => l,
        None => {
            events::log(paths, "WARN", "Another run holds the lock; exiting.");
            return String::new();
        }
    };

    events::write_event(
        paths,
        "run-start",
        serde_json::json!({"dryrun": opts.dry_run, "interactive": false}),
    );

    let state = MachineState::load(paths);
    let catalog = Catalog::load(paths);
    let projects = discover(paths, config, &catalog);
    let active: Vec<&Project> = projects
        .iter()
        .filter(|p| state.status_of(&p.id) == Status::Active)
        .collect();

    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, u32> = HashMap::new();

    for p in &active {
        // Vanished-side guard: never propagate a wholesale deletion.
        if !p.local.exists() || !p.dest.exists() {
            events::log(
                paths,
                "WARN",
                &format!("{}: a side is missing (keeping present side, not propagating deletion).", p.id),
            );
            continue;
        }
        if p.git {
            if let Err(e) = git::integrity_ok(&p.local) {
                events::log(
                    paths,
                    "ERROR",
                    &format!("git fsck found corruption in {}: {}", p.id, e),
                );
                tally(&mut order, &mut counts, "error");
                continue;
            }
        }
        let code = bisync(
            paths,
            config,
            &state,
            p,
            BisyncOpts {
                dry_run: opts.dry_run,
                ..Default::default()
            },
        );
        let status = if code == 0 {
            "ok"
        } else if code < 8 {
            "warn"
        } else {
            "error"
        };
        tally(&mut order, &mut counts, status);
    }

    let summary = order
        .iter()
        .map(|s| format!("{}={}", s, counts[s]))
        .collect::<Vec<_>>()
        .join(" ");
    events::log(
        paths,
        "INFO",
        &format!("Run complete. {}", if summary.is_empty() { "ok=0" } else { &summary }),
    );
    events::write_event(
        paths,
        "run-end",
        serde_json::json!({"summary": summary, "deferred": 0}),
    );
    summary
}
