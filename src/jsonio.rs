//! Shared on-disk helpers that must round-trip with the PowerShell tool:
//! BOM-tolerant reads, atomic no-BOM writes (port of Write-OdsJson), and a
//! break-stale advisory file lock (port of the File.Open(CreateNew) pattern in
//! Edit-OdsMachineState / Invoke-OdsWithProjectLock).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Read a file as UTF-8 with any leading BOM stripped. `None` if missing or blank.
/// PowerShell 5.1's `Add-Content -Encoding utf8` prepends a BOM on file creation,
/// and a BOM makes serde_json reject the whole document — so every read of a file
/// either tool may have written goes through here.
pub fn read_bom(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.strip_prefix('\u{feff}').map(|x| x.to_string()).unwrap_or(s);
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Atomic write with no BOM: write a sibling temp, then rename over the target
/// (Windows std::fs::rename replaces the destination), then sweep stale temps.
/// Mirrors Write-OdsJson so a crash mid-write never leaves a half file in place.
pub fn write_atomic(path: &Path, content: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, content.as_bytes()).is_ok() {
        if std::fs::rename(&tmp, path).is_err() {
            // Fallback: best-effort copy then drop the temp.
            let _ = std::fs::copy(&tmp, path);
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    sweep_orphan_temps(path);
}

/// Remove `<leaf>.tmp.*` temps older than 2 minutes (crashed writers); never
/// touches a concurrent writer's in-flight temp.
fn sweep_orphan_temps(path: &Path) {
    let (Some(dir), Some(leaf)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!("{}.tmp.", leaf.to_string_lossy());
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|d| d.as_secs() > 120).unwrap_or(false))
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// An acquired advisory lock; removed on drop.
pub struct FileLock {
    path: PathBuf,
}
impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Acquire `lock_path` via create_new; on contention, spin until `timeout`, then
/// break the presumed-stale lock and proceed. Always returns a guard (best-effort,
/// matching Edit-OdsMachineState / Invoke-OdsWithProjectLock — never deadlocks).
pub fn lock_or_break(lock_path: &Path, timeout: Duration) -> FileLock {
    if let Some(dir) = lock_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let start = Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(_) => return FileLock { path: lock_path.to_path_buf() },
            Err(_) => {
                if start.elapsed() > timeout {
                    let _ = std::fs::remove_file(lock_path);
                    // One more attempt; if it still races, proceed unguarded.
                    if std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(lock_path)
                        .is_ok()
                    {
                        return FileLock { path: lock_path.to_path_buf() };
                    }
                    return FileLock { path: lock_path.to_path_buf() };
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        }
    }
}
