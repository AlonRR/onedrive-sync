//! Per-repo rclone filter generation — the Rust port of New-OdsFilterFile.
//! First-match-wins ordering, byte-identical to the PowerShell tool.

use crate::config::Config;
use crate::discovery::Project;

/// rclone filter paths use forward slashes.
pub fn convert_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// Escape rclone glob metacharacters so a git-derived path matches literally.
pub fn convert_literal(p: &str) -> String {
    let mut s = String::with_capacity(p.len());
    for c in p.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// PowerShell `-like`: case-insensitive wildcard match (`*`, `?`).
fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// A relative path is excluded if any of its directory segments is an excluded
/// dir, or its leaf matches an excluded-file pattern (port of Test-OdsMatchesExclude).
pub fn matches_exclude(rel: &str, exclude_dirs: &[String], exclude_files: &[String]) -> bool {
    let norm = rel.replace('\\', "/");
    let segs: Vec<&str> = norm.split('/').collect();
    if segs.len() >= 2 {
        for s in &segs[..segs.len() - 1] {
            if exclude_dirs.iter().any(|d| d == s) {
                return true;
            }
        }
    }
    let leaf = segs.last().copied().unwrap_or("");
    exclude_files.iter().any(|pat| glob_match(pat, leaf))
}

/// Whether a dest-relative path is FILTERED OUT of sync — i.e. junk that should
/// not live on OneDrive. A conservative mirror of `generate`'s exclude precedence
/// used by the "clean OneDrive" action:
/// - `.git` is never touched;
/// - anything in `keep` (the committed/tracked set) is KEPT (the filter force-
///   includes tracked files even when they match an exclude rule);
/// - a path under an excluded dir is filtered out (this wins over `sync_anyway`,
///   exactly as the generated filter orders it);
/// - otherwise an allow-listed leaf is kept, and an excluded-file glob is junk.
///
/// gitignore-derived excludes are intentionally NOT considered here (the clean
/// action under-deletes rather than risk over-deleting).
pub fn is_filtered_out(rel: &str, config: &Config, keep: &std::collections::HashSet<String>) -> bool {
    let norm = rel.replace('\\', "/");
    if norm == ".git" || norm.starts_with(".git/") {
        return false;
    }
    if keep.contains(&norm) {
        return false;
    }
    let segs: Vec<&str> = norm.split('/').collect();
    if segs.len() >= 2 && segs[..segs.len() - 1].iter().any(|s| config.exclude_dirs.iter().any(|d| d == s)) {
        return true;
    }
    let leaf = *segs.last().unwrap_or(&"");
    if config.sync_anyway.iter().any(|p| glob_match(p, leaf)) {
        return false;
    }
    config.exclude_files.iter().any(|p| glob_match(p, leaf))
}

/// Build the filter file content for a project (no trailing newline), matching
/// New-OdsFilterFile's ordering exactly.
pub fn generate(project: &Project, config: &Config) -> String {
    let mut lines: Vec<String> = Vec::new();
    let gitdir = project.local.join(".git").is_dir();

    if project.git {
        // 1) volatile / local-only git metadata
        for g in [
            "/.git/index",
            "/.git/logs/**",
            "/.git/FETCH_HEAD",
            "/.git/ORIG_HEAD",
            "/.git/COMMIT_EDITMSG",
            "/.git/**/*.lock",
            "/.git/index.lock",
            "/.git/*.tmp",
        ] {
            lines.push(format!("- {g}"));
        }
        // 2) sync the rest of git history
        lines.push("+ /.git/**".to_string());
        // 3) tracked files that WOULD be excluded -> always include
        if gitdir {
            for t in crate::git::ls_files(&project.local) {
                if matches_exclude(&t, &config.exclude_dirs, &config.exclude_files) {
                    lines.push(format!("+ /{}", convert_literal(&convert_path(&t))));
                }
            }
        }
    }

    // 4) excluded dirs (anywhere)
    for d in &config.exclude_dirs {
        lines.push(format!("- {d}/**"));
    }
    // 5) allow-list (after dir-excludes)
    for a in &config.sync_anyway {
        lines.push(format!("+ {a}"));
    }
    // 6) excluded file patterns
    for f in &config.exclude_files {
        lines.push(format!("- {f}"));
    }
    // 7) gitignore-derived excludes
    if project.git && gitdir {
        for p in crate::git::ls_ignored(&project.local) {
            let fp = convert_literal(&convert_path(&p));
            if fp.ends_with('/') {
                lines.push(format!("- /{fp}**"));
            } else {
                lines.push(format!("- /{fp}"));
            }
        }
    }

    // default: include everything else.
    lines.push("+ **".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn is_filtered_out_keeps_tracked_and_allow_listed_removes_junk() {
        let c = Config::default(); // node_modules/target in exclude_dirs, *.log in exclude_files
        let mut keep = HashSet::new();
        keep.insert("src/main.rs".to_string());
        keep.insert("build.log".to_string()); // a COMMITTED .log — must survive *.log

        // junk that shouldn't be on OneDrive:
        assert!(is_filtered_out("node_modules/react/index.js", &c, &keep));
        assert!(is_filtered_out("debug.log", &c, &keep));
        assert!(is_filtered_out("target/release/app.exe", &c, &keep));

        // must be KEPT:
        assert!(!is_filtered_out("src/main.rs", &c, &keep)); // tracked
        assert!(!is_filtered_out("build.log", &c, &keep)); // tracked, beats *.log
        assert!(!is_filtered_out(".env", &c, &keep)); // sync_anyway allow-list
        assert!(!is_filtered_out("README.md", &c, &keep)); // ordinary file
        assert!(!is_filtered_out(".git/config", &c, &keep)); // never touch .git
    }
}
