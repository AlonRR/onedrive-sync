# OneDrive 2-Way Sync for Git Projects

Two-way sync of your code projects through OneDrive — versioned, selective per
machine, and git-aware. Built on [`rclone bisync`](https://rclone.org/bisync/),
driven by PowerShell and Windows Task Scheduler.

> Replaces the old one-way `robocopy /MIR` push. The installer migrates it for you.

---

## Concept & model

- **A project = a git repo.** A folder is a project **iff it contains `.git`**.
  `.git` is synced, so history travels and marks projects on both sides.
  Non-git folders can still sync if you add them explicitly (`$PlainFolders`).
- **The mirroring law.** A project at `OneDrive\<rel>` corresponds to
  `%USERPROFILE%\<rel>` — same sub-path, different root. So
  `OneDrive\Projects\web\my-app` ⇄ `%USERPROFILE%\Projects\web\my-app`.
- **Two state axes:**
  | Axis | Decided | Stored |
  |------|---------|--------|
  | *Where* a project lives in OneDrive | once, globally | `mappings.json` (in OneDrive) |
  | *Whether* a project is local here | per-machine | `machine-state.json` (LOCALAPPDATA) |
- **What syncs (the filter rule):** tracked files **always** sync; untracked files
  sync **unless git ignores them**, except an allow-list (`$SyncAnywayList` — e.g.
  `.env`) that syncs anyway. Static `$ExcludeDirs`/`$ExcludeFiles` are a coarse first cut.
- **Run-local / sync-source split.** The scripts live in OneDrive
  (`Tools\onedrive-sync\app-src\`) and self-distribute, but **run from a local copy**
  in `%LOCALAPPDATA%\onedrive-sync\app\` so OneDrive never locks a running script.

### How a sync run works
`onedrive-sync.ps1` → loads the core → takes a PID lock → reconcile → discover →
gate (OneDrive-idle + `.git` quiesced) + smart retry → `rclone bisync` per active
repo (prioritized, time-budgeted) → post-sync `.git` verify → conflict scan →
version prune.

---

## Prerequisites (per machine)

- **OneDrive** desktop client installed and signed in (sets `$env:OneDriveConsumer`).
- The scanned project parents set to **“Always keep on this device”** (Files
  On-Demand placeholders break bisync).
- **git** and **rclone (≥ 1.66)** — the installer auto-installs both if missing
  (git via `winget`; rclone pinned + checksum-verified into `%LOCALAPPDATA%`).
- **PowerShell** (the scheduled task + tray run under Windows PowerShell, which is
  always present and STA-capable for the GUI).

---

## Install (run once on each machine)

1. Edit `app-src\sync-config.ps1` → set `$ProjectParents` / `$WatchRoots` / `$PlainFolders`.
2. From the OneDrive tool folder, run:
   ```powershell
   .\install-task.ps1            # or  -IntervalMinutes 15
   ```
   This installs git/rclone, removes the old one-way task, stages the scripts to the
   local app dir, and registers the sync task + tray helper (both at logon).
3. Choose which projects to keep local here:
   ```powershell
   & "$env:LOCALAPPDATA\onedrive-sync\app\onedrive-sync.ps1" -Discover
   ```

**Self-distribution:** scripts + `sync-config.ps1` + `mappings.json` sync to your other
machines automatically. Per-machine items (scheduled task, tray, `machine-state.json`,
`rclone.exe`, bisync workdir, version archive) are set up per machine — so run
`install-task.ps1` once on each.

**Updates:** with `$ToolUpdateMode='auto'` (default) each run stages newer scripts from
OneDrive before running. Set it to `'notify'` for an approval prompt instead.
**Security note:** `auto` means scripts synced via OneDrive auto-execute on every
machine — a single-user convenience; switch to `notify` if you want a gate.

---

## Configuration reference (`sync-config.ps1`)

| Setting | Default | Meaning |
|---------|---------|---------|
| `$ProjectParents` | `OneDrive\Projects` | OneDrive parents whose `.git` children are projects (mirrored locally) |
| `$WatchRoots` | `%USERPROFILE%\Code` | Local folders watched for one-off repos mapped to an *arbitrary* OneDrive dest |
| `$PlainFolders` | `@()` | Opt-in non-git folders (`@{Local;Dest}`) — filtered 2-way sync, no git machinery |
| `$ExcludeDirs` / `$ExcludeFiles` | build junk | Coarse excludes for untracked files |
| `$SyncAnywayList` | `.env, *.local, *.pem, …` | Untracked+gitignored files that sync anyway (secrets/config) |
| `$VersionRetentionDays` / `$VersionMaxGB` | 30 / 5 | Local version-archive pruning (age and size) |
| `$MaxDeletePercent` | 25 | bisync brake against mass deletion |
| `$IdleStabilitySeconds` | 60 | OneDrive-idle / `.git`-quiesce gate window |
| `$CompareMode` | `modtime` | `modtime` (fast) or `checksum`; adapts per project on drift noise |
| `$ToolUpdateMode` | `auto` | `auto` self-update vs `notify` |
| `$RunTimeBudget` | 1500 | Seconds/run before carrying remaining repos to the next cycle |

---

## Usage (CLI)

Run `onedrive-sync.ps1` (local app copy) with:

| Command | Does |
|---------|------|
| *(no args)* | A normal sync run (background semantics) |
| `-Discover` | Interactively pick which available projects to sync here |
| `-List` | Show all projects + per-machine status |
| `-Status` | Recent run/error events |
| `-SyncNow [<id>]` `[-ApproveDeletes]` | Sync one project (or all); `-ApproveDeletes` overrides the max-delete brake |
| `-Pull <id>` | Make a project local here (clears any tombstone) |
| `-Unmap <id> [-DeleteLocal]` | Stop syncing here (keep OneDrive copy); optionally delete the local copy |
| `-Forget <id>` | Retire globally (tombstone); reversible with `-Pull` |
| `-Resync [<id>]` | Force a fresh bisync baseline |
| `-Conflicts` | List unresolved conflict files |
| `-Restore <id> [--File <p>] [--At <time>]` | Restore from the local version archive |
| `-Diag` | Write a diagnostics bundle (secrets redacted) |
| `-Pause` / `-Resume` | Disable/enable the scheduled sync task |
| `-Gui` | Open the management window |
| `-Help` / `-?` | Full help |

Every command is also available from the **tray** (right-click) and the **management
window** (project grid with Sync/Pull/Unmap/Forget/Open/Conflicts/Discover/Show-retired).

---

## Operations

- **Conflicts.** Same-file edits on two machines are kept as `*.conflict-<machine>-<time>`
  copies (logged); resolve by picking the version you want and deleting the other.
  Line-ending-only differences are auto-resolved (no conflict kept).
- **Restore.** `-Restore "<id>" --File src/app.js --At 2026-06-01` pulls a prior version
  from `%LOCALAPPDATA%\onedrive-sync\versions\…`. OneDrive’s own version history is the
  cloud-side fallback (right-click → Version history).
- **Git history.** Commits sync as files. With sequential single-user use this is safe;
  if you ever commit on two machines without syncing between, the divergence guard runs a
  real `git merge` (or tags the orphan tip — never loses commits). For true collaborative
  merging, use a normal git remote.
- **Troubleshooting.** Run `-Diag`. Common causes: a scanned parent not set to “Always
  keep on this device”; OneDrive paused/quota-full (changes won’t propagate — check the
  OneDrive icon); a repo stuck “deferred” because git or OneDrive is busy (it retries).

---

## Limitations & security (by design)

- **Secrets sync** to OneDrive’s cloud (allow-list) so pulled projects run — no encryption layer.
- **Git LFS** isn’t specially handled (detected + warned; future enhancement).
- **Symlinks** aren’t synced (rare on Windows; logged).
- **Renames** re-transfer as delete+add. **OneDrive upload failures** (quota/offline) are
  silent to the tool — check OneDrive’s own status.

See [CHANGELOG.md](CHANGELOG.md) for version history.

---

## Testing

- `Invoke-Pester -Path tests\onedrive-sync.Tests.ps1` (Pester v5), **or**
- `pwsh -File tests\run-tests.ps1` (dependency-free runner).

Both exercise the dangerous, git-aware paths (filters, discovery/pruning, seed,
catalog/tombstones, guards) against throwaway git repos — no real OneDrive needed.
