//! Native management window (egui) + system tray, wired to the shared action
//! layer. Syncs run on a background thread so the UI never blocks. egui is
//! immediate-mode, so there is no retained-mode/dynamic-scope crash surface —
//! the class that plagued the WPF tray.

use crate::config::Config;
use crate::discovery::discover;
use crate::paths::Paths;
use crate::run::{run, RunOpts};
use crate::state::{Catalog, MachineState, Status};
use crate::{actions, conflicts, events};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(PartialEq, Clone, Copy)]
enum View {
    Projects,
    Pending,
    Retired,
    AddWatch,
}

struct Row {
    id: String,
    kind: &'static str,
    git: bool,
    status: String,
    attention: bool,
    conflicts: usize,
    last_sync: Option<String>,
}

struct PendingRow {
    id: String,
    name: String,
    kind: String,
}

struct GuiApp {
    paths: Paths,
    config: Config,
    state: MachineState,
    rows: Vec<Row>,
    pending: Vec<PendingRow>,
    forgotten: Vec<String>,
    last_run: String,
    paused: bool,
    busy: bool,
    busy_msg: String,
    status_msg: String,
    rx: Option<Receiver<String>>,
    view: View,
    selected: Option<String>,
    sel_compare: String,
    sel_maxdelete: String,
    watch_local: String,
    watch_dest: String,
    conflict_view: Option<(String, Vec<PathBuf>)>,
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

/// Format an ISO-8601 timestamp as a coarse "Nm ago" age.
fn ago(ts: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else { return "?".into() };
    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds();
    if secs < 0 {
        "just now".into()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
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
            state: MachineState::default(),
            rows: vec![],
            pending: vec![],
            forgotten: vec![],
            last_run: String::new(),
            paused: false,
            busy: false,
            busy_msg: String::new(),
            status_msg: String::new(),
            rx: None,
            view: View::Projects,
            selected: None,
            sel_compare: "modtime".into(),
            sel_maxdelete: String::new(),
            watch_local: String::new(),
            watch_dest: String::new(),
            conflict_view: None,
            _tray: tray,
            show_id,
            sync_id,
            quit_id,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.state = MachineState::load(&self.paths);
        let catalog = Catalog::load(&self.paths);
        let projects = discover(&self.paths, &self.config, &catalog);
        let att = events::attention_ids(&self.paths);
        let last_sync = events::last_sync_per_project(&self.paths);

        self.rows = projects
            .iter()
            .map(|p| Row {
                kind: p.kind.as_str(),
                git: p.git,
                status: self.state.status_of(&p.id).as_str().to_string(),
                attention: att.iter().any(|a| a.eq_ignore_ascii_case(&p.id)),
                conflicts: if p.local.exists() { conflicts::scan(p).len() } else { 0 },
                last_sync: last_sync.get(&p.id).map(|ts| ago(ts)),
                id: p.id.clone(),
            })
            .collect();

        self.pending = projects
            .iter()
            .filter(|p| self.state.status_of(&p.id) == Status::Undecided)
            .map(|p| PendingRow { id: p.id.clone(), name: p.name.clone(), kind: p.kind.as_str().to_string() })
            .collect();
        self.forgotten = catalog.forgotten.clone();
        self.paused = actions::is_paused(&self.paths);
        self.last_run = events::last_run_end(&self.paths)
            .and_then(|e| e.summary)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(none)".to_string());

        let any_att = self.rows.iter().any(|r| r.attention || r.conflicts > 0);
        let _ = self._tray.set_icon(Some(make_icon(if any_att { [200, 0, 0] } else { [16, 124, 16] })));
    }

    /// Load the selected project's per-project settings into the edit buffers.
    fn select(&mut self, id: &str) {
        self.selected = Some(id.to_string());
        self.sel_compare = self.state.compare.get(id).cloned().unwrap_or_else(|| self.config.compare_mode.clone());
        self.sel_maxdelete = self.state.max_delete.get(id).map(|n| n.to_string()).unwrap_or_default();
        self.conflict_view = None;
    }

    /// Spawn a (possibly slow) job on a background thread; result feeds the status line.
    fn run_job<F>(&mut self, msg: &str, f: F)
    where
        F: FnOnce(&Paths, &Config) -> String + Send + 'static,
    {
        if self.busy {
            return;
        }
        self.busy = true;
        self.busy_msg = msg.to_string();
        self.status_msg.clear();
        let (tx, rx) = channel();
        let paths = self.paths.clone();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f(&paths, &config));
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
            self.run_job("syncing all…", |p, c| run(p, c, RunOpts { dry_run: false, ignore_pause: true }));
        }

