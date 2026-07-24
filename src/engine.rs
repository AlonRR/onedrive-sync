//! Bisync orchestration — the Rust port of Invoke-OdsBisync.
//! Generates the filter, resolves compare-mode and the delete-brake, builds the
//! rclone arguments, runs rclone under a watchdog, and records a bisync event.

use crate::config::Config;
use crate::discovery::Project;
use crate::paths::Paths;
use crate::state::MachineState;
use crate::{events, filter};
use chrono::Utc;
use md5::{Digest, Md5};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

/// Workdir/version key: first 16 hex chars of MD5(id.to_lowercase()).
pub fn id_hash(id: &str) -> String {
    let mut h = Md5::new();
    h.update(id.to_lowercase().as_bytes());
    h.finalize()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<String>()[..16]
        .to_string()
}

/// The local rclone override if one was placed (see `Paths::rclone`), else plain
/// `rclone` resolved off PATH — which is the normal case; the installer preflights
/// rclone onto PATH rather than dropping a copy in the state dir.
pub fn rclone_path(paths: &Paths) -> PathBuf {
    let overridden = paths.rclone();
    if overridden.exists() {
        overridden
    } else {
        PathBuf::from("rclone")
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BisyncOpts {
    pub resync: bool,
    pub dry_run: bool,
    pub force: bool,
    pub approve_deletes: bool,
}

/// Run one project's bisync; returns the rclone exit code (9 = watchdog kill).
pub fn bisync(
    paths: &Paths,
    config: &Config,
    state: &MachineState,
    project: &Project,
    opts: BisyncOpts,
) -> i32 {
    let idh = id_hash(&project.id);
    let wd = paths.bisync_dir().join(&idh);
    let _ = std::fs::create_dir_all(&wd);

    let filter_content = filter::generate(project, config);
    let filter_path = wd.join("filter.txt");
    let changed = write_if_changed(&filter_path, &filter_content);

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let backup = paths.versions_dir().join(&idh).join(&stamp);

    // Compare mode: per-project override, else config default.
    let mode = state
        .compare
        .get(&project.id)
        .cloned()
        .unwrap_or_else(|| config.compare_mode.clone());
    let compare = if mode == "checksum" { "size,checksum" } else { "size,modtime" };

    // Delete-brake: -ApproveDeletes (or config>=100) wins; else per-project override; else config.
    let max_delete = if opts.approve_deletes || config.max_delete_percent >= 100 {
        100
    } else if let Some(o) = state.max_delete.get(&project.id) {
        *o
    } else {
        config.max_delete_percent
    };

    let _ = std::fs::create_dir_all(&project.local);
    let _ = std::fs::create_dir_all(&project.dest);

    let do_resync = opts.resync || changed;
    let computer = std::env::var("COMPUTERNAME").unwrap_or_default();

    let mut args: Vec<String> = vec![
        "bisync".into(),
        project.local.display().to_string(),
        project.dest.display().to_string(),
        "--filters-file".into(),
        filter_path.display().to_string(),
        "--conflict-resolve".into(),
        "none".into(),
        "--conflict-suffix".into(),
        format!("conflict-{computer}-{stamp}"),
        "--backup-dir1".into(),
        backup.display().to_string(),
        "--max-delete".into(),
        max_delete.to_string(),
        "--transfers".into(),
        config.rclone_transfers.to_string(),
        "--compare".into(),
        compare.into(),
        "--resilient".into(),
        "--recover".into(),
        "--workdir".into(),
        wd.display().to_string(),
        "--log-file".into(),
        paths.log_file().display().to_string(),
        "--log-level".into(),
        "INFO".into(),
    ];
    if do_resync {
        args.extend(["--resync".into(), "--resync-mode".into(), "newer".into()]);
    }
    if opts.dry_run {
        args.push("--dry-run".into());
    }
    if opts.force {
        args.push("--force".into());
    }

    events::log(
        paths,
        "INFO",
        &format!(
            "bisync {} [{mode}]{}{}",
            project.id,
            if do_resync { " resync" } else { "" },
            if opts.dry_run { " dry-run" } else { "" }
        ),
    );

    let timeout = Duration::from_secs(config.run_time_budget.max(1800));
    // Serialize on THIS project's workdir so a manual sync and the scheduled run
    // (or the still-live PowerShell tool during shadow) can't collide on it. The
    // lock file sits beside the workdir, matching Invoke-OdsWithProjectLock.
    let lock_path = paths.bisync_dir().join(format!("{idh}.lock"));
    let _plock = crate::jsonio::lock_or_break(&lock_path, Duration::from_secs(600));
    // rclone writes its own ERROR/NOTICE lines into the shared --log-file (set
    // above) rather than stderr, so capturing "why" means tailing what it just
    // appended there — mark the offset before spawning, read only the new bytes.
    let log_start = std::fs::metadata(paths.log_file()).map(|m| m.len()).unwrap_or(0);
    let code = run_rclone(&rclone_path(paths), &args, timeout, paths, &project.id);
    drop(_plock);
    let reason = tail_error_lines(&paths.log_file(), log_start);

    events::write_event(
        paths,
        "bisync",
        serde_json::json!({"id": project.id, "code": code, "resync": do_resync, "dryrun": opts.dry_run, "error": reason}),
    );
    code
}

/// The ERROR-level lines rclone appended to the shared sync.log during this run
/// (bytes from `start_len` on), joined into a short human-readable reason for the
/// GUI's Attention badge. Capped so a chatty failure can't bloat the event log.
fn tail_error_lines(log_file: &Path, start_len: u64) -> String {
    let Ok(mut f) = std::fs::File::open(log_file) else { return String::new() };
    if f.seek(SeekFrom::Start(start_len)).is_err() {
        return String::new();
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return String::new();
    }
    let mut hits: Vec<&str> = buf.lines().filter(|l| l.to_uppercase().contains("ERROR")).collect();
    hits.truncate(6);
    let joined = hits.join(" | ");
    if joined.chars().count() > 500 {
        joined.chars().take(500).collect::<String>() + "…"
    } else {
        joined
    }
}

fn run_rclone(rclone: &Path, args: &[String], timeout: Duration, paths: &Paths, id: &str) -> i32 {
    let mut child = match Command::new(rclone)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            events::log(paths, "ERROR", &format!("failed to launch rclone: {e}"));
            return -1;
        }
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(-1),
            Ok(None) => {
                if start.elapsed() > timeout {
                    events::log(
                        paths,
                        "ERROR",
                        &format!("bisync {id} exceeded {}s — killing rclone.", timeout.as_secs()),
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return 9;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => return -1,
        }
    }
}

/// Workdir for a project id (bisync listing + filter), keyed by id-hash.
pub fn workdir(paths: &Paths, id: &str) -> PathBuf {
    paths.bisync_dir().join(id_hash(id))
}

/// True once bisync has established a baseline (it drops `*.lst` listings in the
/// workdir). Port of Test-OdsBaselineExists.
pub fn baseline_exists(paths: &Paths, id: &str) -> bool {
    let wd = workdir(paths, id);
    std::fs::read_dir(&wd)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .ends_with(".lst")
            })
        })
        .unwrap_or(false)
}

