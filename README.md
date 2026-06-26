# ods

Two-way OneDrive sync for code folders, built on **rclone bisync + git**. One small
native binary: engine, CLI, management GUI, and system tray. A Rust rewrite of a
PowerShell tool, chosen to eliminate three structural problems of the original stack —
PowerShell's dynamic-scope crashes, the WPF re-entrancy crashes, and the per-run script
re-parse overhead.

`rclone` and `git` do the real work as subprocesses; `ods` orchestrates: discovers
projects, generates per-repo filters, runs bisync with a delete-brake and a git health
check, and surfaces state in a native window + tray.

## Build

```sh
cargo build --release      # -> target/release/ods.exe (~10 MB, self-contained)
```

Requires the Rust toolchain (MSVC), and `rclone` + `git` on the machine.

## Use

```
ods list                 # projects + per-machine status (+ conflict flags)
ods status               # last-run summary, recent errors, needs-attention set
ods sync                 # the full run (all active projects)
ods sync <id>            # one project (partial id ok)
ods sync --dry-run       # preview, changes nothing
ods sync --approve-deletes   # raise the delete-brake to 100% for this run
ods resync [id]          # force a fresh bisync baseline (one project, or all active)
ods pull <id>            # activate + sync a project here (revives a tombstone)
ods unmap <id> [--delete-local]   # stop syncing here; keep the OneDrive copy
ods forget <id>          # retire globally (tombstone); reversible with pull
ods add-watch <local> <dest>      # map a local folder to an arbitrary OneDrive dest
ods restore <id> [--file F] [--at T]   # restore from the local version archive
ods conflicts            # list unresolved conflict files
ods discover             # interactively choose which available projects to sync
ods pause | ods resume   # pause/resume the scheduled sync (flag file)
ods filter <id>          # print the generated rclone filter (for validation)
ods diag                 # write a diagnostic bundle to %TEMP%
ods gui                  # management window + tray
```

## Layout

| module | role |
|---|---|
| `paths` / `config` / `state` | machine paths + path guards, TOML config, machine-state.json + the shared catalog (with conflict-copy merge) |
| `jsonio` | BOM-tolerant reads, atomic no-BOM writes, break-stale file locks (PowerShell-interop on-disk contract) |
| `discovery` | find mirror / watch / plain projects (both OneDrive and local sides) |
| `filter` | git-aware rclone filter generation |
| `git` | `ls-files`, gitignore listing, `fsck` health check |
| `engine` | bisync orchestration (filter, compare-mode, delete-brake, watchdog, per-project lock) + gate / seed / baseline |
| `conflicts` | conflict-file scan + git-divergence reconcile |
| `prune` | version-archive pruning (age + size, keeps each project's newest run) |
| `run` | the scheduled run: lock, reconcile, classify, per-project sync, retry, summary |
| `actions` | shared mutations: pull / unmap / forget / add-watch / resync / restore / pause / resume |
| `events` | JSONL event log + sync.log |
| `gui` | egui management window + tray |

See `MIGRATION.md` for parity validation and the cutover plan.
