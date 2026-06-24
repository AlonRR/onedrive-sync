//! Configuration, mirroring sync-config.ps1. Stored as TOML at
//! %LOCALAPPDATA%\onedrive-sync\config.toml; built-in defaults match the
//! effective deployed config so discovery is identical out of the box.

use crate::paths::Paths;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Parent folders under the OneDrive root whose .git children are mirror projects.
    pub project_parents: Vec<String>,
    /// Local folders (under %USERPROFILE%) watched for one-off projects.
    pub watch_roots: Vec<String>,
    /// Explicit non-git Local<->Dest folder pairs.
    pub plain_folders: Vec<PlainFolder>,

    pub exclude_dirs: Vec<String>,
    pub exclude_files: Vec<String>,
    /// Untracked/gitignored files that should still sync (secrets/config).
    pub sync_anyway: Vec<String>,

    /// bisync --max-delete brake (a PERCENT, default 25).
    pub max_delete_percent: u32,
    pub version_retention_days: u32,
    pub version_max_gb: u32,
    pub compare_mode: String,
    pub rclone_transfers: u32,
    pub run_time_budget: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlainFolder {
    pub local: String,
    pub dest: String,
}

impl Default for Config {
    fn default() -> Self {
        let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        Self {
            project_parents: s(&["Projects", "Micro_controller"]),
            watch_roots: s(&["Code"]),
            plain_folders: vec![],
            exclude_dirs: s(&[
                "node_modules", ".pnpm-store", ".yarn", "dist", "build", "out", "target",
                "bin", "obj", ".next", ".nuxt", ".svelte-kit", ".output", "coverage",
                "__pycache__", ".venv", "venv", ".pytest_cache", ".mypy_cache",
                ".ruff_cache", ".tox", ".ipynb_checkpoints", "vendor", ".cache",
                ".parcel-cache", ".idea", ".vs", "logs", "tmp", "temp",
            ]),
            exclude_files: s(&[
                "*.pyc", "*.pyo", "*.pyd", ".DS_Store", "Thumbs.db", "desktop.ini",
                "*.swp", "*.swo", "*~", "*.tsbuildinfo", "*.suo", "*.user",
                "*.eslintcache", "*.tmp", "*.temp", "*.log",
            ]),
            sync_anyway: s(&[
                ".env", "*.env", ".env.*", "*.local", "*.pem", "*.key", "*.p12", "*.pfx",
            ]),
            max_delete_percent: 25,
            version_retention_days: 30,
            version_max_gb: 5,
            compare_mode: "modtime".to_string(),
            rclone_transfers: 4,
            run_time_budget: 1500,
        }
    }
}

impl Config {
    /// Load config.toml if present, else the built-in defaults.
    pub fn load(paths: &Paths) -> Result<Self> {
        let f = paths.config_file();
        if f.exists() {
            let text = std::fs::read_to_string(&f)?;
            Ok(toml::from_str(&text)?)
        } else {
            Ok(Self::default())
        }
    }

    /// Project parents resolved to absolute paths under the OneDrive root.
    pub fn project_parent_paths(&self, paths: &Paths) -> Vec<PathBuf> {
        self.project_parents.iter().map(|r| paths.onedrive.join(r)).collect()
    }

    /// Watch roots resolved to absolute paths under %USERPROFILE%.
    pub fn watch_root_paths(&self, paths: &Paths) -> Vec<PathBuf> {
        self.watch_roots.iter().map(|r| paths.user_profile.join(r)).collect()
    }
}