/// Drop a project's baseline so the next run does a clean --resync. Default removes
/// the whole workdir; `listing_only` keeps the filter but wipes the `*.lst` listings
/// (used after a corrupt-baseline integrity failure). Port of Reset-OdsBaseline.
pub fn reset_baseline(paths: &Paths, id: &str, listing_only: bool) {
    let wd = workdir(paths, id);
    if !wd.is_dir() {
        return;
    }
    if listing_only {
        if let Ok(rd) = std::fs::read_dir(&wd) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().to_lowercase().ends_with(".lst") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    } else {
        let _ = std::fs::remove_dir_all(&wd);
    }
}

/// Result of the pre-sync idle-stability gate.
pub struct Gate {
    pub ok: bool,
    pub reason: &'static str,
    pub transient: bool,
}

/// Pre-sync gate (port of Test-OdsGate): defer a project whose OneDrive side is
/// mid-write, or whose git is active, so we never bisync a half-written tree.
pub fn gate(project: &Project, config: &Config) -> Gate {
    let secs = config.idle_stability_seconds;
    if !tree_stable(&project.dest, secs) {
        return Gate { ok: false, reason: "onedrive-busy", transient: true };
    }
    if project.git && !git_quiesced(&project.local, secs) {
        return Gate { ok: false, reason: "git-active", transient: true };
    }
    Gate { ok: true, reason: "", transient: false }
}

