//! Project discovery — the Rust port of Get-OdsProjects.
//!
//! - Mirror: .git repos found by recursing each project parent (exclude-pruned,
//!   stopping at the repo boundary). id = path relative to the OneDrive root.
//! - Watch: catalog (mappings.json) entries with kind == "watch".
//! - Plain: explicit Local<->Dest pairs from config.
//! Tombstoned (forgotten) ids are excluded.

use crate::config::Config;
use crate::paths::Paths;
use crate::state::Catalog;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mirror,
    Watch,
    Plain,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Mirror => "mirror",
            Kind::Watch => "watch",
            Kind::Plain => "plain",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub local: PathBuf,
    pub dest: PathBuf,
    pub git: bool,
}

/// Discover every known project (mirror + watch + plain), tombstones excluded.
pub fn discover(paths: &Paths, config: &Config, catalog: &Catalog) -> Vec<Project> {
    let mut out: Vec<Project> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let excludes: HashSet<&str> = config.exclude_dirs.iter().map(|s| s.as_str()).collect();

    // 1) Mirror projects: git repos under each parent.
    for parent in config.project_parent_paths(paths) {
        for repo in find_git_repos(&parent, &excludes) {
            let Some(id) = rel_under(&repo, &paths.onedrive) else {
                continue;
            };
            if catalog.is_forgotten(&id) || !seen.insert(id.to_lowercase()) {
                continue;
            }
            out.push(Project {
                name: leaf(&id),
                local: paths.user_profile.join(rel_to_pathbuf(&id)),
                dest: repo,
                kind: Kind::Mirror,
                git: true,
                id,
            });
        }
    }

    // 2) Watch projects: catalog entries.
    for e in &catalog.entries {
        if e.kind != "watch" || catalog.is_forgotten(&e.id) || !seen.insert(e.id.to_lowercase()) {
            continue;
        }
        let local = paths.user_profile.join(rel_to_pathbuf(&e.local_rel));
        let dest = paths.onedrive.join(rel_to_pathbuf(&e.dest_rel));
        let git = local.join(".git").is_dir();
        out.push(Project {
            name: leaf(&e.id),
            id: e.id.clone(),
            kind: Kind::Watch,
            local,
            dest,
            git,
        });
    }

    // 3) Plain folders: explicit pairs from config.
    for pf in &config.plain_folders {
        let dest = PathBuf::from(&pf.dest);
        let id = rel_under(&dest, &paths.onedrive).unwrap_or_else(|| pf.dest.clone());
        if catalog.is_forgotten(&id) || !seen.insert(id.to_lowercase()) {
            continue;
        }
        out.push(Project {
            name: leaf(&id),
            id,
            kind: Kind::Plain,
            local: PathBuf::from(&pf.local),
            dest,
            git: false,
        });
    }

    out.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    out
}

/// Recurse `root` for .git-bearing directories, pruning excluded dirs and never
/// descending into a repo (or into `.git`). Only real repos (a `.git` *dir*) count;
/// `.git`-file projects (submodule/worktree) are skipped, as in the PS tool.
fn find_git_repos(root: &Path, excludes: &HashSet<&str>) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    if !root.is_dir() {
        return repos;
    }
    let mut it = WalkDir::new(root).into_iter();
    while let Some(next) = it.next() {
        let entry = match next {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" || excludes.contains(name.as_str()) {
            it.skip_current_dir();
            continue;
        }
        if entry.path().join(".git").is_dir() {
            repos.push(entry.path().to_path_buf());
            it.skip_current_dir(); // do not descend into the repo
        }
    }
    repos
}

/// Path relative to `root`, joined with backslashes (the Windows id format).
fn rel_under(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\\"))
    }
}

fn rel_to_pathbuf(rel: &str) -> PathBuf {
    rel.split(['\\', '/']).collect()
}

fn leaf(id: &str) -> String {
    id.rsplit(['\\', '/']).next().unwrap_or(id).to_string()
}
