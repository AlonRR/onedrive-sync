# ods (Rust) — parity validation record

The Rust rewrite's cutover is complete (`install.ps1` has cut every machine over
since `v0.1.0`); this doc is the record of what was validated against the
PowerShell tool during that port, kept for reference. For current behavior see
`README.md` / `CLAUDE.md`; for what shipped in each release see `CHANGELOG.md`.

## What was validated against the PowerShell tool on real data

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

## Cutover — complete

`scripts/install.ps1` automates what this section used to describe manually: it
registers `ods-sync` (every 30 min + at logon) and `ods-tray` (at logon), then
disables (not deletes) the old `OneDriveCodeSync` / `OneDriveCodeSyncTray`
PowerShell tasks, aborting the swap rather than leaving both schedules live.

Rollback: `scripts\uninstall.ps1` removes the `ods` tasks and re-enables the
PowerShell ones. A full removal (no PowerShell fallback) is `ods uninstall` — see
`README.md`.

## Repo hygiene

This repo's own source is itself a synced project (see `CLAUDE.md`) — it lives
under a local root that the tool mirrors into OneDrive, so `target/` (gitignored,
and in the tool's `exclude_dirs`) never churns a sync.
