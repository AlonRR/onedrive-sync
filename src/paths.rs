//! Per-machine and shared paths, mirroring onedrive-sync-core.ps1.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Resolved roots for this machine. `local_root` is per-machine state under
/// %LOCALAPPDATA%; `onedrive` is the synced OneDrive folder; `user_profile`
/// is the mirror-law base for local project paths.
#[derive(Debug, Clone)]
pub struct Paths {
    pub local_root: PathBuf,
    pub onedrive: PathBuf,
    pub user_profile: PathBuf,
}

impl Paths {
    /// Resolve from the environment (as the PowerShell tool does).
    pub fn discover() -> Result<Self> {
        let local = std::env::var("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
        let onedrive = std::env::var("OneDriveConsumer")
            .context("$OneDriveConsumer is not set — start/sign-in the personal OneDrive client first")?;
        let user = std::env::var("USERPROFILE").context("USERPROFILE is not set")?;
        Ok(Self {
            local_root: PathBuf::from(local).join("onedrive-sync"),
            onedrive: PathBuf::from(onedrive),
            user_profile: PathBuf::from(user),
        })
    }

    pub fn machine_state(&self) -> PathBuf { self.local_root.join("machine-state.json") }
    pub fn events_dir(&self) -> PathBuf { self.local_root.join("events") }
    pub fn logs_dir(&self) -> PathBuf { self.local_root.join("logs") }
    pub fn log_file(&self) -> PathBuf { self.logs_dir().join("sync.log") }
    pub fn bisync_dir(&self) -> PathBuf { self.local_root.join("bisync") }
    pub fn versions_dir(&self) -> PathBuf { self.local_root.join("versions") }
    pub fn lock_file(&self) -> PathBuf { self.local_root.join(".lock") }
    pub fn pending(&self) -> PathBuf { self.local_root.join("pending.json") }
    pub fn paused_flag(&self) -> PathBuf { self.local_root.join("paused.flag") }
    pub fn config_file(&self) -> PathBuf { self.local_root.join("config.toml") }

    /// Shared catalog (tombstones + watch/plain mappings), lives in OneDrive.
    pub fn mappings(&self) -> PathBuf {
        self.onedrive.join("Tools").join("onedrive-sync").join("mappings.json")
    }

    /// Bundled rclone (installer drops it here); fall back to PATH if absent.
    pub fn rclone(&self) -> PathBuf { self.local_root.join("rclone.exe") }
}
