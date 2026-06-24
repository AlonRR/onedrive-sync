use anyhow::Result;
use clap::{Parser, Subcommand};
use ods::config::Config;
use ods::engine::{bisync, BisyncOpts};
use ods::paths::Paths;
use ods::state::{Catalog, MachineState, Status};

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
    /// Show a last-run / errors summary.
    Status,
    /// Sync one project by id, or all if omitted.
    Sync {
        id: Option<String>,
        /// Preview without changing files.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the generated rclone filter for a project (for validation).
    Filter { id: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    let config = Config::load(&paths)?;

    match cli.cmd {
        Cmd::List => {
            let state = MachineState::load(&paths);
            let catalog = Catalog::load(&paths);
            println!("local root : {}", paths.local_root.display());
            println!("onedrive   : {}", paths.onedrive.display());
            println!(
                "parents    : {}",
                config
                    .project_parent_paths(&paths)
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!(
                "watch roots: {}",
                config
                    .watch_root_paths(&paths)
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let projects = ods::discovery::discover(&paths, &config, &catalog);
            println!(
                "state      : {} active, {} skip, {} catalog entries, {} tombstones",
                state.active.len(),
                state.skip.len(),
                catalog.entries.len(),
                catalog.forgotten.len()
            );
            println!("\n{} project(s):", projects.len());
            for p in &projects {
                println!(
                    "  {:7} {:5} {:10} {}",
                    p.kind.as_str(),
                    if p.git { "git" } else { "plain" },
                    state.status_of(&p.id).as_str(),
                    p.id
                );
            }
        }
        Cmd::Status => {
            match ods::events::last_run_end(&paths) {
                Some(e) => println!(
                    "last run-end : {}  ({})",
                    e.summary.as_deref().unwrap_or("?"),
                    e.ts
                ),
                None => println!("last run-end : (none recent)"),
            }
            if let Ok(text) = std::fs::read_to_string(paths.log_file()) {
                let mut errs: Vec<&str> =
                    text.lines().filter(|l| l.contains("[ERROR]")).collect();
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
            let att = ods::events::attention_ids(&paths);
            if att.is_empty() {
                println!("needs attention: none");
            } else {
                println!("needs attention: {}", att.join(", "));
            }
        }
        Cmd::Sync { id, dry_run } => match id {
            // A single project: bisync just that one (the -SyncNow <id> path).
            Some(want) => {
                let state = MachineState::load(&paths);
                let catalog = Catalog::load(&paths);
                let projects = ods::discovery::discover(&paths, &config, &catalog);
                let Some(p) = projects.iter().find(|p| p.id.eq_ignore_ascii_case(&want)) else {
                    eprintln!("no project matching '{want}'");
                    return Ok(());
                };
                if p.git {
                    if let Err(e) = ods::git::integrity_ok(&p.local) {
                        ods::events::log(
                            &paths,
                            "ERROR",
                            &format!("git fsck found corruption in {}: {}", p.id, e),
                        );
                        return Ok(());
                    }
                }
                let code = bisync(
                    &paths,
                    &config,
                    &state,
                    p,
                    BisyncOpts {
                        dry_run,
                        ..Default::default()
                    },
                );
                let class = if code == 0 {
                    "ok"
                } else if code < 8 {
                    "warn"
                } else {
                    "error"
                };
                println!("{:55} code={code} -> {class}", p.id);
            }
            // No id: the full scheduled/default run (lock, events, summary).
            None => {
                let summary = ods::run::run(
                    &paths,
                    &config,
                    ods::run::RunOpts {
                        dry_run,
                        ignore_pause: false,
                    },
                );
                println!("Run complete. {}", if summary.is_empty() { "(skipped)" } else { &summary });
            }
        },
        Cmd::Filter { id } => {
            let catalog = Catalog::load(&paths);
            let projects = ods::discovery::discover(&paths, &config, &catalog);
            match projects.iter().find(|p| p.id.eq_ignore_ascii_case(&id)) {
                Some(p) => print!("{}", ods::filter::generate(p, &config)),
                None => eprintln!("no project matching '{}'", id),
            }
        }
    }
    Ok(())
}
