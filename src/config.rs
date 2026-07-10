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

    /// OneDrive-idle gate window: a project whose dest/.git changed within this many
    /// seconds is deferred (transient) rather than synced mid-write.
    pub idle_stability_seconds: u64,
    /// Smart-retry of transiently-gated repos within a run.
    pub retry_max_attempts: u32,
    /// Backoff (seconds) between gated-repo retries; last value repeats.
    pub retry_backoff: Vec<u64>,
    /// Cap total backoff wait per run before deferring to the next cycle.
    pub retry_max_wait_seconds: u64,
    /// Escalate (log ERROR) a repo deferred this many consecutive cycles.
    pub defer_escalate_cycles: u32,

    /// GUI theme: "system" (follow the OS, default), "dark", or "light". Only the
    /// management window reads this; it has no effect on sync behaviour.
    pub theme: String,
    /// GUI layout: which side the selected-project detail panel docks to —
    /// "bottom" (default) or "right". Only the management window reads this.
    pub drawer_side: String,
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
            idle_stability_seconds: 60,
            retry_max_attempts: 4,
            retry_backoff: vec![5, 10, 20],
            retry_max_wait_seconds: 120,
            defer_escalate_cycles: 5,
            theme: "system".to_string(),
            drawer_side: "bottom".to_string(),
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

    /// Persist to config.toml (atomic, no BOM) so the GUI settings editor and the
    /// next load agree. Discovery picks the change up on the next refresh/run.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        crate::jsonio::write_atomic(&paths.config_file(), &text);
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The Settings editor serializes the whole Config to TOML. `plain_folders` is a
    /// TOML array-of-tables, which must appear AFTER all scalar keys — guard that a
    /// NON-EMPTY plain_folders still round-trips (an empty one hides the problem).
    #[test]
    fn config_with_plain_folders_round_trips_through_toml() {
        let mut c = Config::default();
        c.plain_folders = vec![
            PlainFolder { local: r"C:\Users\me\notes".into(), dest: r"C:\Users\me\OneDrive\notes".into() },
            PlainFolder { local: r"C:\Users\me\docs".into(), dest: r"C:\Users\me\OneDrive\docs".into() },
        ];
        c.max_delete_percent = 33;
        let text = toml::to_string_pretty(&c).expect("serialize Config to TOML");
        let back: Config = toml::from_str(&text).expect("re-parse Config from TOML");
        assert_eq!(back.plain_folders.len(), 2);
        assert_eq!(back.plain_folders[0].dest, r"C:\Users\me\OneDrive\notes");
        assert_eq!(back.max_delete_percent, 33);
        assert_eq!(back.project_parents, c.project_parents);
    }
}
