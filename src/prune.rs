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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::paths::Paths;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("ods-prune-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn mkrun(versions: &Path, proj: &str, run: &str, bytes: usize) {
        let d = versions.join(proj).join(run);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.bin"), vec![0u8; bytes]).unwrap();
    }

    /// With the size cap forced to 0, prune deletes everything it is ALLOWED to —
    /// but every project must keep at least its newest run (the last restore point).
    #[test]
    fn never_deletes_a_projects_last_run() {
        let root = temp_root("guard");
        let paths = Paths {
            local_root: root.clone(),
            onedrive: root.join("od"),
            user_profile: root.join("up"),
        };
        let versions = paths.versions_dir();
        for proj in ["A", "B"] {
            for run in ["20260101T000000Z", "20260201T000000Z", "20260301T000000Z"] {
                mkrun(&versions, proj, run, 1024);
            }
        }
        let mut config = Config::default();
        config.version_max_gb = 0; // force the size cap to bite
        config.version_retention_days = 36_500; // disable age expiry

        let deleted = version_prune(&paths, &config, false);
        assert!(!deleted.is_empty(), "size cap should have deleted something");
        for proj in ["A", "B"] {
            let remaining = super::subdirs(&versions.join(proj));
            assert_eq!(remaining.len(), 1, "project {proj} must keep exactly its newest run");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dry mode reports what it would delete but touches nothing on disk.
    #[test]
    fn dry_mode_deletes_nothing() {
        let root = temp_root("dry");
        let paths = Paths {
            local_root: root.clone(),
            onedrive: root.join("od"),
            user_profile: root.join("up"),
        };
        let versions = paths.versions_dir();
        for run in ["20260101T000000Z", "20260201T000000Z"] {
            mkrun(&versions, "A", run, 1024);
        }
        let mut config = Config::default();
        config.version_max_gb = 0;
        config.version_retention_days = 0; // everything is "old"

        let would = version_prune(&paths, &config, true);
        assert!(!would.is_empty());
        for run in would {
            assert!(run.exists(), "dry mode must not delete {}", run.display());
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
