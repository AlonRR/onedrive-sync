//! Public mutations the CLI and GUI share (port of the run.ps1 "public mutations"
//! region + the CLI's pause/resume/conflicts): pull, unmap, forget, add-watch,
//! resync, restore, conflict listing, pause/resume, and partial-id resolution.

use crate::config::Config;
use crate::discovery::{discover, Project};
use crate::engine::{self, bisync, BisyncOpts};
use crate::paths::{self, Paths};
use crate::run::sync_project;
use crate::state::{self, Catalog, CatalogEntry, MachineState, Status};
use crate::{conflicts, events};
use chrono::{NaiveDateTime, Utc};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn projects(paths: &Paths, config: &Config) -> Vec<Project> {
    discover(paths, config, &Catalog::load(paths))
}

fn find<'a>(list: &'a [Project], id: &str) -> Option<&'a Project> {
    list.iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

/// Resolve a (possibly partial) id (port of Resolve-OdsId): an exact id wins; a
/// partial matching exactly one resolves to it; a partial matching several is
/// ambiguous (error if `destructive`, else the first match).
pub fn resolve_id(list: &[Project], partial: &str, destructive: bool) -> Result<String, String> {
    if partial.is_empty() {
        return Ok(partial.to_string());
    }
    if let Some(p) = list.iter().find(|p| p.id.eq_ignore_ascii_case(partial)) {
        return Ok(p.id.clone());
    }
    let lc = partial.to_lowercase();
    let hits: Vec<&Project> = list.iter().filter(|p| p.id.to_lowercase().contains(&lc)).collect();
    match hits.len() {
        1 => Ok(hits[0].id.clone()),
        0 => Ok(partial.to_string()),
        _ if destructive => Err(format!(
            "'{partial}' is ambiguous — matches {} projects: {}. Use the exact id.",
            hits.len(),
            hits.iter().map(|p| p.id.clone()).collect::<Vec<_>>().join(", ")
        )),
        _ => Ok(hits[0].id.clone()),
    }
}

/// Pull a project local here: clear any tombstone, mark active, sync it.
pub fn pull(paths: &Paths, config: &Config, id: &str) -> Result<String, String> {
    let mut cat = Catalog::load(paths);
    if cat.is_forgotten(id) {
        cat.forgotten.retain(|f| !f.eq_ignore_ascii_case(id));
        cat.save(paths);
    }
    state::set_state(paths, id, Status::Active);
    let list = projects(paths, config);
    let p = find(&list, id).ok_or_else(|| format!("Project '{id}' not found in available set."))?;
    let st = MachineState::load(paths);
    let (status, _) = sync_project(paths, config, &st, p, false, false);
    Ok(status)
}

/// Stop syncing a project here (mark skip, drop the baseline). `delete_local` also
/// removes the local copy — refused on a protected root.
pub fn unmap(paths: &Paths, config: &Config, id: &str, delete_local: bool) -> Result<(), String> {
    let list = projects(paths, config);
    let proj = find(&list, id).cloned();
    state::set_state(paths, id, Status::Skip);
    {
        let lock = paths.bisync_dir().join(format!("{}.lock", engine::id_hash(id)));
        let _g = crate::jsonio::lock_or_break(&lock, Duration::from_secs(600));
        engine::reset_baseline(paths, id, false);
    }
    if delete_local {
        if let Some(p) = &proj {
            if p.local.exists() {
                if paths.is_protected_root(&p.local) {
                    return Err(format!(
                        "Refusing --delete-local for '{id}': '{}' is (or contains) a protected root.",
                        p.local.display()
                    ));
                }
                std::fs::remove_dir_all(&p.local).map_err(|e| e.to_string())?;
                events::log(paths, "INFO", &format!("Removed local copy of {id} (OneDrive copy preserved)."));
            }
        }
    }
    events::log(paths, "INFO", &format!("Unmapped {id} on this machine (skip). OneDrive copy intact."));
    Ok(())
}

/// Retire a project globally (tombstone). OneDrive copy untouched; reversible with pull.
pub fn forget(paths: &Paths, id: &str) {
    let mut cat = Catalog::load(paths);
    cat.entries.retain(|e| !e.id.eq_ignore_ascii_case(id));
    if !cat.is_forgotten(id) {
        cat.forgotten.push(id.to_string());
    }
    cat.save(paths);
    state::edit(paths, |s| {
        s.active.retain(|a| !a.eq_ignore_ascii_case(id));
        s.skip.retain(|a| !a.eq_ignore_ascii_case(id));
    });
    events::log(paths, "INFO", &format!("Forgot {id} (tombstoned). OneDrive copy untouched; use pull to revive."));
}

