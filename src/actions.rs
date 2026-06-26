//! Public mutations the CLI and GUI share (port of the run.ps1 "public mutations"
//! region + the CLI's pause/resume/conflicts): pull, unmap, forget, add-watch,
//! resync, restore, conflict listing, pause/resume, and partial-id resolution.

use crate::config::Config;
use crate::discovery::{discover, Project};
use crate::engine::{self, bisync, BisyncOpts};
use crate::paths::{self, Paths};
use crate::run::sync_project;
use crate::state::{self, Catalog, CatalogEntry, MachineState, Status};
use crate::{conflicts, events, filter, git};
use chrono::{NaiveDateTime, Utc};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

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

/// One filtered entry on the OneDrive (dest) side: a single junk file, or a whole
/// excluded directory (e.g. `node_modules/`) aggregated as a unit.
#[derive(Clone)]
pub struct CleanItem {
    pub path: PathBuf, // absolute, under dest
    pub rel: String,   // dest-relative, forward-slash
    pub is_dir: bool,
    pub bytes: u64,
    pub files: usize, // file count (1 for a file; recursive for a dir)
}

/// The result of scanning a project's OneDrive copy for filtered junk.
pub struct CleanScan {
    pub items: Vec<CleanItem>,
    pub total_bytes: u64,
    pub total_files: usize,
}

/// The set of paths that MUST be kept (never deleted) for a project. For a git
/// project this is the COMMITTED tree of the OneDrive copy itself (machine-
/// independent shared truth), unioned with the local working copy's tracked
/// files. Returns `None` for a git project whose committed set can't be read —
/// the caller must then refuse, never treat "unknown" as "delete everything".
fn keep_set(project: &Project) -> Option<HashSet<String>> {
    if !project.git {
        return Some(HashSet::new()); // watch/plain: nothing tracked to protect
    }
    let committed = git::committed_files(&project.dest)?; // None -> refuse
    if committed.is_empty() {
        return None; // a git project with no readable HEAD -> refuse
    }
    let mut keep: HashSet<String> = committed.into_iter().map(|s| s.replace('\\', "/")).collect();
    if project.local.join(".git").is_dir() {
        for t in git::ls_files(&project.local) {
            keep.insert(t.replace('\\', "/"));
        }
    }
    Some(keep)
}

/// Recursively sum (bytes, file_count) under a directory.
fn dir_size(dir: &Path) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut files = 0usize;
    for e in WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        if e.file_type().is_file() {
            files += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (bytes, files)
}

/// Scan a project's OneDrive copy for filtered files that shouldn't be there.
/// Whole excluded dirs are aggregated UNLESS they contain a kept (committed)
/// file. Refuses (Err) for a git project whose tracked set can't be established,
/// or if literally every file under the copy matches the filters (misconfig).
pub fn scan_dest_filtered(config: &Config, project: &Project) -> Result<CleanScan, String> {
    if !project.dest.exists() {
        return Err("No OneDrive copy found for this project.".into());
    }
    let keep = keep_set(project).ok_or_else(|| {
        "Refusing: can't read the committed file set from the OneDrive copy's .git (no HEAD). Pull or resync this project first, then retry.".to_string()
    })?;

    let mut items: Vec<CleanItem> = Vec::new();
    let (mut total_bytes, mut total_files, mut dest_files) = (0u64, 0usize, 0usize);

    let mut it = WalkDir::new(&project.dest).min_depth(1).into_iter();
    while let Some(entry) = it.next() {
        let Ok(entry) = entry else { continue };
        let Some(rel) = paths::rel_under(entry.path(), &project.dest).map(|r| r.replace('\\', "/")) else { continue };
        if rel == ".git" || rel.starts_with(".git/") {
            if entry.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        if entry.file_type().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if config.exclude_dirs.iter().any(|d| *d == name) {
                let prefix = format!("{rel}/");
                let has_kept = keep.iter().any(|k| k.starts_with(&prefix));
                if !has_kept {
                    let (b, f) = dir_size(entry.path());
                    total_bytes += b;
                    total_files += f;
                    dest_files += f;
                    items.push(CleanItem { path: entry.path().to_path_buf(), rel, is_dir: true, bytes: b, files: f });
                    it.skip_current_dir(); // counted as a unit; don't descend
                }
            }
            continue;
        }
        dest_files += 1;
        if filter::is_filtered_out(&rel, config, &keep) {
            let b = entry.metadata().map(|m| m.len()).unwrap_or(0);
            total_bytes += b;
            total_files += 1;
            items.push(CleanItem { path: entry.path().to_path_buf(), rel, is_dir: false, bytes: b, files: 1 });
        }
    }

    if total_files > 0 && total_files >= dest_files {
        return Err("Refusing: every file under the OneDrive copy matched the filters — that points to a misconfiguration, not junk. Check the exclude rules in Settings.".into());
    }
    Ok(CleanScan { items, total_bytes, total_files })
}

