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
| **state writes** | atomic no-BOM writes round-trip with `Get-OdsMachineState`/`Get-OdsCatalog` **both directions, both PowerShell hosts**; catalog conflict-copy merge ported |
| discovery | **identical 7-project set** (mirror + watch); now also scans the local mirror side for repos not yet on OneDrive |
| filter generation | **byte-identical** filter content |
| bisync (dry-run) | **identical rclone exit code**; now under the per-project workdir lock |
| full run (dry-run) | **identical summary counts** (`ok=4 warn=1`) with the gate / reconcile / retry / prune path live |
| **real sync** | `Bisync successful`, baseline updated, 0 B transferred on an in-sync repo |
| gate / seed / retry | idle-stability gate, first-run newest-wins seed, smart-retry with backoff, defer escalation |
| conflicts / divergence | conflict-file scan; git-divergence reconcile (fetch/merge or tag-orphan) |
| version prune | age + size cap; unit-proven to **never delete a project's newest run** |
| actions | pull / unmap (+ protected-root-guarded `--delete-local`) / forget / add-watch / resync / restore / pause / resume / discover |
| GUI + tray | native window: Projects grid (last-sync + conflict columns), per-project settings, Pending / Retired / Add-watch tabs, conflict viewer, Pause toggle; tray red on attention/conflicts |

CLI: `ods list | status | sync [id] [--dry-run] [--approve-deletes] | resync [id] | pull <id> | unmap <id> [--delete-local] | forget <id> | add-watch <local> <dest> | restore <id> [--file] [--at] | conflicts | discover | pause | resume | filter <id> | diag | gui`.

Unit tests cover the state JSON shape, catalog merge, BOM tolerance, the
conflict-name matcher, the path guards (protected-root / overlap / rel-under),
and the version-prune newest-run guard.

## Intentionally NOT ported

- **Tool self-update (`Update-OdsAppFromSource`).** That staged newer `app-src`
  PowerShell scripts into the live app dir. The Rust tool is a single binary —
  it updates by swapping the `.exe` (the installer copies `target\release`), so
  there is nothing to stage. No equivalent is needed.

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
