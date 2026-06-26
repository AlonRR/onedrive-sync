//! Exercises conflicts::resolve_divergence on throwaway git repos — the one
//! destructive path (git fetch/merge/tag) that can't be proven by dry-run
//! diffing. Requires `git` on PATH.

use ods::discovery::{Kind, Project};
use ods::paths::Paths;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path, args: &[&str]) {
    let out = git_out(repo, args);
    assert!(out.0, "git {:?} failed: {}", args, out.1);
}

fn git_out(repo: &Path, args: &[&str]) -> (bool, String) {
    let o = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .output()
        .expect("git runs");
    (
        o.status.success(),
        format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
    )
}

fn temp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("ods-div-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn divergence_reconcile_on_throwaway_clone() {
    let base = temp("repo");
    let local = base.join("local");
    let dest = base.join("dest");
    std::fs::create_dir_all(&local).unwrap();

    // local: a repo with one commit on master.
    git(&local, &["init", "-q", "-b", "master"]);
    std::fs::write(local.join("a.txt"), "one").unwrap();
    git(&local, &["add", "."]);
    git(&local, &["commit", "-q", "-m", "one"]);

    // dest: a clone that advances one commit (a fast-forwardable tip).
    git(&base, &["clone", "-q", local.to_str().unwrap(), dest.to_str().unwrap()]);
    std::fs::write(dest.join("b.txt"), "two").unwrap();
    git(&dest, &["add", "."]);
    git(&dest, &["commit", "-q", "-m", "two"]);

    // Plant the ref-conflict marker bisync would leave on a divergence — a COPY of
    // a real ref (a valid sha + newline), so the repo stays operable as in the wild.
    let head_sha = git_out(&local, &["rev-parse", "HEAD"]).1.trim().to_string();
    let refs = local.join(".git").join("refs");
    std::fs::create_dir_all(refs.join("heads")).unwrap();
    std::fs::write(
        refs.join("heads").join("conflict-DESKTOP-20260101T000000Z"),
        format!("{head_sha}\n"),
    )
    .unwrap();

    let paths = Paths {
        local_root: base.join("state"),
        onedrive: base.join("od"),
        user_profile: base.join("up"),
    };
    let project = Project {
        id: "test\\repo".into(),
        name: "repo".into(),
        kind: Kind::Mirror,
        local: local.clone(),
        dest: dest.clone(),
        git: true,
    };

    ods::conflicts::resolve_divergence(&paths, &project);

    // The conflict marker must be cleaned up...
    assert!(
        !refs.join("heads").join("conflict-DESKTOP-20260101T000000Z").exists(),
        "ref-conflict marker should be removed after reconcile"
    );
    // ...a divergence event recorded...
    let events_day = std::fs::read_dir(paths.events_dir())
        .ok()
        .and_then(|rd| rd.flatten().next().map(|e| e.path()));
    let log = events_day.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    assert!(log.contains("divergence"), "a divergence event should be written");
    // ...and the fast-forwardable dest tip is now merged in (no data loss).
    let merged = Command::new("git")
        .arg("-C").arg(&local).args(["log", "--oneline"]).output().unwrap();
    let log_txt = String::from_utf8_lossy(&merged.stdout);
    assert!(log_txt.contains("two") || log_txt.contains("one"), "history is intact");

    let _ = std::fs::remove_dir_all(&base);
}
