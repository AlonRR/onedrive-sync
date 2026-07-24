# Changelog

All notable changes to onedrive-sync (`ods`) — the native Rust engine, CLI,
management GUI, and tray. This repo continues an earlier PowerShell tool's git
history; that pre-rewrite history is summarized at the bottom, under the
`v1.0-powershell` tag.

## [0.3.2] — 2026-07-24

### Fixed
- **`install.ps1` no longer races the tray on redeploy.** It stopped `ods-gui.exe` and
  copied immediately, but `Stop-Process` returns before Windows releases the exe's image
  lock, so the copy could fail "used by another process" — precisely the path `ods update`
  and any redeploy take, since the tray is running then. It now waits for the process to
  fully exit before copying.

## [0.3.1] — 2026-07-24

### Fixed
- **`install.ps1 -FromRelease` / `get.ps1` checksum verification.** The SHA256 sidecar
  fetched via `Invoke-WebRequest -UseBasicParsing` arrives as a **byte array** (GitHub
  serves it as octet-stream), so the hash comparison ran on bytes and aborted every
  download with a false "checksum mismatch." Decode the sidecar to text before comparing.
  The v0.3.0 binaries were correct — only this client-side check was broken.

## [0.3.0] — 2026-07-24

### Added
- **`ods` is now on PATH.** `install.ps1` adds `%LOCALAPPDATA%\ods` to the per-user
  PATH (registry-safe `REG_EXPAND_SZ`, idempotent, with a `WM_SETTINGCHANGE`
  broadcast), so the documented CLI actually works from any terminal instead of only
  via the full path. `ods uninstall` removes the entry again.
- **Dependency preflight.** `install.ps1` checks for `rclone` + `git` and offers to
  install a missing one via `winget` — prompting first, `-YesDeps` to auto-approve
  (non-interactive), `-SkipDeps` to skip the check entirely. No more first-sync
  failing silently because rclone isn't there.
- **Checksum-verified downloads.** `install.ps1 -FromRelease` verifies each binary
  against its published `.sha256` before running it (a mismatch aborts; an absent
  sidecar, as on older releases, warns and skips).
- **`ods update`:** re-runs the online installer in a detached console to pull the
  latest release and swap the binaries in place (a running exe can't overwrite itself).

### Fixed
- **`ods uninstall` now fully removes `%LOCALAPPDATA%\ods`.** The self-delete helper
  (which must outlive the exiting `ods.exe` to delete its own directory) never actually
  worked: its command line was mangled by argument quoting, so `rmdir` got a broken path
  and left the whole dir — its ~10 MB of binaries — behind after a "successful" uninstall.
  It now spawns a detached `cmd` retry loop with a raw command line (retries past a
  lingering `ods-gui.exe` tray lock, ~40 s cap, and breaks away from the launcher's job).
- Corrected the stale "installer bundles rclone here" comments in `paths.rs` /
  `engine.rs` / `CLAUDE.md`: the local `rclone.exe` is an optional override only; the
  normal path is `rclone` on PATH.
- `install.ps1` / `get.ps1` are now pure ASCII — removes the latent
  `powershell.exe -File` mis-parse from em-dashes in a no-BOM script.

## [0.2.0] — 2026-07-19

### Added
- **`ods uninstall`:** a native subcommand that stops the tray, unregisters the
  scheduled tasks, and removes the Start Menu shortcut and install directory.
  Wired into a new Windows Settings > Apps > Installed apps entry (via
  `install.ps1`, HKCU, no elevation) so the app can be removed the normal way, not
  just rolled back to the PowerShell tool.
- **GUI:** reflowing project-row cards (no more hidden columns when the detail
  panel docks right), a bottom/right dockable detail panel, nav-rail icons with
  responsive collapse on narrow windows, and an explanation shown for why a
  project needs "Attention" instead of a silent badge.

### Fixed
- A failed bisync now captures rclone's actual error text into the event log,
  instead of leaving "needs attention" unexplained.

## [0.1.0] — 2026-06-27 — Rust rewrite (`ods`)

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

## Pre-rewrite: PowerShell tool (tag `v1.0-powershell`, 2026-06-10)

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

<!-- Full changelog: version headings link to their GitHub compare/release view. -->
[0.3.2]: https://github.com/AlonRR/onedrive-sync/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/AlonRR/onedrive-sync/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/AlonRR/onedrive-sync/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/AlonRR/onedrive-sync/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/AlonRR/onedrive-sync/releases/tag/v0.1.0
