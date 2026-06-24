//! The scheduled/default run — the Rust port of Invoke-OdsRun: pause gate, run
//! lock, reconcile, undecided classification (auto-activate local mirrors, else
//! pending.json), the change-prioritized + time-budgeted active loop, the
//! per-project sync (gate -> seed -> bisync -> divergence -> integrity ->
//! conflict scan), smart-retry of transiently-gated repos, and version pruning.

use crate::config::Config;
use crate::discovery::{discover, Kind, Project};
use crate::engine::{self, bisync, BisyncOpts};
use crate::paths::Paths;
use crate::state::{self, Catalog, MachineState, Status};
use crate::{conflicts, events, git, prune};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOpts {
    pub dry_run: bool,
    pub ignore_pause: bool,
}

// ---------------------------------------------------------------------------
// Run lock (port of Enter-OdsLock): a {pid,ts} JSON file. A lock held by a live
// pid and younger than an hour blocks; otherwise it is broken and retried.
// ---------------------------------------------------------------------------
struct LockGuard {
    path: PathBuf,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_lock(paths: &Paths) -> Option<LockGuard> {
    let _ = std::fs::create_dir_all(&paths.local_root);
    let p = paths.lock_file();
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(mut f) => {
                let body = serde_json::json!({
                    "pid": std::process::id(),
                    "ts": Utc::now().to_rfc3339(),
                })
                .to_string();
                let _ = f.write_all(body.as_bytes());
                return Some(LockGuard { path: p });
            }
            Err(_) => {
                if holder_is_live(&p) {
                    events::log(paths, "WARN", "Another run holds the lock; exiting.");
                    return None;
                }
                let _ = std::fs::remove_file(&p); // break stale, retry
            }
        }
    }
    None
}

/// True if the existing lock is held by a still-running pid and is younger than an
/// hour. An unreadable/old/dead-pid lock returns false so the caller breaks it.
fn holder_is_live(lock: &PathBuf) -> bool {
    let Some(text) = crate::jsonio::read_bom(lock) else { return false };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    let age_ok = v
        .get("ts")
        .and_then(|t| t.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_minutes() < 60)
        .unwrap_or(false);
    if !age_ok {
        return false;
    }
    match v.get("pid").and_then(|p| p.as_u64()) {
        Some(pid) => pid_alive(pid as u32),
        None => false,
    }
}

/// Best-effort liveness check via tasklist; assume alive on failure so we never
/// break a lock that might still be held.
fn pid_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(true)
}

fn tally(order: &mut Vec<String>, counts: &mut HashMap<String, u32>, status: &str) {
    *counts.entry(status.to_string()).or_insert(0) += 1;
    if !order.iter().any(|s| s == status) {
        order.push(status.to_string());
    }
}

/// One project's full sync (port of Sync-OdsProject). Returns (status, transient)
/// where status is ok|warn|error|conflict|deferred.
pub fn sync_project(
    paths: &Paths,
    config: &Config,
    state: &MachineState,
    project: &Project,
    dry_run: bool,
    skip_gate: bool,
) -> (String, bool) {
    if !skip_gate && !dry_run {
        let g = engine::gate(project, config);
        if !g.ok {
            return ("deferred".into(), g.transient);
        }
    }

    let baseline = engine::baseline_exists(paths, &project.id);
    if !baseline && !dry_run {
        engine::seed(paths, project);
    }
    let resync = !baseline;

    let code = bisync(
        paths,
        config,
        state,
        project,
        BisyncOpts { resync, dry_run, ..Default::default() },
    );
    if resync && code >= 8 {
        return ("error".into(), false);
    }
    let mut status = if code == 0 {
        "ok"
    } else if code < 8 {
        "warn"
    } else {
        "error"
    }
    .to_string();

    if !dry_run && project.git {
        conflicts::resolve_divergence(paths, project);
        if git::integrity_ok(&project.local).is_err() {
            // Don't keep a corrupt baseline — wipe the listing so the next run resyncs.
            engine::reset_baseline(paths, &project.id, true);
            status = "error".into();
        }
    }

    let conflict_files = conflicts::scan(project);
    if !conflict_files.is_empty() {
        events::log(
            paths,
            "WARN",
            &format!("{}: {} conflict file(s) need attention.", project.id, conflict_files.len()),
        );
        status = "conflict".into();
    }
    (status, false)
}

