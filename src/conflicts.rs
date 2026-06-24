//! Conflict scan (port of Get-OdsConflicts) and git-divergence reconcile
//! (port of Resolve-OdsDivergence).

use crate::discovery::Project;
use crate::events;
use crate::git;
use crate::paths::Paths;
use chrono::Utc;
use std::path::PathBuf;
use walkdir::WalkDir;

/// rclone's conflict copies, named `conflict-<machine>-<yyyyMMddT...>` from any
/// machine. Anchored on the 8-digit date stamp so an ordinary file ending in
/// `-NAME.ext` is not mistaken for a conflict. The repo's `.git` is skipped.
pub fn scan(project: &Project) -> Vec<PathBuf> {
    if !project.local.exists() {
        return vec![];
    }
    let mut out = Vec::new();
    for entry in WalkDir::new(&project.local).into_iter().filter_entry(|e| {
        !(e.file_type().is_dir() && e.file_name().to_string_lossy() == ".git")
    }) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if is_conflict_name(&entry.file_name().to_string_lossy()) {
            out.push(entry.path().to_path_buf());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// True if `name` matches `conflict-<machine>-<8 digits>T…` (the rclone suffix).
fn is_conflict_name(name: &str) -> bool {
    let Some(cpos) = name.find("conflict-") else { return false };
    let bytes = name.as_bytes();
    // Look for `-DDDDDDDDT` at or after the end of "conflict-".
    let start = cpos + "conflict-".len();
    let mut i = start;
    while i + 9 < bytes.len() {
        if bytes[i] == b'-'
            && bytes[i + 1..i + 9].iter().all(|b| b.is_ascii_digit())
            && bytes[i + 9] == b'T'
        {
            return true;
        }
        i += 1;
    }
    false
}

/// If bisync surfaced a `.git` ref conflict, reconcile via git instead of accepting
/// it: fetch the dest tip and fast-forward/merge; on a real conflict, tag the other
/// tip as an orphan and abort so nothing is lost. Port of Resolve-OdsDivergence.
pub fn resolve_divergence(paths: &Paths, project: &Project) {
    if !project.git {
        return;
    }
    let refs_dir = project.local.join(".git").join("refs");
    let ref_conflicts: Vec<PathBuf> = WalkDir::new(&refs_dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.file_name().to_string_lossy().contains("conflict-"))
        .map(|e| e.path().to_path_buf())
        .collect();

    let branch = stdout_trim(&git::run(
        &project.local,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    ));
    if branch.is_empty() || ref_conflicts.is_empty() {
        return; // detached/unborn, or no divergence surfaced
    }

    events::log(paths, "WARN", &format!(
        "Divergence detected on {} branch '{branch}'; reconciling via git.", project.id));

    let dest = project.dest.display().to_string();
    let fetch = git::run(&project.local, &["fetch", &dest, &branch]);
    if fetch.code != 0 {
        events::log(paths, "ERROR", &format!(
            "Divergence reconcile aborted on {}: git fetch from dest failed (code {}).",
            project.id, fetch.code));
        events::write_event(paths, "divergence",
            serde_json::json!({"id": project.id, "result": "fetch-failed", "code": fetch.code}));
        return;
    }

    let merge = git::run(&project.local, &["merge", "--no-edit", "FETCH_HEAD"]);
    if merge.code != 0 {
        let tag = format!("ods-orphan-{}", Utc::now().format("%Y%m%d%H%M%S"));
        git::run(&project.local, &["tag", &tag, "FETCH_HEAD"]);
        git::run(&project.local, &["merge", "--abort"]);
        events::log(paths, "ERROR", &format!(
            "Auto-merge failed on {}; tagged other tip as {tag} for manual resolution.", project.id));
        events::write_event(paths, "divergence",
            serde_json::json!({"id": project.id, "result": "manual", "tag": tag}));
    } else {
        events::write_event(paths, "divergence",
            serde_json::json!({"id": project.id, "result": "merged"}));
    }
    for rc in ref_conflicts {
        let _ = std::fs::remove_file(rc);
    }
}

fn stdout_trim(o: &git::GitOut) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::is_conflict_name;

    #[test]
    fn matches_rclone_conflict_suffix() {
        assert!(is_conflict_name("app.conflict-DESKTOP01-20260624T101500Z.js"));
        assert!(is_conflict_name("notes.conflict-PC-20260101T000000.txt"));
    }

    #[test]
    fn ignores_ordinary_names() {
        assert!(!is_conflict_name("my-app-DESKTOP.js")); // no 8-digit stamp
        assert!(!is_conflict_name("conflict-resolver.rs")); // no stamp at all
        assert!(!is_conflict_name("readme.md"));
    }
}
