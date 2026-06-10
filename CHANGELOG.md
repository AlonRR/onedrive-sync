# Changelog

All notable changes to the OneDrive 2-way sync tool. The version here matches
`app-src\VERSION` (the stamp the self-updater reads).

## [0.1.0] — 2026-06-10

Initial rewrite from the one-way `robocopy /MIR` push to a 2-way, versioned,
selective, multi-machine sync built on `rclone bisync`.

### Added
- **2-way sync engine** (`rclone bisync`) with a git-aware per-repo filter:
  tracked files always sync; untracked files honor `.gitignore` plus a
  `$SyncAnywayList` allow-list (so `.env` and other secrets travel); volatile git
  metadata (`.git/index`, reflogs, locks) is excluded.
- **Project model:** a folder is a project iff it contains `.git`; recursive,
  exclude-pruned discovery finds repos at any depth without misdetecting a
  dependency's `.git` inside `node_modules`. Opt-in non-git `$PlainFolders`.
- **Mirroring law** `OneDrive\<rel>` ⇄ `%USERPROFILE%\<rel>`.
- **Selective, per-machine** sync via `machine-state.json`; shared `mappings.json`
  catalog with `forgotten` tombstones and conflict-copy merge.
- **Versioning:** local timestamped archive (`--backup-dir1`) pruned by age and
  size, plus OneDrive cloud history; layered `-Restore`.
- **Safety:** newest-wins first-run seed, idle + `.git`-quiesce gate with smart
  retry, `--max-delete` brake, post-sync `git fsck` verify, divergence guard
  (real `git merge` / orphan-tip tag), conflict scan with quarantine.
- **Surface:** shared core module (`onedrive-sync-core*.ps1`) driving an identical
  CLI (`onedrive-sync.ps1`) and tray + GUI (`onedrive-sync-tray.ps1`).
- **Install/update:** `install-task.ps1` auto-installs git/rclone, migrates the old
  `OneDriveCodeSync` task, stages scripts to a local app dir (run-local / sync-source
  split), and registers the sync task + tray. `$ToolUpdateMode` (`auto`/`notify`).
- **Observability:** structured per-run JSONL audit log + `-Diag` bundle;
  start-of-run reconcile pass.
- **Tests:** Pester v5 suite + dependency-free runner over the dangerous paths.

### Changed
- Replaced the old one-way `onedrive-sync.ps1` / `sync-config.ps1`; removed
  `migrate-from-onedrive.ps1` (superseded by selective pull + seed + `-Resync`).

### Notes
- Single-user, sequential-machine model: the tool does not engineer for concurrent
  multi-writer contention.
- Secrets sync to OneDrive's cloud by choice (no encryption). Git LFS not yet
  supported (detected + warned). Symlinks not synced.
