# onedrive-sync — project notes for Claude

Two-way OneDrive sync for code/folders, built on **rclone bisync + git**. Pure PowerShell,
WPF/WinForms tray + CLI + scheduled task. This file captures the non-obvious things; the code
is the source of truth for the rest.

## Layout & load order
- `app-src/` is the source. Dot-source chain: `onedrive-sync-core.ps1` → `…core.discovery.ps1`
  + `…core.engine.ps1` → `…core.run.ps1`. The tray (`onedrive-sync-tray.ps1`) and CLI
  (`onedrive-sync.ps1`) each dot-source core, which pulls in everything.
- `install-task.ps1` registers the scheduled tasks. `tests/run-tests.ps1` is a dependency-free
  test runner (22 checks). `scripts/deploy.ps1` + `scripts/smoke.ps1` are the deploy / health
  helpers (see the `ods-deploy` / `ods-smoke` skills).

## Runtime split — this matters (real bugs live here)
- The **tray runs Windows PowerShell 5.1** (scheduled task `OneDriveCodeSyncTray`, launched as
  `powershell.exe`). The CLI / spawned syncs prefer **pwsh 7**. Core is dot-sourced into **both**,
  so it must work under both. **Always test under both hosts** (use `ods-test`).
- `ConvertFrom-Json` parses ISO timestamps to `[datetime]` under **pwsh 7** but leaves strings
  under **5.1**. `Set-Content -Encoding utf8` writes a **BOM under 5.1**. `Set-StrictMode -Version
  Latest` + `$ErrorActionPreference='Stop'` are in effect (missing-property access throws).

## Deploy — edits to app-src do nothing until staged
The tray + scheduled sync run the **installed copy** at `%LOCALAPPDATA%\onedrive-sync\app`, not
`app-src`. After any edit, run the **`ods-deploy`** skill (`scripts/deploy.ps1`) to stage + restart.
The tray must be restarted to reload core. The kill match MUST exclude `$PID` and anchor on
`-File ...onedrive-sync-tray` (a bare `-match 'onedrive-sync-tray'` self-matches and kills the
deploy process too).

## Data & state
- Per-machine, `%LOCALAPPDATA%\onedrive-sync\`: `machine-state.json` (`active`/`skip` are arrays
  of ids; `compare`/`deferred`/`maxDelete` are objects keyed by id — must be PSCustomObject),
  `.lock` (run lock), `machine-state.json.lock` (state-write lock), `pending.json`,
  `events\YYYY-MM-DD.jsonl`, `bisync\<idhash>\` baselines, `versions\<idhash>\` archive, `logs\`.
- Shared, in OneDrive: `Tools\onedrive-sync\mappings.json` (catalog + tombstones; merged across
  machines via conflict copies — a per-machine lock can't protect it).
- **Project id by kind**: mirror = rel path; watch = `destRel`; plain = Dest relative to the
  OneDrive root (else full Dest). Bisync events and `Get-OdsProjectStatus` both key on
  `$Project.id` — same namespace, so the LAST SYNC column matches.

## Concurrency
The scheduled `Invoke-OdsRun` holds `.lock`. All machine-state writes go through
`Edit-OdsMachineState` (atomic file lock). Each bisync is wrapped in `Invoke-OdsWithProjectLock`
(per-project) so a manual sync can't collide with the scheduled run on the same workdir.

## rclone gotchas
- `rclone bisync --max-delete` is a **percentage** (default 50; we pass 25), NOT a count.
- `--resync` defaults to **path1 (local) wins**; we always add `--resync-mode newer` so a forced
  resync (e.g. a filter/.gitignore change) keeps the newer side instead of overwriting OneDrive.

## Conventions
- Commits: Conventional Commits matching the existing history (`feat(scope):` / `fix(scope):`,
  scope = component like `tray`), subjects ≤72 chars, detail in the body. **No `Co-Authored-By`
  trailer.** Run `tests/run-tests.ps1` (both hosts) before committing core changes.
