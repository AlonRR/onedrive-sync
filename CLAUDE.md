# onedrive-sync (`ods`) — project notes for Claude

Two-way OneDrive sync for code folders, built on **rclone bisync + git** — one small
native **Rust** binary (engine + CLI + management GUI + tray, egui/eframe). This file
captures the non-obvious things; the **code is the source of truth**, and `README.md` /
`MIGRATION.md` carry the module-role table, the CLI surface, and the port status.

## This is the Rust rewrite — and it dogfoods itself
- The repo **continues the original PowerShell project's git history**: tag
  `v1.0-powershell` marks the last PowerShell commit; `main` is the continuation. The
  PowerShell sources live only in history now (recover via `git show v1.0-powershell:<path>`).
- **This folder is itself a live synced project** (the `Tools\onedrive-sync` mirror).
  Editing source here propagates to OneDrive on the next scheduled run. `target/` is
  gitignored AND in the tool's `exclude_dirs`, so builds never sync. `mappings.json` is the
  live shared catalog — gitignored on purpose; leave it.

## Layout & binaries
- `src/lib.rs` is the crate; every module (`paths`, `config`, `state`, `jsonio`,
  `discovery`, `filter`, `git`, `engine`, `conflicts`, `prune`, `run`, `actions`, `events`,
  `gui`, `icon`) lives in the library. See `README.md` for the per-module role table.
- **Two binaries, both linking the lib:** `ods` (`src/main.rs`, the CLI) and `ods-gui`
  (`src/bin/ods-gui.rs`, the windowed app). `ods gui` and `ods-gui.exe` both reach
  `gui::run_gui`.

## State & data (per machine)
- **`%LOCALAPPDATA%\onedrive-sync\`** is the state dir / local root (NOT the install dir):
  `config.toml`, `machine-state.json`, `pending.json`, `bisync\<idhash>\` baselines,
  `versions\<idhash>\` archive, `events\YYYY-MM-DD.jsonl`, `logs\`, `.lock` (run lock),
  and optionally `rclone.exe` (a local override only — the installer does NOT drop one;
  `rclone` normally comes off PATH). The **installed binaries** live separately in
  `%LOCALAPPDATA%\ods\`.
- Shared, in OneDrive: `Tools\onedrive-sync\mappings.json` (catalog + tombstones; merged
  across machines via conflict copies — a per-machine lock can't protect it).
- `engine::id_hash` (md-5) names the `bisync\`/`versions\` subdirs — keep it stable or
  baselines orphan. **Project id by kind** (mirror = rel path; watch = `destRel`; plain =
  Dest relative to the OneDrive root): bisync events and status both key on `Project.id`,
  so they line up.

## On-disk contract (no PowerShell host, but the file format is shared)
- `jsonio` does **BOM-tolerant reads, atomic no-BOM writes, and break-stale file locks** —
  the state files keep the shape the PowerShell tool wrote, so they round-trip. Don't
  "tidy" the JSON shape without checking the `state.rs` tests.
- Machine-state writes go through the atomic file lock; each bisync runs under a per-project
  workdir lock (`jsonio::lock_or_break` on `bisync\<idhash>.lock`) so a manual sync can't
  collide with the scheduled run.

## rclone / git gotchas (real bugs live here)
- `rclone bisync --max-delete` is a **percentage** (we pass 25), NOT a count.
- `--resync` is a **UNION**, not "path1 wins": a stale OneDrive copy re-introduces deleted
  files *and branch refs* back to local. Clean **both** sides identically before a resync.
  We add `--resync-mode newer` so a forced resync keeps the newer side.
- The synced `.git` **omits the index** (the filter excludes `/.git/index`). So on the
  OneDrive copy `git status` / `git clean` lie until you `git reset` to rebuild the index
  from HEAD — cleaning against the stale index would delete the wrong files.

## Scheduled-task ACL gotcha (Windows)
- `ods-sync`/`ods-tray` are registered via `Register-ScheduledTask` with an explicit
  `-Principal` (`RunLevel Limited`), non-elevated — that part genuinely needs no
  elevation. But **once a task exists this way, deleting/disabling/replacing it can
  need elevation even for the same user who created it and even for a plain
  non-admin task** — confirmed on this machine: `schtasks /Delete`, `/Change
  /Disable`, and `Unregister-ScheduledTask` all fail `Access is denied` non-elevated,
  while `Register-ScheduledTask -Force` against an *already-current* task silently
  skips instead of hitting the same wall (see `Register-OdsTask` in install.ps1).
  Check `Get-Task...GetSecurityDescriptor` if this resurfaces — the DACL can end up
  with the task owned by `BA` (Builtin Administrators) and the actual user granted
  only `FR` (read), a state that's sticky once it happens. `ods uninstall`
  (`src/main.rs`) handles this defensively: it checks the real exit status of the
  delete and refuses to touch the Start Menu shortcut / registry entry / install dir
  if task removal failed, so a partial run never strands the very exe the orphaned
  tasks and the Settings "Uninstall" button still point at.

## GUI (egui/eframe)
- The `eframe::App` impl uses `fn ui(&mut self, …)` (not `update`); panels via
  `Panel::top/bottom/left(...).show_inside`, sizing with `default_size`/`max_size` (NOT the
  deprecated `default_height`), styling via `ctx.global_style()` (NOT `style()`).
- egui's default font (Ubuntu-Light) is too thin/low-contrast — `install_fonts` loads
  **Segoe UI** from `C:\Windows\Fonts`. The light + dark `Palette` is measured to clear
  **WCAG-AA** (body AAA). AccessKit is compiled in; F5 = refresh, Esc = close drawer,
  Ctrl + `+`/`-`/`0` = zoom, and a 2px focus ring marks keyboard focus.

## Build / deploy
- `cargo build` (debug) or `cargo build --release`. The release profile keeps **unwinding**
  (no `panic="abort"`) on purpose, so a panic in a background sync thread is contained
  instead of killing the tray.
- `scripts/install.ps1` builds release + installs to `%LOCALAPPDATA%\ods` and registers the
  `ods-sync` (scheduled) and `ods-tray` (logon) tasks. **A running GUI locks `ods-gui.exe`**
  — `Stop-Process -Name ods-gui` before building/installing; the tray must be restarted to
  pick up a new binary.
- Run `cargo test` (lib unit tests + `tests/divergence.rs`) before committing engine changes.

## Conventions
- Conventional Commits matching history (`feat(scope):` / `fix(scope):` / `test:`), subjects
  ≤72 chars, detail in the body. **No `Co-Authored-By` trailer.**
