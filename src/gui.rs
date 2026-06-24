//! Native management window (egui) + system tray, wired to the engine. Syncs run
//! on a background thread so the UI never blocks. egui is immediate-mode, so there
//! is no retained-mode/dynamic-scope crash surface — the class that plagued WPF.

use crate::config::Config;
use crate::discovery::discover;
use crate::engine::{bisync, BisyncOpts};
use crate::events;
use crate::paths::Paths;
use crate::run::{run, RunOpts};
use crate::state::{Catalog, MachineState};
use eframe::egui;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

struct Row {
    id: String,
    kind: &'static str,
    git: bool,
    status: String,
    attention: bool,
}

struct GuiApp {
    paths: Paths,
    config: Config,
    rows: Vec<Row>,
    last_run: String,
    busy: bool,
    busy_msg: String,
    rx: Option<Receiver<String>>,
    _tray: TrayIcon,
    show_id: MenuId,
    sync_id: MenuId,
    quit_id: MenuId,
}

fn make_icon(rgb: [u8; 3]) -> Icon {
    let (w, h) = (32u32, 32u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
    }
    Icon::from_rgba(rgba, w, h).expect("icon")
}

impl GuiApp {
    fn new(paths: Paths, config: Config) -> Self {
        let menu = Menu::new();
        let show = MenuItem::new("Show window", true, None);
        let sync = MenuItem::new("Sync all now", true, None);
        let quit = MenuItem::new("Quit", true, None);
        menu.append(&show).unwrap();
        menu.append(&sync).unwrap();
        menu.append(&quit).unwrap();
        let (show_id, sync_id, quit_id) = (show.id().clone(), sync.id().clone(), quit.id().clone());
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("OneDrive Sync")
            .with_icon(make_icon([16, 124, 16]))
            .build()
            .expect("tray");

        let mut app = Self {
            paths,
            config,
            rows: vec![],
            last_run: String::new(),
            busy: false,
            busy_msg: String::new(),
            rx: None,
            _tray: tray,
            show_id,
            sync_id,
            quit_id,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        let state = MachineState::load(&self.paths);
        let catalog = Catalog::load(&self.paths);
        let projects = discover(&self.paths, &self.config, &catalog);
        let att = events::attention_ids(&self.paths);
        self.rows = projects
            .iter()
            .map(|p| Row {
                kind: p.kind.as_str(),
                git: p.git,
                status: state.status_of(&p.id).as_str().to_string(),
                attention: att.iter().any(|a| a.eq_ignore_ascii_case(&p.id)),
                id: p.id.clone(),
            })
            .collect();
        self.last_run = events::last_run_end(&self.paths)
            .and_then(|e| e.summary)
            .unwrap_or_else(|| "(none)".to_string());
        // tray colour: red if anything needs attention, else green.
        let any_att = self.rows.iter().any(|r| r.attention);
        let _ = self
            ._tray
            .set_icon(Some(make_icon(if any_att { [200, 0, 0] } else { [16, 124, 16] })));
    }

    fn start_sync(&mut self, dry_run: bool, id: Option<String>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.busy_msg = match &id {
            Some(i) => format!("resyncing {i}…"),
            None => if dry_run { "previewing…".into() } else { "syncing all…".into() },
        };
        let (tx, rx) = channel();
        let paths = self.paths.clone();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let summary = match id {
                Some(want) => {
                    let state = MachineState::load(&paths);
                    let catalog = Catalog::load(&paths);
                    let projects = discover(&paths, &config, &catalog);
                    match projects.iter().find(|p| p.id.eq_ignore_ascii_case(&want)) {
                        Some(p) => {
                            let code = bisync(
                                &paths,
                                &config,
                                &state,
                                p,
                                BisyncOpts {
                                    resync: true,
                                    dry_run,
                                    ..Default::default()
                                },
                            );
                            format!("resync {want}: code {code}")
                        }
                        None => format!("no project '{want}'"),
                    }
                }
                None => run(
                    &paths,
                    &config,
                    RunOpts {
                        dry_run,
                        ignore_pause: dry_run,
                    },
                ),
            };
            let _ = tx.send(summary);
        });
        self.rx = Some(rx);
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Tray menu events.
        let mut want_sync = false;
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.quit_id {
                std::process::exit(0);
            } else if ev.id == self.show_id {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if ev.id == self.sync_id {
                want_sync = true;
            }
        }
        while TrayIconEvent::receiver().try_recv().is_ok() {}
        if want_sync {
            self.start_sync(false, None);
        }

        // Collect a completed background sync.
        if let Some(rx) = &self.rx {
            if let Ok(summary) = rx.try_recv() {
                self.busy = false;
                if !summary.is_empty() {
                    self.last_run = summary;
                }
                self.rx = None;
                self.refresh();
            }
        }

        ui.horizontal(|ui| {
            ui.heading("OneDrive Sync");
            if self.busy {
                ui.spinner();
                ui.label(&self.busy_msg);
            }
        });
        ui.label(format!("last run: {}", self.last_run));
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                self.refresh();
            }
            if ui
                .add_enabled(!self.busy, egui::Button::new("Dry-run all"))
                .clicked()
            {
                self.start_sync(true, None);
            }
            if ui
                .add_enabled(!self.busy, egui::Button::new("Sync all"))
                .clicked()
            {
                self.start_sync(false, None);
            }
        });
        ui.separator();

        let mut resync: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("projects")
                .striped(true)
                .num_columns(5)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.strong("status");
                    ui.strong("kind");
                    ui.strong("git");
                    ui.strong("project");
                    ui.strong("");
                    ui.end_row();
                    for r in &self.rows {
                        let color = if r.attention {
                            egui::Color32::from_rgb(200, 0, 0)
                        } else if r.status == "active" {
                            egui::Color32::from_rgb(16, 124, 16)
                        } else {
                            egui::Color32::GRAY
                        };
                        let label = if r.attention {
                            format!("● {}", r.status)
                        } else {
                            r.status.clone()
                        };
                        ui.colored_label(color, label);
                        ui.label(r.kind);
                        ui.label(if r.git { "git" } else { "—" });
                        ui.monospace(&r.id);
                        if ui
                            .add_enabled(!self.busy, egui::Button::new("Resync"))
                            .on_hover_text("Re-establish the baseline (recover a stuck project)")
                            .clicked()
                        {
                            resync = Some(r.id.clone());
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some(id) = resync {
            self.start_sync(false, Some(id));
        }

        ui.ctx().request_repaint_after(Duration::from_millis(300));
    }
}

/// Launch the management window + tray (blocks on the event loop).
pub fn run_gui(paths: Paths, config: Config) -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "OneDrive Sync",
        native_options,
        Box::new(move |_cc| Ok(Box::new(GuiApp::new(paths, config)))),
    )
}
