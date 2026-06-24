//! Bisync orchestration — the Rust port of Invoke-OdsBisync.
//! Generates the filter, resolves compare-mode and the delete-brake, builds the
//! rclone arguments, runs rclone under a watchdog, and records a bisync event.

use crate::config::Config;
use crate::discovery::Project;
use crate::paths::Paths;
use crate::state::MachineState;
use crate::{events, filter};
use chrono::Utc;
use md5::{Digest, Md5};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Workdir/version key: first 16 hex chars of MD5(id.to_lowercase()).
pub fn id_hash(id: &str) -> String {
    let mut h = Md5::new();
    h.update(id.to_lowercase().as_bytes());
    h.finalize()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<String>()[..16]
        .to_string()
}

/// Bundled rclone if present, else `rclone` on PATH.
pub fn rclone_path(paths: &Paths) -> PathBuf {
    let bundled = paths.rclone();
    if bundled.exists() {
        bundled
    } else {
        PathBuf::from("rclone")
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BisyncOpts {
    pub resync: bool,
    pub dry_run: bool,
    pub force: bool,
    pub approve_deletes: bool,
}

/// Run one project's bisync; returns the rclone exit code (9 = watchdog kill).
pub fn bisync(
    paths: &Paths,
    config: &Config,
    state: &MachineState,
    project: &Project,
    opts: BisyncOpts,
) -> i32 {
    let idh = id_hash(&project.id);
    let wd = paths.bisync_dir().join(&idh);
    let _ = std::fs::create_dir_all(&wd);

    let filter_content = filter::generate(project, config);
    let filter_path = wd.join("filter.txt");
    let changed = write_if_changed(&filter_path, &filter_content);

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let backup = paths.versions_dir().join(&idh).join(&stamp);

    // Compare mode: per-project override, else config default.
    let mode = state
        .compare
        .get(&project.id)
        .cloned()
        .unwrap_or_else(|| config.compare_mode.clone());
    let compare = if mode == "checksum" { "size,checksum" } else { "size,modtime" };

    // Delete-brake: -ApproveDeletes (or config>=100) wins; else per-project override; else config.
    let max_delete = if opts.approve_deletes || config.max_delete_percent >= 100 {
        100
    } else if let Some(o) = state.max_delete.get(&project.id) {
        *o
    } else {
        config.max_delete_percent
    };

    let _ = std::fs::create_dir_all(&project.local);
    let _ = std::fs::create_dir_all(&project.dest);

    let do_resync = opts.resync || changed;
    let computer = std::env::var("COMPUTERNAME").unwrap_or_default();

    let mut args: Vec<String> = vec![
        "bisync".into(),
        project.local.display().to_string(),
        project.dest.display().to_string(),
        "--filters-file".into(),
        filter_path.display().to_string(),
        "--conflict-resolve".into(),
        "none".into(),
        "--conflict-suffix".into(),
        format!("conflict-{computer}-{stamp}"),
        "--backup-dir1".into(),
        backup.display().to_string(),
        "--max-delete".into(),
        max_delete.to_string(),
        "--transfers".into(),
        config.rclone_transfers.to_string(),
        "--compare".into(),
        compare.into(),
        "--resilient".into(),
        "--recover".into(),
        "--workdir".into(),
        wd.display().to_string(),
        "--log-file".into(),
        paths.log_file().display().to_string(),
        "--log-level".into(),
        "INFO".into(),
    ];
    if do_resync {
        args.extend(["--resync".into(), "--resync-mode".into(), "newer".into()]);
    }
    if opts.dry_run {
        args.push("--dry-run".into());
    }
    if opts.force {
        args.push("--force".into());
    }

    events::log(
        paths,
        "INFO",
        &format!(
            "bisync {} [{mode}]{}{}",
            project.id,
            if do_resync { " resync" } else { "" },
            if opts.dry_run { " dry-run" } else { "" }
        ),
    );

    let timeout = Duration::from_secs(config.run_time_budget.max(1800));
    let code = run_rclone(&rclone_path(paths), &args, timeout, paths, &project.id);

    events::write_event(
        paths,
        "bisync",
        serde_json::json!({"id": project.id, "code": code, "resync": do_resync, "dryrun": opts.dry_run}),
    );
    code
}

fn run_rclone(rclone: &Path, args: &[String], timeout: Duration, paths: &Paths, id: &str) -> i32 {
    let mut child = match Command::new(rclone)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            events::log(paths, "ERROR", &format!("failed to launch rclone: {e}"));
            return -1;
        }
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(-1),
            Ok(None) => {
                if start.elapsed() > timeout {
                    events::log(
                        paths,
                        "ERROR",
                        &format!("bisync {id} exceeded {}s — killing rclone.", timeout.as_secs()),
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return 9;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => return -1,
        }
    }
}

/// Write the filter only when it changed (trim-compared, BOM-tolerant). Returns
/// whether it changed, matching New-OdsFilterFile's change detection (which drives
/// whether the next bisync forces a --resync).
fn write_if_changed(path: &Path, content: &str) -> bool {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let existing = existing.strip_prefix('\u{feff}').unwrap_or(&existing);
    let changed = existing.trim_end() != content.trim_end();
    if changed {
        let _ = std::fs::write(path, content);
    }
    changed
}
