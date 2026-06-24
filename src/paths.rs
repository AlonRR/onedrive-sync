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
    /// Cross-process lock guarding machine-state.json read-modify-write.
    pub fn state_lock(&self) -> PathBuf { self.local_root.join("machine-state.json.lock") }
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

/// `$Full` relative to `$Root` using '\', or `None` if not under it. `Some("")`
/// when `Full == Root` (port of Get-OdsRelUnder — the empty string is meaningful:
/// callers reject it so a project can't map onto a whole root).
pub fn rel_under(full: &std::path::Path, root: &std::path::Path) -> Option<String> {
    let f = normalize(full);
    let r = normalize(root);
    if f.eq_ignore_ascii_case(&r) {
        return Some(String::new());
    }
    let prefix = format!("{r}\\");
    if starts_with_ci(&f, &prefix) {
        Some(f[prefix.len()..].to_string())
    } else {
        None
    }
}

/// True if a == b, or one is nested in the other (port of Test-OdsOverlap).
pub fn paths_overlap(a: &std::path::Path, b: &std::path::Path) -> bool {
    let a = normalize(a);
    let b = normalize(b);
    a.eq_ignore_ascii_case(&b)
        || starts_with_ci(&a, &format!("{b}\\"))
        || starts_with_ci(&b, &format!("{a}\\"))
}

impl Paths {
    /// True if `path` is something we must never recursively delete or map onto: a
    /// drive root, the user profile / OneDrive / system roots, or an ancestor of any.
    /// Fails closed — an empty/unparseable path is treated as protected.
    pub fn is_protected_root(&self, path: &std::path::Path) -> bool {
        if path.as_os_str().to_string_lossy().trim().is_empty() {
            return true; // blank/whitespace fails closed
        }
        let full = normalize(path);
        if full.is_empty() {
            return true;
        }
        // Drive root (no parent component, e.g. "C:\").
        if std::path::Path::new(&full).parent().is_none() {
            return true;
        }
        let mut roots: Vec<String> = vec![normalize(&self.user_profile), normalize(&self.onedrive)];
        for v in ["PUBLIC", "ProgramData", "windir"] {
            if let Ok(p) = std::env::var(v) {
                roots.push(normalize(std::path::Path::new(&p)));
            }
        }
        for r in roots {
            if r.is_empty() {
                continue;
            }
            if full.eq_ignore_ascii_case(&r) || starts_with_ci(&r, &format!("{full}\\")) {
                return true;
            }
        }
        false
    }
}

fn normalize(p: &std::path::Path) -> String {
    // Lexical absolutize (like [IO.Path]::GetFullPath) — existence-independent, no
    // symlink resolution, so it behaves the same for a dest that doesn't exist yet.
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|c| c.join(p)).unwrap_or_else(|_| p.to_path_buf())
    };
    abs.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string()
}

fn starts_with_ci(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len()
        && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rel_under_basics() {
        let root = Path::new(r"C:\Users\me\OneDrive");
        assert_eq!(rel_under(Path::new(r"C:\Users\me\OneDrive\Projects\app"), root).as_deref(), Some(r"Projects\app"));
        // Equal to root -> empty string (callers must reject this).
        assert_eq!(rel_under(root, root).as_deref(), Some(""));
        // Not under root -> None.
        assert_eq!(rel_under(Path::new(r"C:\Other\x"), root), None);
        // A sibling that merely shares a prefix string is NOT under.
        assert_eq!(rel_under(Path::new(r"C:\Users\me\OneDriveX\y"), root), None);
    }

    #[test]
    fn overlap_detects_nesting_either_way() {
        let a = Path::new(r"C:\a\b");
        assert!(paths_overlap(a, Path::new(r"C:\a\b")));
        assert!(paths_overlap(a, Path::new(r"C:\a\b\c")));
        assert!(paths_overlap(a, Path::new(r"C:\a")));
        assert!(!paths_overlap(a, Path::new(r"C:\a\bb")));
    }

    #[test]
    fn protected_root_fails_closed() {
        let p = Paths {
            local_root: PathBuf::from(r"C:\Users\me\AppData\Local\onedrive-sync"),
            onedrive: PathBuf::from(r"C:\Users\me\OneDrive"),
            user_profile: PathBuf::from(r"C:\Users\me"),
        };
        assert!(p.is_protected_root(Path::new(r"C:\Users\me")), "the profile root itself");
        assert!(p.is_protected_root(Path::new(r"C:\Users\me\OneDrive")), "the onedrive root");
        assert!(p.is_protected_root(Path::new(r"C:\Users")), "an ancestor of a root");
        assert!(p.is_protected_root(Path::new(r"C:\")), "a drive root");
        assert!(p.is_protected_root(Path::new("   ")), "blank fails closed");
        // A real project folder under the profile is NOT protected.
        assert!(!p.is_protected_root(Path::new(r"C:\Users\me\Projects\app")));
    }
}