/// Execute a run; returns the summary string (empty if paused/locked-out).
pub fn run(paths: &Paths, config: &Config, opts: RunOpts) -> String {
    if !opts.ignore_pause && paths.paused_flag().exists() {
        events::log(paths, "INFO", "Sync paused (paused.flag present); skipping run.");
        return String::new();
    }
    let _lock = match acquire_lock(paths) {
        Some(l) => l,
        None => return String::new(),
    };

    events::write_event(
        paths,
        "run-start",
        serde_json::json!({"dryrun": opts.dry_run, "interactive": false}),
    );

    let catalog = Catalog::load(paths);
    let projects = discover(paths, config, &catalog);

    // Reconcile: drop machine-state ids that no longer exist; warn on a vanished side.
    reconcile(paths, &projects);
    let state = MachineState::load(paths); // read after the reconcile prune

    // Classify undecided projects.
    let mut active_ids: Vec<String> = state.active.clone();
    let mut undecided: Vec<&Project> = Vec::new();
    for p in &projects {
        match state.status_of(&p.id) {
            Status::Active | Status::Skip => continue,
            Status::Undecided => {
                if p.local.exists() && p.kind == Kind::Mirror {
                    state::set_state(paths, &p.id, Status::Active);
                    active_ids.push(p.id.clone());
                    events::log(paths, "INFO", &format!("Auto-activated new local project {}.", p.id));
                } else {
                    undecided.push(p);
                }
            }
        }
    }
    write_pending(paths, &undecided);
    if !undecided.is_empty() {
        events::log(
            paths,
            "INFO",
            &format!("{} new project(s) available; awaiting decision (tray/discover).", undecided.len()),
        );
    }

    // Active projects, most-recently-changed first.
    let mut active: Vec<&Project> = projects
        .iter()
        .filter(|p| active_ids.iter().any(|a| a.eq_ignore_ascii_case(&p.id)))
        .collect();
    active.sort_by(|a, b| last_change(b).cmp(&last_change(a)));

    let deadline = Instant::now() + Duration::from_secs(config.run_time_budget);
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut deferred: Vec<&Project> = Vec::new();

    for p in &active {
        if Instant::now() > deadline {
            events::log(paths, "INFO", &format!("Time budget reached; carrying {} to next cycle.", p.id));
            deferred.push(p);
            continue;
        }
        // Vanished-side guard: never propagate a wholesale deletion.
        if !p.local.exists() || !p.dest.exists() {
            events::log(
                paths,
                "WARN",
                &format!("{}: a side is missing (keeping present side, not propagating deletion).", p.id),
            );
            continue;
        }
        let (status, transient) = sync_project(paths, config, &state, p, opts.dry_run, false);
        tally(&mut order, &mut counts, &status);
        if status == "deferred" && transient {
            deferred.push(p);
        }
    }

    // Smart-retry of transiently-gated repos with backoff.
    if !deferred.is_empty() && !opts.dry_run {
        let backoff = if config.retry_backoff.is_empty() {
            vec![5u64, 10, 20]
        } else {
            config.retry_backoff.clone()
        };
        let mut waited = 0u64;
        let mut attempt = 0u32;
        while !deferred.is_empty()
            && attempt < config.retry_max_attempts
            && waited < config.retry_max_wait_seconds
            && Instant::now() < deadline
        {
            let sleep = backoff[(attempt as usize).min(backoff.len() - 1)];
            std::thread::sleep(Duration::from_secs(sleep));
            waited += sleep;
            attempt += 1;
            let mut still: Vec<&Project> = Vec::new();
            for p in &deferred {
                let (status, _) = sync_project(paths, config, &state, p, false, false);
                if status == "deferred" {
                    still.push(p);
                } else {
                    tally(&mut order, &mut counts, &status);
                }
            }
            deferred = still;
        }
        for p in &deferred {
            state::update_defer_count(paths, &p.id, config.defer_escalate_cycles);
            events::log(paths, "INFO", &format!("Deferring {} to next cycle.", p.id));
        }
    }

    if !opts.dry_run {
        prune::version_prune(paths, config, false);
    }

    let summary = order
        .iter()
        .map(|s| format!("{}={}", s, counts[s]))
        .collect::<Vec<_>>()
        .join(" ");
    events::log(
        paths,
        "INFO",
        &format!("Run complete. {}", if summary.is_empty() { "(no active projects)" } else { &summary }),
    );
    events::write_event(
        paths,
        "run-end",
        serde_json::json!({"summary": summary, "deferred": deferred.len()}),
    );
    summary
}

/// Prune machine-state ids that no longer exist, and warn on a one-side-missing
/// active project (port of Invoke-OdsReconcile).
fn reconcile(paths: &Paths, projects: &[Project]) {
    let valid: std::collections::HashSet<String> =
        projects.iter().map(|p| p.id.to_lowercase()).collect();
    state::edit(paths, |s| {
        s.active.retain(|a| valid.contains(&a.to_lowercase()));
        s.skip.retain(|a| valid.contains(&a.to_lowercase()));
    });
    let state = MachineState::load(paths);
    for p in projects {
        if state.status_of(&p.id) == Status::Active {
            let local_gone = !p.local.exists();
            let dest_gone = !p.dest.exists();
            if local_gone ^ dest_gone {
                events::log(
                    paths,
                    "WARN",
                    &format!(
                        "Reconcile: {} has one side missing (local={} dest={}); will skip + keep present side.",
                        p.id, !local_gone, !dest_gone
                    ),
                );
            }
        }
    }
}

/// pending.json: the undecided set the tray/discover flow acts on.
fn write_pending(paths: &Paths, undecided: &[&Project]) {
    let list: Vec<serde_json::Value> = undecided
        .iter()
        .map(|p| serde_json::json!({"id": p.id, "name": p.name, "kind": p.kind.as_str()}))
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&list) {
        crate::jsonio::write_atomic(&paths.pending(), &json);
    }
}

/// Newest non-.git file mtime across both sides (port of Get-OdsLastChange), used
/// to prioritize the most-recently-touched projects within the time budget.
fn last_change(project: &Project) -> SystemTime {
    let mut latest = SystemTime::UNIX_EPOCH;
    for root in [&project.local, &project.dest] {
        if !root.exists() {
            continue;
        }
        for e in WalkDir::new(root).into_iter().flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            if e.path().components().any(|c| c.as_os_str() == ".git") {
                continue;
            }
            if let Some(t) = e.metadata().ok().and_then(|m| m.modified().ok()) {
                if t > latest {
                    latest = t;
                }
            }
        }
    }
    latest
}
