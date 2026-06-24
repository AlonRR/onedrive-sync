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
ods list                 # projects + per-machine status
ods status               # last-run summary, recent errors, needs-attention set
ods sync                 # the full run (all active projects)
ods sync <id>            # one project
ods sync --dry-run       # preview, changes nothing
ods filter <id>          # print the generated rclone filter (for validation)
ods gui                  # management window + tray
```

## Layout

| module | role |
|---|---|
| `paths` / `config` / `state` | machine paths, TOML config, machine-state.json + the shared catalog |
| `discovery` | find mirror / watch / plain projects |
| `filter` | git-aware rclone filter generation |
| `git` | `ls-files`, gitignore listing, `fsck` health check |
| `engine` | bisync orchestration (filter, compare-mode, delete-brake, watchdog) |
| `run` | the scheduled run: lock, events, per-project loop, summary |
| `events` | JSONL event log + sync.log |
| `gui` | egui management window + tray |

See `MIGRATION.md` for parity validation and the cutover plan.
