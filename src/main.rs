//! ods CLI — a thin command surface over the shared engine. Every management
//! command shares the library, so it behaves identically to the tray/GUI.

use anyhow::Result;
use clap::{Parser, Subcommand};
use ods::config::Config;
use ods::discovery::discover;
use ods::paths::Paths;
use ods::state::{Catalog, MachineState, Status};
use ods::{actions, conflicts, events, run};

#[derive(Parser)]
#[command(name = "ods", about = "OneDrive two-way code sync (Rust)", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List known projects and their per-machine status.
    List,
    /// Show a last-run / errors / needs-attention summary.
    Status,
    /// Sync one project by id, or all if omitted.
    Sync {
        id: Option<String>,
        /// Preview without changing files.
        #[arg(long)]
        dry_run: bool,
        /// Raise the delete-brake to 100% for this run (allow mass deletions).
        #[arg(long)]
        approve_deletes: bool,
        /// Run even if paused (the no-id run honors paused.flag by default).
        #[arg(long)]
        force: bool,
    },
    /// Force a fresh bisync baseline for a project id (or all active if omitted).
    Resync { id: Option<String> },
    /// Pull a project local on this machine (skip/undecided/forgotten -> active).
    Pull { id: String },
    /// Stop syncing a project here (keep the OneDrive copy).
    Unmap {
        id: String,
        /// Also remove the local copy (refused on a protected root).
        #[arg(long)]
        delete_local: bool,
    },
    /// Retire a project globally (tombstone). Reversible with `pull`.
    Forget { id: String },
    /// Map a local folder to an arbitrary OneDrive destination (watch project).
    AddWatch { local: String, dest: String },
    /// Restore a project (or one --file) from the local version archive.
    Restore {
        id: String,
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        at: Option<String>,
    },
    /// List unresolved conflict files across projects.
    Conflicts,
    /// Delete filtered files (e.g. node_modules) from a project's OneDrive copy,
    /// enforcing "filtered files don't live on OneDrive". Previews unless --yes.
    Clean {
        id: String,
        /// Actually delete (without this flag, only previews what would go).
        #[arg(long)]
        yes: bool,
    },
    /// Interactively choose which available projects to sync locally.
    Discover,
    /// Pause the scheduled sync (runs skip until `resume`).
    Pause,
    /// Resume the scheduled sync.
    Resume,
    /// Print the generated rclone filter for a project (for validation).
    Filter { id: String },
    /// Write a diagnostic bundle (logs + config + state) to %TEMP%.
    Diag,
    /// Open the management window + system tray.
    Gui,
    /// Remove the scheduled tasks, Start Menu shortcut, and Installed Apps entry,
    /// then delete %LOCALAPPDATA%\ods. This is the target Settings > Apps launches;
    /// run it directly to fully remove ods the same way.
    Uninstall,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Uninstall deliberately skips Paths::discover(), which requires a signed-in
    // OneDrive client — every other subcommand needs that, but a broken OneDrive
    // sign-in is a plausible reason someone reaches for uninstall in the first place.
    if let Cmd::Uninstall = cli.cmd {
        if let Err(e) = uninstall() {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let paths = Paths::discover()?;
    let config = Config::load(&paths)?;

    if let Err(e) = dispatch(cli.cmd, &paths, &config) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    Ok(())
}

/// Remove ods's Windows integration for the current user: stop the tray, unregister
/// the scheduled tasks, delete the Start Menu shortcut and the "Installed Apps"
/// registry entry, then self-delete %LOCALAPPDATA%\ods (which holds this very exe).
fn uninstall() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let local = std::env::var("LOCALAPPDATA").map_err(|_| "LOCALAPPDATA is not set".to_string())?;
    let dir = std::path::Path::new(&local).join("ods");

    println!("Stopping ods-gui...");
    let _ = Command::new("taskkill").args(["/F", "/IM", "ods-gui.exe"]).output();

    // A task registered by install.ps1 is owned by whoever was elevated the first
    // time it was created (install.ps1 itself only gets away without elevation when
    // an already-current task lets it skip re-registration) — so deleting it can
    // need elevation even though the user who owns ods never needed it to sync.
    // Check each result for real instead of assuming success.
    let mut tasks_removed = true;
    for task in ["ods-sync", "ods-tray"] {
        match Command::new("schtasks").args(["/Delete", "/TN", task, "/F"]).output() {
            Ok(out) if out.status.success() => println!("removed scheduled task {task}"),
            Ok(out) => {
                tasks_removed = false;
                println!("could not remove scheduled task {task}: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Err(e) => {
                tasks_removed = false;
                println!("could not remove scheduled task {task}: {e}");
            }
        }
    }

    // Don't touch anything else if the tasks are still there: the tasks and
    // UninstallString both point at the files in `dir`, so deleting it now would
    // strand a live scheduled task with no exe to run, and no way to re-run
    // `ods uninstall` to finish the job.
    if !tasks_removed {
        println!();
        println!("Scheduled tasks need an elevated shell to remove (same as install.ps1's");
        println!("schedule swap). Nothing else was changed — re-run 'ods uninstall' from an");
        println!("administrator prompt to finish.");
        return Err("uninstall incomplete: scheduled tasks need elevation".to_string());
    }

    if let Ok(appdata) = std::env::var("APPDATA") {
        let shortcut = std::path::Path::new(&appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs\ods (OneDrive Sync).lnk");
        let _ = std::fs::remove_file(&shortcut);
    }

    let _ = Command::new("reg")
        .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\ods", "/f"])
        .output();
    println!("removed the Installed Apps entry");

    // This exe's own file lives inside `dir`, so it can't remove the directory while
    // running (the file is locked). Hand off to a short-lived detached helper that
    // waits for this process to exit, then deletes the whole directory. `ping`, not
    // `timeout`, for the delay — `timeout.exe` refuses to run with redirected stdin,
    // which is exactly how Windows launches an UninstallString.
    let script = format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display());
    let _ = Command::new("cmd")
        .args(["/C", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn();

    println!("ods uninstalled.");
    Ok(())
}

fn list_projects(paths: &Paths, config: &Config) -> Vec<ods::discovery::Project> {
    discover(paths, config, &Catalog::load(paths))
}

/// `--approve-deletes` raises the brake to 100% for this run (matches the PS tool
/// cloning the config with MaxDeletePercent=100).
fn with_approve(config: &Config, approve: bool) -> Config {
    let mut c = config.clone();
    if approve {
        c.max_delete_percent = 100;
    }
    c
}

fn dispatch(cmd: Cmd, paths: &Paths, config: &Config) -> Result<(), String> {
    match cmd {
        Cmd::List => {
            let state = MachineState::load(paths);
            let catalog = Catalog::load(paths);
            let projects = discover(paths, config, &catalog);
            println!("local root : {}", paths.local_root.display());
            println!("onedrive   : {}", paths.onedrive.display());
            println!(
                "state      : {} active, {} skip, {} catalog entries, {} tombstones",
                state.active.len(),
                state.skip.len(),
                catalog.entries.len(),
                catalog.forgotten.len()
            );
            println!("\n{} project(s):", projects.len());
            for p in &projects {
                let conflicts = if p.local.exists() {
                    conflicts::scan(p).len()
                } else {
                    0
                };
                let cflag = if conflicts > 0 { format!(" !{conflicts}c") } else { String::new() };
                println!(
                    "  {:9} {:5} {:10} {}{}",
                    p.kind.as_str(),
                    if p.git { "git" } else { "plain" },
                    state.status_of(&p.id).as_str(),
                    p.id,
                    cflag
                );
            }
        }
        Cmd::Status => {
            match events::last_run_end(paths) {
                Some(e) => println!("last run-end : {}  ({})", e.summary.as_deref().unwrap_or("?"), e.ts),
                None => println!("last run-end : (none recent)"),
            }
            if actions::is_paused(paths) {
                println!("paused       : yes (resume to re-enable)");
            }
            if let Ok(text) = std::fs::read_to_string(paths.log_file()) {
                let mut errs: Vec<&str> = text.lines().filter(|l| l.contains("[ERROR]")).collect();
                let recent: Vec<&str> = errs.split_off(errs.len().saturating_sub(5));
                if recent.is_empty() {
                    println!("recent errors: none");
                } else {
                    println!("recent errors:");
                    for e in recent {
                        println!("  {e}");
                    }
                }
            }
            let att = events::attention_ids(paths);
            println!("needs attention: {}", if att.is_empty() { "none".into() } else { att.join(", ") });
        }
        Cmd::Sync { id, dry_run, approve_deletes, force } => {
            let cfg = with_approve(config, approve_deletes);
            match id {
                // A single project is always an explicit action — like -SyncNow <id>,
                // it syncs directly and does not consult the pause flag.
                Some(want) => {
                    let list = list_projects(paths, &cfg);
                    let rid = actions::resolve_id(&list, &want, false)?;
                    let Some(p) = list.iter().find(|p| p.id == rid) else {
                        println!("No project matching '{want}'.");
                        return Ok(());
                    };
                    let st = MachineState::load(paths);
                    let (status, _) = run::sync_project(paths, &cfg, &st, p, dry_run, false);
                    println!("{:55} -> {status}", p.id);
                }
                // The no-id run is the scheduled/default run: it HONORS paused.flag
                // (the authoritative, elevation-free pause). --force overrides it.
                None => {
                    let summary = run::run(paths, &cfg, run::RunOpts { dry_run, ignore_pause: force });
                    if summary.is_empty() {
                        println!("Run complete. ({})", if actions::is_paused(paths) { "paused — use --force to override" } else { "skipped" });
                    } else {
                        println!("Run complete. {summary}");
                    }
                }
            }
        }
        Cmd::Resync { id } => {
            let resolved = match &id {
                Some(want) if want != "*" => {
                    let list = list_projects(paths, config);
                    Some(actions::resolve_id(&list, want, false)?)
                }
                _ => None,
            };
            actions::resync(paths, config, resolved.as_deref());
            println!("Resync complete.");
        }
        Cmd::Pull { id } => {
            let list = list_projects(paths, config);
            let rid = actions::resolve_id(&list, &id, false)?;
            let status = actions::pull(paths, config, &rid)?;
            println!("{rid} -> {status}");
        }
        Cmd::Unmap { id, delete_local } => {
            let list = list_projects(paths, config);
            let rid = actions::resolve_id(&list, &id, true)?;
            actions::unmap(paths, config, &rid, delete_local)?;
        }
        Cmd::Forget { id } => {
            let list = list_projects(paths, config);
            let rid = actions::resolve_id(&list, &id, true)?;
            actions::forget(paths, &rid);
        }
        Cmd::AddWatch { local, dest } => {
            let rid = actions::add_watch(paths, std::path::Path::new(&local), std::path::Path::new(&dest))?;
            println!("Mapped watch project -> {rid}");
        }
        Cmd::Restore { id, file, at } => {
            let list = list_projects(paths, config);
            let rid = actions::resolve_id(&list, &id, true)?;
            actions::restore(paths, config, &rid, at.as_deref(), file.as_deref())?;
            println!("Restored {rid}.");
        }
        Cmd::Conflicts => {
            let found = actions::list_conflicts(paths, config);
            if found.is_empty() {
                println!("No unresolved conflicts.");
            } else {
                for (id, files) in found {
                    println!("{id}:");
                    for f in files {
                        println!("   {}", f.display());
                    }
                }
            }
        }
        Cmd::Clean { id, yes } => {
            let list = list_projects(paths, config);
            let rid = actions::resolve_id(&list, &id, true)?;
            let Some(p) = list.iter().find(|p| p.id == rid) else {
                println!("No project matching '{id}'.");
                return Ok(());
            };
            let scan = actions::scan_dest_filtered(config, p)?;
            if scan.items.is_empty() {
                println!("{rid}: OneDrive is already clean of filtered files.");
                return Ok(());
            }
            println!("{rid}: {} entr(ies), {} file(s) on OneDrive match the filters:", scan.items.len(), scan.total_files);
            for it in &scan.items {
                println!("  {} {}", if it.is_dir { "DIR " } else { "file" }, it.rel);
            }
            if yes {
                let (f, b) = actions::clean_scanned(paths, config, p, &scan.items)?;
                println!("Cleaned {f} file(s), {b} bytes freed from the OneDrive copy.");
            } else {
                println!("(preview — re-run with --yes to delete)");
            }
        }
        Cmd::Discover => discover_interactive(paths, config),
        Cmd::Pause => actions::pause(paths),
        Cmd::Resume => actions::resume(paths),
        Cmd::Filter { id } => {
            let list = list_projects(paths, config);
            match list.iter().find(|p| p.id.eq_ignore_ascii_case(&id)) {
                Some(p) => print!("{}", ods::filter::generate(p, config)),
                None => eprintln!("no project matching '{id}'"),
            }
        }
        Cmd::Diag => match actions::diag(paths, config) {
            Ok(p) => println!("Diagnostic bundle: {}", p.display()),
            Err(e) => eprintln!("{e}"),
        },
        Cmd::Gui => {
            ods::gui::run_gui(paths.clone(), config.clone()).map_err(|e| e.to_string())?;
        }
        Cmd::Uninstall => unreachable!("main() handles Uninstall before Paths::discover()"),
    }
    Ok(())
}

/// Interactive picker for undecided projects (the CLI half of -Discover). New
/// watch-root repos that need a destination are listed with an add-watch hint.
fn discover_interactive(paths: &Paths, config: &Config) {
    let state = MachineState::load(paths);
    let projects = list_projects(paths, config);
    let undecided: Vec<&ods::discovery::Project> = projects
        .iter()
        .filter(|p| state.status_of(&p.id) == Status::Undecided)
        .collect();
    if undecided.is_empty() {
        println!("No new projects awaiting a decision.");
        return;
    }
    println!("New projects available to sync on this machine:");
    for (i, p) in undecided.iter().enumerate() {
        println!("  [{}] {}  ({})", i + 1, p.name, p.id);
    }
    print!("Enter numbers to PULL (comma-separated), 'a' for all, Enter to skip all: ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return;
    }
    let ans = line.trim();
    let chosen: Vec<usize> = if ans.eq_ignore_ascii_case("a") {
        (0..undecided.len()).collect()
    } else {
        ans.split([',', ' '])
            .filter_map(|t| t.trim().parse::<usize>().ok())
            .filter(|n| *n >= 1 && *n <= undecided.len())
            .map(|n| n - 1)
            .collect()
    };
    for (i, p) in undecided.iter().enumerate() {
        if chosen.contains(&i) {
            match actions::pull(paths, config, &p.id) {
                Ok(s) => println!("pulled {} -> {s}", p.id),
                Err(e) => println!("  {e}"),
            }
        } else {
            ods::state::set_state(paths, &p.id, Status::Skip);
        }
    }
}

