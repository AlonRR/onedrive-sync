// Windowed entry point: the management window + tray with NO console window
// (windows subsystem). The `ods` binary stays a console app for the CLI; this
// one is what the logon task launches so the tray comes up cleanly.
#![windows_subsystem = "windows"]

fn main() -> anyhow::Result<()> {
    let paths = ods::paths::Paths::discover()?;
    let config = ods::config::Config::load(&paths)?;
    ods::gui::run_gui(paths, config).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}
