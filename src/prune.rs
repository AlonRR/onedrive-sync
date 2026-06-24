//! Version-archive pruning (port of Invoke-OdsVersionPrune): age-based, then a
//! size cap (oldest first) — but NEVER a project's newest run, so every project
//! keeps at least one restore point. `dry` returns what would be deleted without
//! touching disk (used to prove parity before letting it delete for real).

use crate::config::Config;
use crate::paths::Paths;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

pub fn version_prune(paths: &Paths, config: &Config, dry: bool) -> Vec<PathBuf> {
    let root = paths.versions_dir();
    if !root.is_dir() {
        return vec![];
    }
    let proj_dirs: Vec<PathBuf> = subdirs(&root);

    // Keep each project's newest run (by mtime) — honored by both passes.
    let mut keep: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for d in &proj_dirs {
        if let Some(newest) = subdirs(d)
            .into_iter()
            .max_by_key(|r| mtime(r).unwrap_or(SystemTime::UNIX_EPOCH))
        {
            keep.insert(newest);
        }
    }

    let mut deleted = Vec::new();
    let retention = Duration::from_secs(config.version_retention_days as u64 * 86_400);

    // Age-based pass.
    for d in &proj_dirs {
        for run in subdirs(d) {
            if keep.contains(&run) {
                continue;
            }
            let old = mtime(&run)
                .and_then(|t| t.elapsed().ok())
                .map(|e| e > retention)
                .unwrap_or(false);
            if old {
                remove(&run, dry);
                deleted.push(run);
            }
        }
    }

    // Size-cap pass (oldest first), still never a project's newest run.
    let mut survivors: Vec<PathBuf> = proj_dirs
        .iter()
        .flat_map(|d| subdirs(d))
        .filter(|r| !keep.contains(r) && !deleted.contains(r))
        .collect();
    survivors.sort_by_key(|r| mtime(r).unwrap_or(SystemTime::UNIX_EPOCH));

    let max_bytes = config.version_max_gb as u64 * 1024 * 1024 * 1024;
    let mut total: u64 = survivors.iter().map(|r| dir_size(r)).sum();
    let mut i = 0;
    while total > max_bytes && i < survivors.len() {
        let sz = dir_size(&survivors[i]);
        remove(&survivors[i], dry);
        deleted.push(survivors[i].clone());
        total = total.saturating_sub(sz);
        i += 1;
    }
    deleted
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn dir_size(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}

fn remove(run: &Path, dry: bool) {
    if !dry {
        let _ = std::fs::remove_dir_all(run);
    }
}