/// Persist a new watch mapping (port of Add-OdsWatchMapping); returns the new id.
pub fn add_watch(paths: &Paths, local: &Path, dest: &Path) -> Result<String, String> {
    let dest_rel = paths::rel_under(dest, &paths.onedrive)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Destination must be a folder UNDER OneDrive ({}).", paths.onedrive.display()))?;
    let local_rel = paths::rel_under(local, &paths.user_profile)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Local folder must be a folder UNDER your profile ({}).", paths.user_profile.display()))?;
    if paths.is_protected_root(local) || paths.is_protected_root(dest) {
        return Err("Local or Dest is a protected root — refused.".into());
    }
    if paths::paths_overlap(local, dest) {
        return Err("Local and Dest overlap — refused (would self-sync).".into());
    }
    let mut cat = Catalog::load(paths);
    cat.entries.retain(|e| !e.id.eq_ignore_ascii_case(&dest_rel));
    cat.entries.push(CatalogEntry {
        id: dest_rel.clone(),
        local_rel,
        dest_rel: dest_rel.clone(),
        kind: "watch".into(),
        extra: Default::default(),
    });
    cat.save(paths);
    state::set_state(paths, &dest_rel, Status::Active);
    events::log(paths, "INFO", &format!("Mapped watch project -> {dest_rel}."));
    Ok(dest_rel)
}

/// Force a fresh bisync baseline for one project, or every active project.
pub fn resync(paths: &Paths, config: &Config, id: Option<&str>) {
    let list = projects(paths, config);
    let st = MachineState::load(paths);
    let targets: Vec<&Project> = match id {
        Some(want) => find(&list, want).into_iter().collect(),
        None => list.iter().filter(|p| st.status_of(&p.id) == Status::Active).collect(),
    };
    for p in targets {
        bisync(paths, config, &st, p, BisyncOpts { resync: true, ..Default::default() });
    }
}

/// All unresolved conflict files, grouped by project (only projects that have any).
pub fn list_conflicts(paths: &Paths, config: &Config) -> Vec<(String, Vec<PathBuf>)> {
    let mut out = Vec::new();
    for p in projects(paths, config) {
        if !p.local.exists() {
            continue;
        }
        let c = conflicts::scan(&p);
        if !c.is_empty() {
            out.push((p.id.clone(), c));
        }
    }
    out
}

/// Delete one rclone conflict copy. Guarded: the path must still be a file and its
/// name must match the conflict-copy pattern, so an ordinary file can't be removed.
pub fn delete_conflict(paths: &Paths, file: &Path) -> Result<(), String> {
    if !file.is_file() {
        return Err("Not a file (already gone?).".into());
    }
    let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if !conflicts::is_conflict_name(&name) {
        return Err("Refusing: not a recognised conflict-copy filename.".into());
    }
    std::fs::remove_file(file).map_err(|e| e.to_string())?;
    events::log(paths, "INFO", &format!("Deleted conflict copy {}", file.display()));
    Ok(())
}

/// Write a diagnostic bundle (config + machine-state + recent log tail) to %TEMP%
/// and return its path. NOT redacted — contains full paths and log lines.
pub fn diag(paths: &Paths, config: &Config) -> Result<PathBuf, String> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let temp = std::env::var("TEMP").unwrap_or_else(|_| ".".into());
    let bundle = Path::new(&temp).join(format!("ods-diag-{stamp}.txt"));
    let mut out = String::new();
    out.push_str("# ods diagnostics — NOT redacted; contains full paths and recent log lines.\n");
    out.push_str("=== config ===\n");
    out.push_str(&serde_json::to_string_pretty(config).unwrap_or_default());
    out.push_str("\n=== machine-state ===\n");
    out.push_str(&serde_json::to_string_pretty(&MachineState::load(paths)).unwrap_or_default());
    out.push_str("\n=== recent log ===\n");
    if let Ok(text) = std::fs::read_to_string(paths.log_file()) {
        let tail: Vec<&str> = text.lines().rev().take(200).collect();
        for l in tail.iter().rev() {
            out.push_str(l);
            out.push('\n');
        }
    }
    std::fs::write(&bundle, out).map_err(|e| e.to_string())?;
    Ok(bundle)
}

/// Pause the scheduled sync (the flag file the run loop honors; authoritative and
/// elevation-free). Best-effort also disables the task so it doesn't even spawn.
pub fn pause(paths: &Paths) {
    crate::jsonio::write_atomic(&paths.paused_flag(), &Utc::now().to_rfc3339());
    best_effort_task("/Disable");
    events::log(paths, "INFO", "Scheduled sync paused (runs skip until resume).");
}

pub fn resume(paths: &Paths) {
    let _ = std::fs::remove_file(paths.paused_flag());
    best_effort_task("/Enable");
    events::log(paths, "INFO", "Scheduled sync resumed.");
}

pub fn is_paused(paths: &Paths) -> bool {
    paths.paused_flag().exists()
}

fn best_effort_task(change: &str) {
    let _ = std::process::Command::new("schtasks")
        .args(["/Change", "/TN", "ods-sync", change])
        .output();
}

