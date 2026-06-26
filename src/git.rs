//! Thin git wrapper. Unlike PowerShell, a subprocess writing to stderr does not
//! raise here, so we simply judge by the exit code and read stdout.

use std::path::Path;
use std::process::Command;

pub struct GitOut {
    pub code: i32,
    pub stdout: Vec<u8>,
}

pub fn run(repo: &Path, args: &[&str]) -> GitOut {
    match Command::new("git").arg("-C").arg(repo).args(args).output() {
        Ok(o) => GitOut {
            code: o.status.code().unwrap_or(-1),
            stdout: o.stdout,
        },
        Err(_) => GitOut {
            code: -1,
            stdout: Vec::new(),
        },
    }
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect()
}

/// Tracked files (NUL-separated), empty on error.
pub fn ls_files(repo: &Path) -> Vec<String> {
    let o = run(repo, &["ls-files", "-z"]);
    if o.code == 0 {
        split_nul(&o.stdout)
    } else {
        vec![]
    }
}

/// Ignored/untracked paths git would exclude (directories collapsed), NUL-separated.
pub fn ls_ignored(repo: &Path) -> Vec<String> {
    let o = run(
        repo,
        &["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"],
    );
    if o.code == 0 {
        split_nul(&o.stdout)
    } else {
        vec![]
    }
}

/// Repo health for sync safety (port of Test-OdsGitIntegrity). Only GENUINE object
/// corruption makes a repo unsafe to copy; a broken refs/remotes/*/HEAD or dangling
/// objects (normal gc churn) are tolerated. Returns Err(reason) only on real corruption.
pub fn integrity_ok(repo: &Path) -> Result<(), String> {
    if !repo.join(".git").exists() {
        return Ok(()); // not a git repo / nothing to check
    }
    // Unborn HEAD (no commits) is valid.
    if run(repo, &["rev-parse", "--verify", "--quiet", "HEAD"]).code != 0 {
        return Ok(());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["fsck", "--connectivity-only", "--no-dangling"])
        .output();
    let (code, text) = match out {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(_) => return Ok(()), // can't run fsck — don't block the sync
    };
    if code != 0 {
        let serious: Vec<&str> = text
            .lines()
            .filter(|l| {
                let ll = l.to_lowercase();
                (ll.contains("missing")
                    || ll.contains("broken link")
                    || ll.contains("corrupt")
                    || ll.contains("unable to read")
                    || ll.contains("bad object")
                    || ll.contains("bad tree")
                    || ll.contains("bad commit")
                    || ll.contains("sha1 mismatch"))
                    && !l.contains("refs/remotes/")
            })
            .collect();
        if !serious.is_empty() {
            return Err(serious.join("; ").trim().to_string());
        }
    }
    Ok(())
}
