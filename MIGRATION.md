# ods (Rust) — status & migration plan

A from-scratch Rust port of the PowerShell onedrive-sync tool: one ~10 MB native
binary that is the engine, CLI, management GUI, and tray. `rclone` and `git` stay
subprocesses. No PowerShell host, so the dynamic-scope crash class is gone; no WPF,
so the re-entrancy crash class is gone; no per-run script re-parse, so the overhead
is gone.

## What's done (and proven against the PowerShell tool on real data)

| Area | Validation |
|---|---|
| config / paths / state | reads the real machine-state.json + mappings.json + the effective (override-merged) roots |
| discovery | **identical 7-project set** (mirror + watch), same ids/kinds/git |
| filter generation | **byte-identical** filter content |
| bisync (dry-run) | **identical rclone exit code** |
| full run (dry-run) | **identical summary** (`ok=4 warn=1`) |
| **real sync** | `Bisync successful`, baseline updated, 0 B transferred on an in-sync repo |
| GUI + tray | native window lists every project + status, tray red/green by attention, threaded syncs |

CLI: `ods list | status | sync [id] [--dry-run] | filter <id> | gui`.

## Not yet ported (refinements; the run still works without them)

- idle-stability gate + smart-retry/deferral of transiently-gated repos
- undecided-project → `pending.json` writing and the interactive discover flow
- version-archive pruning; conflict scan/listing; pause/resume CLI; -ApproveDeletes flag
- per-project settings UI, watch-folder add, retired-projects view

These are additive; the scheduled sync and the GUI are usable today.

## Cutover (do this when YOU are ready — not before)

The PowerShell tool is the proven one. Don't flip the schedule until the Rust tool has
shadow-run for a while:

1. **Shadow.** For a week or two, periodically run `ods sync --dry-run` right after a
   real PowerShell run and confirm the summaries still match (they do today).
2. **Swap the schedule.** Register a scheduled task running
   `ods.exe sync` every 30 min + at logon, and `ods.exe gui` at logon; then disable the
   `OneDriveCodeSync` / `OneDriveCodeSyncTray` PowerShell tasks. Keep them (don't delete)
   so you can roll back instantly.
3. **Watch the first few real runs** in the GUI / `ods status`.

Rollback is just: re-enable the two PowerShell tasks, disable the `ods` ones.

## Repo hygiene (your "local repo, sync via the tool" idea)

- This repo lives **outside OneDrive** (`C:\Users\…\ods`) so its 1.7 GB `target/` never
  churns a sync; `target/` is gitignored and is in the tool's `exclude_dirs` anyway.
- To dogfood, register this repo's **source** as a watch project so the tool syncs it
  up (it will skip `target/`).
- Likewise, move the **onedrive-sync** tool's own repo out of
  `…\OneDrive\Tools\onedrive-sync` into a local repo synced the same way, so git and
  OneDrive stop contending over the same files.