// ---------------------------------------------------------------------------
// Restore from the local version archive (port of Restore-OdsItem).
// ---------------------------------------------------------------------------

/// Parse the UTC stamp from an archive run-dir name (`yyyyMMddTHHmmssZ`, optionally
/// `seed-`/`pre-restore-` prefixed). Unparseable -> None.
fn run_stamp(name: &str) -> Option<NaiveDateTime> {
    let core = name
        .strip_prefix("seed-")
        .or_else(|| name.strip_prefix("pre-restore-"))
        .unwrap_or(name);
    NaiveDateTime::parse_from_str(core, "%Y%m%dT%H%M%SZ").ok()
}

fn parse_at(at: &str) -> Option<NaiveDateTime> {
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(at, fmt) {
            return Some(dt);
        }
    }
    // Date only -> end of that day, so "--at 2026-06-01" includes that day's runs.
    chrono::NaiveDate::parse_from_str(at, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(23, 59, 59))
}

/// One restorable run from the local version archive.
#[derive(Clone)]
pub struct ArchiveRun {
    /// A parse_at-compatible timestamp ("YYYY-MM-DD HH:MM:SS") to pass to `restore`.
    pub at: String,
    /// Human label for the list (with a "(seed)" marker for the initial seed run).
    pub label: String,
}

/// List restorable archive runs for a project, newest first (excludes pre-restore
/// backups). Empty if the project has no local versions yet.
pub fn archive_runs(paths: &Paths, id: &str) -> Vec<ArchiveRun> {
    let base = paths.versions_dir().join(engine::id_hash(id));
    let Ok(rd) = std::fs::read_dir(&base) else { return vec![] };
    let mut runs: Vec<(NaiveDateTime, bool)> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with("pre-restore-"))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            run_stamp(&name).map(|s| (s, name.starts_with("seed-")))
        })
        .collect();
    runs.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    runs.into_iter()
        .map(|(dt, seed)| ArchiveRun {
            at: dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            label: format!("{}{}", dt.format("%Y-%m-%d %H:%M"), if seed { "  (seed)" } else { "" }),
        })
        .collect()
}

pub fn restore(
    paths: &Paths,
    config: &Config,
    id: &str,
    at: Option<&str>,
    file: Option<&str>,
) -> Result<(), String> {
    if let Some(f) = file {
        if f.split(['\\', '/']).any(|seg| seg == "..") {
            return Err(format!("Invalid file '{f}' ('..' is not allowed)."));
        }
    }
    let base = paths.versions_dir().join(engine::id_hash(id));
    if !base.is_dir() {
        return Err(format!("No local versions for '{id}'. Try OneDrive version history."));
    }
    let mut runs: Vec<(PathBuf, NaiveDateTime)> = std::fs::read_dir(&base)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with("pre-restore-"))
        .filter_map(|e| run_stamp(&e.file_name().to_string_lossy()).map(|s| (e.path(), s)))
        .collect();
    runs.sort_by(|a, b| b.1.cmp(&a.1)); // newest first

    if let Some(at) = at {
        let at_dt = parse_at(at).ok_or_else(|| format!("Could not parse --at '{at}' as a date/time."))?;
        runs.retain(|(_, s)| *s <= at_dt);
    }
    let (run, run_name) = runs
        .first()
        .map(|(p, _)| (p.clone(), p.file_name().unwrap().to_string_lossy().to_string()))
        .ok_or_else(|| "No archived version at/before that time.".to_string())?;

    let list = projects(paths, config);
    let proj = find(&list, id).ok_or_else(|| format!("Project '{id}' not found."))?;
    let src = match file {
        Some(f) => run.join(f),
        None => run.clone(),
    };
    if !src.exists() {
        return Err(format!("Not in archive run {run_name}: {}", file.unwrap_or("(whole)")));
    }
    let dst = match file {
        Some(f) => proj.local.join(f),
        None => proj.local.clone(),
    };

    // Back up current content before clobbering, so a wrong restore is undoable.
    if dst.exists() {
        let pre = base.join(format!("pre-restore-{}", Utc::now().format("%Y%m%dT%H%M%SZ")));
        let pre_target = match file {
            Some(f) => pre.join(f),
            None => pre.clone(),
        };
        if let Err(e) = copy_path(&dst, &pre_target) {
            events::log(paths, "WARN", &format!("Pre-restore backup of '{id}' failed: {e}. Proceeding."));
        } else {
            events::log(paths, "INFO", &format!("Backed up current '{id}' before restore."));
        }
    }

    events::log(paths, "INFO", &format!("Restoring {id} {} from {run_name}.", file.unwrap_or("(whole)")));
    copy_path(&src, &dst).map_err(|e| e.to_string())?;
    events::write_event(paths, "restore", serde_json::json!({"id": id, "run": run_name, "file": file}));
    Ok(())
}

/// Copy a file or a directory tree from `src` to `dst`.
fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(src)?;
    if meta.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(src).into_iter().flatten() {
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