/// True if nothing under `dir` was written in the last `stable_seconds`. Returns
/// early on the first too-recent file (matching Select-Object -First 1).
fn tree_stable(dir: &Path, stable_seconds: u64) -> bool {
    if !dir.exists() {
        return true;
    }
    !any_recent_file(dir, stable_seconds, false)
}

/// Like tree_stable but for a repo's `.git`: a present `*.lock` OR a recent write
/// means git is mid-operation. Port of Test-OdsGitQuiesced.
fn git_quiesced(repo_local: &Path, stable_seconds: u64) -> bool {
    let git_dir = repo_local.join(".git");
    if !git_dir.exists() {
        return true; // plain or unborn
    }
    !any_recent_file(&git_dir, stable_seconds, true)
}

/// Walk `dir`; return true on the first file modified within `stable_seconds`, or
/// (when `lock_counts`) on the first `*.lock` file. Bounded by early return.
fn any_recent_file(dir: &Path, stable_seconds: u64, lock_counts: bool) -> bool {
    for entry in WalkDir::new(dir).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        if lock_counts
            && entry.file_name().to_string_lossy().to_lowercase().ends_with(".lock")
        {
            return true;
        }
        let recent = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| t.elapsed().map(|e| e.as_secs() < stable_seconds).unwrap_or(false))
            .unwrap_or(false);
        if recent {
            return true;
        }
    }
    false
}

/// First-run seed (port of Invoke-OdsSeed): when both sides are non-empty and there
/// is no baseline yet, archive the OLDER copy of every differing file before the
/// initial --resync (which keeps the newer side) can overwrite it — newest wins,
/// nothing is lost.
pub fn seed(paths: &Paths, project: &Project) {
    let (local, dest) = (&project.local, &project.dest);
    if !local.exists() || !dest.exists() {
        return;
    }
    let local_files: Vec<PathBuf> = WalkDir::new(local)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();
    let dest_has_files = WalkDir::new(dest)
        .into_iter()
        .flatten()
        .any(|e| e.file_type().is_file());
    if local_files.is_empty() || !dest_has_files {
        return;
    }

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let archive = paths
        .versions_dir()
        .join(id_hash(&project.id))
        .join(format!("seed-{stamp}"));

    for lf in &local_files {
        let Ok(rel) = lf.strip_prefix(local) else { continue };
        let df = dest.join(rel);
        if !df.is_file() {
            continue;
        }
        let (Some(lh), Some(dh)) = (file_sha256(lf), file_sha256(&df)) else { continue };
        if lh == dh {
            continue; // identical — nothing to preserve
        }
        let lt = file_mtime(lf);
        let dt = file_mtime(&df);
        if let (Some(lt), Some(dt)) = (lt, dt) {
            let skew_hours = lt.duration_since(dt).or_else(|_| dt.duration_since(lt))
                .map(|d| d.as_secs() / 3600).unwrap_or(0);
            if skew_hours > 48 {
                events::log(paths, "WARN", &format!(
                    "Large mtime gap on '{}' during seed ({skew_hours}h) — check machine clocks.",
                    rel.display()));
            }
            // The loser is the OLDER file; --resync keeps the newer, so archive the older.
            let (loser, loser_root) = if lt >= dt { (df.as_path(), dest.as_path()) } else { (lf.as_path(), local.as_path()) };
            save_archive_copy(loser, loser_root, &archive);
        }
    }
    events::write_event(paths, "seed", serde_json::json!({"id": project.id}));
}

fn save_archive_copy(source: &Path, root: &Path, archive_dir: &Path) {
    let Ok(rel) = source.strip_prefix(root) else { return };
    let target = archive_dir.join(rel);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(source, &target);
}

fn file_sha256(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Write the filter only when it changed (trim-compared, BOM-tolerant). Returns
/// whether it changed, matching New-OdsFilterFile's change detection (which drives
/// whether the next bisync forces a --resync).
fn write_if_changed(path: &Path, content: &str) -> bool {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let existing = existing.strip_prefix('\u{feff}').unwrap_or(&existing);
    let changed = existing.trim_end() != content.trim_end();
    if changed {
        let _ = std::fs::write(path, content);
    }
    changed
}