        // Collect a completed background job.
        if let Some(rx) = &self.rx {
            if let Ok(result) = rx.try_recv() {
                self.busy = false;
                self.status_msg = result;
                self.rx = None;
                self.refresh();
            }
        }

        self.header(ui);
        ui.separator();
        self.tab_bar(ui);
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| match self.view {
            View::Projects => self.view_projects(ui),
            View::Pending => self.view_pending(ui),
            View::Retired => self.view_retired(ui),
            View::AddWatch => self.view_add_watch(ui),
        });

        ui.ctx().request_repaint_after(Duration::from_millis(400));
    }
}

impl GuiApp {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("OneDrive Sync");
            if self.busy {
                ui.spinner();
                ui.label(&self.busy_msg);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (txt, on) = if self.paused { ("▶ Resume", true) } else { ("⏸ Pause", false) };
                if ui.button(txt).clicked() {
                    if on {
                        actions::resume(&self.paths);
                    } else {
                        actions::pause(&self.paths);
                    }
                    self.refresh();
                }
                if self.paused {
                    ui.colored_label(egui::Color32::from_rgb(200, 140, 0), "PAUSED");
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(format!("last run: {}", self.last_run));
            if ui.button("Refresh").clicked() {
                self.refresh();
            }
            if ui.add_enabled(!self.busy, egui::Button::new("Dry-run all")).clicked() {
                self.run_job("previewing…", |p, c| run(p, c, RunOpts { dry_run: true, ignore_pause: true }));
            }
            if ui.add_enabled(!self.busy, egui::Button::new("Sync all")).clicked() {
                self.run_job("syncing all…", |p, c| run(p, c, RunOpts { dry_run: false, ignore_pause: true }));
            }
        });
        if !self.status_msg.is_empty() {
            ui.colored_label(egui::Color32::GRAY, &self.status_msg);
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.view, View::Projects, "Projects");
            let plabel = if self.pending.is_empty() { "Pending".to_string() } else { format!("Pending ({})", self.pending.len()) };
            ui.selectable_value(&mut self.view, View::Pending, plabel);
            ui.selectable_value(&mut self.view, View::Retired, "Retired");
            ui.selectable_value(&mut self.view, View::AddWatch, "Add watch");
        });
    }

    fn view_projects(&mut self, ui: &mut egui::Ui) {
        let mut clicked: Option<String> = None;
        let mut resync: Option<String> = None;
        let mut sync: Option<String> = None;
        egui::Grid::new("projects").striped(true).num_columns(7).spacing([12.0, 6.0]).show(ui, |ui| {
            for h in ["status", "kind", "git", "last sync", "conflicts", "project", ""] {
                ui.strong(h);
            }
            ui.end_row();
            for r in &self.rows {
                let color = if r.attention || r.conflicts > 0 {
                    egui::Color32::from_rgb(200, 0, 0)
                } else if r.status == "active" {
                    egui::Color32::from_rgb(16, 124, 16)
                } else {
                    egui::Color32::GRAY
                };
                let label = if r.attention { format!("● {}", r.status) } else { r.status.clone() };
                ui.colored_label(color, label);
                ui.label(r.kind);
                ui.label(if r.git { "git" } else { "—" });
                ui.label(r.last_sync.clone().unwrap_or_else(|| "—".into()));
                if r.conflicts > 0 {
                    ui.colored_label(egui::Color32::from_rgb(200, 0, 0), format!("{} ⚠", r.conflicts));
                } else {
                    ui.label("—");
                }
                if ui.selectable_label(self.selected.as_deref() == Some(r.id.as_str()), egui::RichText::new(&r.id).monospace()).clicked() {
                    clicked = Some(r.id.clone());
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.busy, egui::Button::new("Sync")).clicked() {
                        sync = Some(r.id.clone());
                    }
                    if ui.add_enabled(!self.busy, egui::Button::new("Resync")).on_hover_text("Re-establish the baseline (recover a stuck project)").clicked() {
                        resync = Some(r.id.clone());
                    }
                });
                ui.end_row();
            }
        });
        if let Some(id) = clicked {
            self.select(&id);
        }
        if let Some(id) = sync {
            self.run_job(&format!("syncing {id}…"), move |p, c| {
                let st = MachineState::load(p);
                let list = discover(p, c, &Catalog::load(p));
                match list.iter().find(|x| x.id == id) {
                    Some(proj) => { let (s, _) = crate::run::sync_project(p, c, &st, proj, false, false); format!("{id}: {s}") }
                    None => format!("no project '{id}'"),
                }
            });
        }
        if let Some(id) = resync {
            self.run_job(&format!("resyncing {id}…"), move |p, c| { actions::resync(p, c, Some(&id)); format!("resync {id} done") });
        }

        if self.selected.is_some() {
            ui.separator();
            self.detail_panel(ui);
        }
    }

    fn detail_panel(&mut self, ui: &mut egui::Ui) {
        let id = self.selected.clone().unwrap();
        ui.heading(egui::RichText::new(&id).monospace());
        ui.horizontal(|ui| {
            ui.label("compare:");
            egui::ComboBox::from_id_salt("compare").selected_text(&self.sel_compare).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.sel_compare, "modtime".to_string(), "modtime");
                ui.selectable_value(&mut self.sel_compare, "checksum".to_string(), "checksum");
            });
            ui.label("max-delete % (blank = default):");
            ui.add(egui::TextEdit::singleline(&mut self.sel_maxdelete).desired_width(50.0));
            if ui.button("Apply settings").clicked() {
                let compare = if self.sel_compare == self.config.compare_mode { None } else { Some(self.sel_compare.as_str()) };
                let maxd = self.sel_maxdelete.trim().parse::<u32>().ok();
                crate::state::set_project_settings(&self.paths, &id, compare, maxd);
                self.status_msg = format!("settings applied to {id}");
                self.refresh();
            }
        });
        ui.horizontal(|ui| {
            if ui.button("View conflicts").clicked() {
                let list = discover(&self.paths, &self.config, &Catalog::load(&self.paths));
                if let Some(p) = list.iter().find(|p| p.id == id) {
                    self.conflict_view = Some((id.clone(), conflicts::scan(p)));
                }
            }
            if ui.add_enabled(!self.busy, egui::Button::new("Pull")).on_hover_text("Activate + sync here").clicked() {
                let pid = id.clone();
                self.run_job(&format!("pulling {pid}…"), move |p, c| match actions::pull(p, c, &pid) {
                    Ok(s) => format!("pulled {pid}: {s}"),
                    Err(e) => e,
                });
            }
            if ui.button("Unmap").on_hover_text("Stop syncing here; keep the OneDrive copy").clicked() {
                match actions::unmap(&self.paths, &self.config, &id, false) {
                    Ok(()) => self.status_msg = format!("unmapped {id}"),
                    Err(e) => self.status_msg = e,
                }
                self.selected = None;
                self.refresh();
            }
            if ui.button("Forget").on_hover_text("Retire globally (tombstone); reversible with Pull").clicked() {
                actions::forget(&self.paths, &id);
                self.status_msg = format!("forgot {id}");
                self.selected = None;
                self.refresh();
            }
        });

        if let Some((cid, files)) = self.conflict_view.clone() {
            ui.separator();
            ui.label(format!("Conflicts in {cid}:"));
            if files.is_empty() {
                ui.label("  (none)");
            } else {
                for f in files {
                    ui.monospace(f.display().to_string());
                }
            }
        }
    }

    fn view_pending(&mut self, ui: &mut egui::Ui) {
        if self.pending.is_empty() {
            ui.label("No new projects awaiting a decision.");
            return;
        }
        ui.label("New projects available to sync on this machine:");
        let mut activate: Option<String> = None;
        let mut skip: Option<String> = None;
        egui::Grid::new("pending").striped(true).num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
            for h in ["name", "kind", "project", ""] {
                ui.strong(h);
            }
            ui.end_row();
            for p in &self.pending {
                ui.label(&p.name);
                ui.label(&p.kind);
                ui.monospace(&p.id);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.busy, egui::Button::new("Activate")).clicked() {
                        activate = Some(p.id.clone());
                    }
                    if ui.button("Skip").clicked() {
                        skip = Some(p.id.clone());
                    }
                });
                ui.end_row();
            }
        });
        if let Some(id) = activate {
            self.run_job(&format!("pulling {id}…"), move |p, c| match actions::pull(p, c, &id) {
                Ok(s) => format!("activated {id}: {s}"),
                Err(e) => e,
            });
        }
        if let Some(id) = skip {
            crate::state::set_state(&self.paths, &id, Status::Skip);
            self.refresh();
        }
    }

    fn view_retired(&mut self, ui: &mut egui::Ui) {
        if self.forgotten.is_empty() {
            ui.label("No retired (tombstoned) projects.");
            return;
        }
        ui.label("Retired projects — Revive re-activates and syncs them here:");
        let mut revive: Option<String> = None;
        egui::Grid::new("retired").striped(true).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            for id in &self.forgotten {
                ui.monospace(id);
                if ui.add_enabled(!self.busy, egui::Button::new("Revive")).clicked() {
                    revive = Some(id.clone());
                }
                ui.end_row();
            }
        });
        if let Some(id) = revive {
            self.run_job(&format!("reviving {id}…"), move |p, c| match actions::pull(p, c, &id) {
                Ok(s) => format!("revived {id}: {s}"),
                Err(e) => e,
            });
        }
    }

    fn view_add_watch(&mut self, ui: &mut egui::Ui) {
        ui.label("Map a local folder to an arbitrary OneDrive destination (watch project).");
        egui::Grid::new("addwatch").num_columns(2).spacing([8.0, 8.0]).show(ui, |ui| {
            ui.label("Local folder (under your profile):");
            ui.add(egui::TextEdit::singleline(&mut self.watch_local).desired_width(420.0));
            ui.end_row();
            ui.label("OneDrive destination (under OneDrive):");
            ui.add(egui::TextEdit::singleline(&mut self.watch_dest).desired_width(420.0));
            ui.end_row();
        });
        if ui.button("Add watch mapping").clicked() {
            let (local, dest) = (self.watch_local.trim().to_string(), self.watch_dest.trim().to_string());
            if local.is_empty() || dest.is_empty() {
                self.status_msg = "both folders are required".into();
            } else {
                match actions::add_watch(&self.paths, std::path::Path::new(&local), std::path::Path::new(&dest)) {
                    Ok(id) => { self.status_msg = format!("mapped -> {id}"); self.watch_local.clear(); self.watch_dest.clear(); self.refresh(); }
                    Err(e) => self.status_msg = e,
                }
            }
        }
    }
}

/// Launch the management window + tray (blocks on the event loop).
pub fn run_gui(paths: Paths, config: Config) -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "OneDrive Sync",
        native_options,
        Box::new(move |_cc| Ok(Box::new(GuiApp::new(paths, config)))),
    )
}