/// Delete the previously-scanned filtered items from the OneDrive copy. Holds the
/// per-project lock (so a scheduled bisync can't race), re-derives the keep-set
/// and RE-VERIFIES every item before removing it (defense in depth), and refuses
/// to touch a protected root. These files are excluded by the sync filter, so
/// their removal is invisible to bisync — no baseline reset is needed.
pub fn clean_scanned(paths: &Paths, config: &Config, project: &Project, items: &[CleanItem]) -> Result<(usize, u64), String> {
    let keep = keep_set(project).ok_or_else(|| "Refusing: the tracked set is no longer readable — aborting to avoid deleting committed files.".to_string())?;
    let lock = paths.bisync_dir().join(format!("{}.lock", engine::id_hash(&project.id)));
    let _g = crate::jsonio::lock_or_break(&lock, Duration::from_secs(600));

    let (mut deleted, mut freed) = (0usize, 0u64);
    for it in items {
        // Containment + safety re-checks against the live tree.
        if !it.path.starts_with(&project.dest) || it.rel == ".git" || it.rel.starts_with(".git/") {
            continue;
        }
        if paths.is_protected_root(&it.path) {
            continue;
        }
        if it.is_dir {
            let name = it.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let prefix = format!("{}/", it.rel);
            if !config.exclude_dirs.iter().any(|d| *d == name) || keep.iter().any(|k| k.starts_with(&prefix)) {
                continue; // dir no longer excluded, or now holds a kept file
            }
            if it.path.is_dir() {
                let (b, f) = dir_size(&it.path);
                if std::fs::remove_dir_all(&it.path).is_ok() {
                    deleted += f;
                    freed += b;
                }
            }
        } else {
            if !filter::is_filtered_out(&it.rel, config, &keep) {
                continue; // no longer junk (e.g. now tracked)
            }
            if it.path.is_file() {
                let b = it.path.metadata().map(|m| m.len()).unwrap_or(0);
                if std::fs::remove_file(&it.path).is_ok() {
                    deleted += 1;
                    freed += b;
                }
            }
        }
    }
    events::log(paths, "INFO", &format!("Cleaned {deleted} filtered file(s) ({freed} bytes) from the OneDrive copy of {}.", project.id));
    Ok((deleted, freed))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!("ods-test-{}-{tag}", std::process::id()));
        Paths { local_root: base.clone(), onedrive: base.clone(), user_profile: base }
    }

    #[test]
    fn delete_conflict_removes_only_conflict_named_files() {
        let p = temp_paths("dc");
        let dir = p.local_root.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let conflict = dir.join("app.conflict-PC-20260101T000000.txt");
        let ordinary = dir.join("app.txt");
        std::fs::write(&conflict, b"x").unwrap();
        std::fs::write(&ordinary, b"y").unwrap();

        // Refuses an ordinary file (guard), leaving it in place.
        assert!(delete_conflict(&p, &ordinary).is_err());
        assert!(ordinary.exists(), "ordinary file must be untouched");

        // Deletes a real rclone conflict copy.
        assert!(delete_conflict(&p, &conflict).is_ok());
        assert!(!conflict.exists());

        let _ = std::fs::remove_dir_all(&p.local_root);
    }

    fn proj(local: PathBuf, dest: PathBuf, git: bool) -> Project {
        Project { id: "Proj".into(), name: "Proj".into(), kind: crate::discovery::Kind::Mirror, local, dest, git }
    }

    #[test]
    fn clean_refuses_git_project_with_no_committed_set() {
        // THE safety test: a git project whose committed set can't be read must be
        // REFUSED — otherwise an empty tracked set would let the clean delete
        // committed files from the shared OneDrive copy.
        let p = temp_paths("clean-norefs");
        let dest = p.onedrive.join("Proj");
        std::fs::create_dir_all(dest.join("node_modules")).unwrap();
        std::fs::write(dest.join("node_modules/x.js"), b"x").unwrap();
        let project = proj(p.user_profile.join("absent-local"), dest.clone(), true);
        assert!(
            scan_dest_filtered(&Config::default(), &project).is_err(),
            "a git project with no readable HEAD on the dest copy must be refused"
        );
        let _ = std::fs::remove_dir_all(&p.onedrive);
    }

    #[test]
    fn scan_flags_junk_and_keeps_normal_for_non_git() {
        let p = temp_paths("clean-scan");
        let dest = p.onedrive.join("Plain");
        std::fs::create_dir_all(dest.join("node_modules")).unwrap();
        std::fs::write(dest.join("node_modules/x.js"), b"x").unwrap();
        std::fs::write(dest.join("debug.log"), b"l").unwrap();
        std::fs::write(dest.join("keep.txt"), b"k").unwrap();
        let project = proj(p.user_profile.join("local"), dest.clone(), false);
        let scan = scan_dest_filtered(&Config::default(), &project).expect("non-git scan should succeed");
        let rels: Vec<&str> = scan.items.iter().map(|i| i.rel.as_str()).collect();
        assert!(rels.contains(&"node_modules"), "the node_modules dir is flagged as a unit");
        assert!(rels.contains(&"debug.log"), "a *.log file is flagged");
        assert!(!rels.iter().any(|r| r.contains("keep.txt")), "an ordinary file is kept");
        let _ = std::fs::remove_dir_all(&p.onedrive);
    }

    #[test]
    fn restore_at_string_round_trips() {
        // archive_runs emits "%Y-%m-%d %H:%M:%S"; restore selects via parse_at on it,
        // so the two must agree on the format (the linchpin of GUI restore).
        let s = "2026-06-24 15:30:00";
        assert!(parse_at(s).is_some(), "restore must parse the timestamp archive_runs produces");
        // run_stamp parses the archive dir name; both must land on the same instant.
        let from_dir = run_stamp("20260624T153000Z").expect("run dir name parses");
        assert_eq!(from_dir.format("%Y-%m-%d %H:%M:%S").to_string(), s);
        assert_eq!(parse_at(s), Some(from_dir));
    }
}
