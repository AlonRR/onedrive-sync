# Changelog

All notable changes to onedrive-sync (`ods`). This repo continues the original
PowerShell project; the `v1.0-powershell` tag marks the last PowerShell release,
recorded under `[0.1.0]` below.

## [Unreleased] — Rust rewrite (`ods`)

A from-scratch Rust port of the PowerShell tool into one ~10 MB native binary
(engine + CLI + management GUI + tray). `rclone` and `git` stay subprocesses. No
PowerShell host (dynamic-scope crash class gone), no WPF (re-entrancy crash class
gone), no per-run script re-parse.

### Added
- **Engine / CLI / GUI parity** with the PowerShell tool, validated on real data:
  state writes round-trip with the old on-disk format, byte-identical filter
  generation, identical discovery set and dry-run summary counts (see `MIGRATION.md`).
- **Native GUI (egui/eframe) + system tray:** Projects grid (last-sync and conflict
  columns), per-project settings, Pending / Retired tabs, conflict viewer, Pause toggle.
- **Accessibility:** Segoe UI fonts, measured WCAG-AA light and dark themes, keyboard
  ops (F5 / Esc / Ctrl `+` `-` `0` zoom), a visible focus ring, and AccessKit.
- **"Add folders" page:** scan a parent folder for projects (discovery roots) and map a
  single folder — both in one place.
- **"Clean OneDrive of filtered files":** removes junk (e.g. `node_modules`) that leaked
  onto OneDrive, enforcing "filtered files don't live on OneDrive." The keep-set is the
  destination's own committed tree and the action refuses if that is unreadable. CLI:
  `ods clean <id> [--yes]`.

### Changed
- The single binary updates by swapping the `.exe`; the PowerShell self-updater
  (`Update-OdsAppFromSource`) is intentionally not ported.
- Scheduled tasks are now `ods-sync` + `ods-tray`; the `OneDriveCodeSync*` PowerShell
  tasks and the staged PowerShell app dir are retired.

---

## [0.1.0] — 2026-06-10 (PowerShell)

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
