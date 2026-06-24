//! Native management window (egui) + system tray, wired to the shared action
//! layer. Syncs run on a background thread so the UI never blocks. egui is
//! immediate-mode, so there is no retained-mode/dynamic-scope crash surface —
//! the class that plagued the WPF tray.
//!
//! The styling follows mainstream desktop-UI guidance: WCAG-AA contrast (light
//! text on a dark surface, status shown by colour AND text/badge, never colour
//! alone), generous spacing and click targets, a clear visual hierarchy, an
//! always-visible system-status line, and an accessibility text-zoom control.

use crate::config::Config;
use crate::discovery::discover;
use crate::paths::Paths;
use crate::run::{run, RunOpts};
use crate::state::{Catalog, MachineState, Status};
use crate::{actions, conflicts, events, icon};
use eframe::egui;
use egui::{Color32, RichText};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

// Semantic palette (chosen for >= 4.5:1 contrast of the label text on each fill).
const C_OK: Color32 = Color32::from_rgb(34, 139, 58);
const C_SKIP: Color32 = Color32::from_rgb(86, 92, 102);
const C_UNDECIDED: Color32 = Color32::from_rgb(176, 132, 24);
const C_ATTENTION: Color32 = Color32::from_rgb(206, 56, 56);
const C_ACCENT: Color32 = Color32::from_rgb(74, 142, 240);
const C_TEXT: Color32 = Color32::from_rgb(232, 234, 238);
const C_DIM: Color32 = Color32::from_rgb(150, 156, 166);

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
    filter: String,
    zoom: f32,
    logo: Option<egui::TextureHandle>,
    _tray: TrayIcon,
    show_id: MenuId,
    sync_id: MenuId,
    quit_id: MenuId,
}

fn make_icon(rgb: [u8; 3]) -> Icon {
    Icon::from_rgba(icon::rgba(32, rgb), 32, 32).expect("icon")
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

/// A colour+text status chip (never colour alone — the word carries the meaning too).
fn badge(ui: &mut egui::Ui, text: &str, fill: Color32, fg: Color32) {
    ui.label(RichText::new(format!(" {text} ")).color(fg).background_color(fill).strong());
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
            .with_icon(make_icon(icon::BRAND))
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
            filter: String::new(),
            zoom: 1.0,
            logo: None,
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
        let _ = self._tray.set_icon(Some(make_icon(if any_att { [206, 56, 56] } else { icon::BRAND })));
    }

    fn select(&mut self, id: &str) {
        self.selected = Some(id.to_string());
        self.sel_compare = self.state.compare.get(id).cloned().unwrap_or_else(|| self.config.compare_mode.clone());
        self.sel_maxdelete = self.state.max_delete.get(id).map(|n| n.to_string()).unwrap_or_default();
        self.conflict_view = None;
    }

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

    fn set_zoom(&mut self, ctx: &egui::Context, z: f32) {
        self.zoom = z.clamp(0.8, 1.8);
        ctx.set_zoom_factor(self.zoom);
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.logo.is_none() {
            let img = egui::ColorImage::from_rgba_unmultiplied([48, 48], &icon::rgba(48, icon::BRAND));
            self.logo = Some(ui.ctx().load_texture("ods-logo", img, egui::TextureOptions::LINEAR));
        }

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
        ui.add_space(4.0);
        self.tab_bar(ui);
        ui.add_space(6.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.view {
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
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(egui::vec2(28.0, 28.0)));
            }
            ui.heading("OneDrive Sync");
            if self.busy {
                ui.add_space(6.0);
                ui.spinner();
                ui.label(RichText::new(&self.busy_msg).color(C_ACCENT));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Pause / resume.
                if self.paused {
                    if ui.button(RichText::new("Resume").strong()).on_hover_text("Re-enable the scheduled sync").clicked() {
                        actions::resume(&self.paths);
                        self.refresh();
                    }
                    badge(ui, "PAUSED", C_UNDECIDED, Color32::BLACK);
                } else if ui.button("Pause").on_hover_text("Skip scheduled runs until resumed").clicked() {
                    actions::pause(&self.paths);
                    self.refresh();
                }
                ui.separator();
                // Accessibility: text size.
                let z = self.zoom;
                if ui.add_enabled(z < 1.79, egui::Button::new("A+")).on_hover_text("Larger text").clicked() {
                    self.set_zoom(ui.ctx(), z + 0.1);
                }
                ui.label(RichText::new(format!("{}%", (z * 100.0).round() as i32)).color(C_DIM));
                if ui.add_enabled(z > 0.81, egui::Button::new("A-")).on_hover_text("Smaller text").clicked() {
                    self.set_zoom(ui.ctx(), z - 0.1);
                }
            });
        });

        // System-status line: last run + a one-line action result.
        ui.horizontal(|ui| {
            ui.label(RichText::new("Last run:").color(C_DIM));
            let lr = self.last_run.clone();
            let col = if lr.contains("error") { C_ATTENTION } else if lr.contains("warn") { C_UNDECIDED } else { C_OK };
            ui.label(RichText::new(&lr).color(col).strong());
            if !self.status_msg.is_empty() {
                ui.separator();
                ui.label(RichText::new(&self.status_msg).color(C_DIM).italics());
            }
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                self.refresh();
            }
            if ui.add_enabled(!self.busy, egui::Button::new("Dry-run all")).on_hover_text("Preview every active project; changes nothing").clicked() {
                self.run_job("previewing…", |p, c| run(p, c, RunOpts { dry_run: true, ignore_pause: true }));
            }
            if ui.add_enabled(!self.busy, egui::Button::new(RichText::new("Sync all").strong())).clicked() {
                self.run_job("syncing all…", |p, c| run(p, c, RunOpts { dry_run: false, ignore_pause: true }));
            }
        });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.view, View::Projects, RichText::new("Projects").size(15.0));
            let plabel = if self.pending.is_empty() { "Pending".to_string() } else { format!("Pending ({})", self.pending.len()) };
            ui.selectable_value(&mut self.view, View::Pending, RichText::new(plabel).size(15.0));
            ui.selectable_value(&mut self.view, View::Retired, RichText::new("Retired").size(15.0));
            ui.selectable_value(&mut self.view, View::AddWatch, RichText::new("Add watch").size(15.0));
        });
        ui.separator();
    }

    fn view_projects(&mut self, ui: &mut egui::Ui) {
        // Legend (explains the colour coding — colour is never the only cue).
        ui.horizontal(|ui| {
            ui.label(RichText::new("Legend:").color(C_DIM).small());
            badge(ui, "active", C_OK, Color32::WHITE);
            badge(ui, "skip", C_SKIP, Color32::WHITE);
            badge(ui, "undecided", C_UNDECIDED, Color32::BLACK);
            badge(ui, "attention", C_ATTENTION, Color32::WHITE);
        });
        ui.add_space(2.0);
        // Filter box (recognition over recall; scales to many projects).
        ui.horizontal(|ui| {
            ui.label(RichText::new("Filter:").color(C_DIM));
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("type to filter projects…").desired_width(260.0));
            if !self.filter.is_empty() && ui.button("clear").clicked() {
                self.filter.clear();
            }
        });
        ui.add_space(4.0);

        let needle = self.filter.to_lowercase();
        let visible: Vec<usize> = (0..self.rows.len())
            .filter(|&i| needle.is_empty() || self.rows[i].id.to_lowercase().contains(&needle))
            .collect();

        let mut clicked: Option<String> = None;
        let mut resync: Option<String> = None;
        let mut sync: Option<String> = None;
        egui::Grid::new("projects").striped(true).num_columns(7).spacing([14.0, 9.0]).show(ui, |ui| {
            for h in ["STATUS", "KIND", "GIT", "LAST SYNC", "CONFLICTS", "PROJECT", ""] {
                ui.label(RichText::new(h).color(C_DIM).small().strong());
            }
            ui.end_row();
            for &i in &visible {
                let r = &self.rows[i];
                // status badge
                let (fill, fg) = match r.status.as_str() {
                    "active" => (C_OK, Color32::WHITE),
                    "skip" => (C_SKIP, Color32::WHITE),
                    _ => (C_UNDECIDED, Color32::BLACK),
                };
                if r.attention {
                    badge(ui, "attention", C_ATTENTION, Color32::WHITE);
                } else {
                    badge(ui, &r.status, fill, fg);
                }
                ui.label(r.kind);
                ui.label(if r.git { "git" } else { "-" });
                ui.label(RichText::new(r.last_sync.clone().unwrap_or_else(|| "-".into())).color(C_DIM));
                if r.conflicts > 0 {
                    badge(ui, &format!("{}", r.conflicts), C_ATTENTION, Color32::WHITE);
                } else {
                    ui.label(RichText::new("-").color(C_DIM));
                }
                let sel = self.selected.as_deref() == Some(r.id.as_str());
                if ui.selectable_label(sel, RichText::new(&r.id).monospace()).on_hover_text("Select for settings & actions").clicked() {
                    clicked = Some(r.id.clone());
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.busy, egui::Button::new("Sync")).on_hover_text("Sync this project now").clicked() {
                        sync = Some(r.id.clone());
                    }
                    if ui.add_enabled(!self.busy, egui::Button::new("Resync")).on_hover_text("Rebuild the baseline (recover a stuck project)").clicked() {
                        resync = Some(r.id.clone());
                    }
                });
                ui.end_row();
            }
        });
        if visible.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No projects match the filter.").color(C_DIM).italics());
        }

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
            ui.add_space(8.0);
            egui::Frame::group(ui.style()).show(ui, |ui| self.detail_panel(ui));
        }
    }

    fn detail_panel(&mut self, ui: &mut egui::Ui) {
        let id = self.selected.clone().unwrap();
        ui.horizontal(|ui| {
            ui.heading(RichText::new(&id).monospace().size(17.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("close").clicked() {
                    self.selected = None;
                }
            });
        });
        if self.selected.is_none() {
            return;
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Compare:");
            egui::ComboBox::from_id_salt("compare").selected_text(&self.sel_compare).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.sel_compare, "modtime".to_string(), "modtime (fast)");
                ui.selectable_value(&mut self.sel_compare, "checksum".to_string(), "checksum (exact)");
            });
            ui.add_space(12.0);
            ui.label("Max-delete %:");
            ui.add(egui::TextEdit::singleline(&mut self.sel_maxdelete).hint_text("default").desired_width(56.0));
            if ui.button("Apply settings").on_hover_text("Save per-project compare mode & delete-brake").clicked() {
                let compare = if self.sel_compare == self.config.compare_mode { None } else { Some(self.sel_compare.as_str()) };
                let maxd = self.sel_maxdelete.trim().parse::<u32>().ok();
                crate::state::set_project_settings(&self.paths, &id, compare, maxd);
                self.status_msg = format!("settings applied to {id}");
                self.refresh();
            }
        });
        ui.add_space(6.0);
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
            if ui.button(RichText::new("Forget").color(C_ATTENTION)).on_hover_text("Retire globally (tombstone); reversible with Pull").clicked() {
                actions::forget(&self.paths, &id);
                self.status_msg = format!("forgot {id}");
                self.selected = None;
                self.refresh();
            }
        });

        if let Some((cid, files)) = self.conflict_view.clone() {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Conflicts in {cid}:")).strong());
            if files.is_empty() {
                ui.label(RichText::new("  (none)").color(C_DIM));
            } else {
                for f in files {
                    ui.monospace(f.display().to_string());
                }
            }
        }
    }

    fn view_pending(&mut self, ui: &mut egui::Ui) {
        if self.pending.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No new projects awaiting a decision.").color(C_DIM));
            return;
        }
        ui.label("New projects available to sync on this machine:");
        ui.add_space(4.0);
        let mut activate: Option<String> = None;
        let mut skip: Option<String> = None;
        egui::Grid::new("pending").striped(true).num_columns(4).spacing([14.0, 9.0]).show(ui, |ui| {
            for h in ["NAME", "KIND", "PROJECT", ""] {
                ui.label(RichText::new(h).color(C_DIM).small().strong());
            }
            ui.end_row();
            for p in &self.pending {
                ui.label(&p.name);
                ui.label(&p.kind);
                ui.monospace(&p.id);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.busy, egui::Button::new(RichText::new("Activate").strong())).clicked() {
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
            ui.add_space(8.0);
            ui.label(RichText::new("No retired (tombstoned) projects.").color(C_DIM));
            return;
        }
        ui.label("Retired projects — Revive re-activates and syncs them here:");
        ui.add_space(4.0);
        let mut revive: Option<String> = None;
        egui::Grid::new("retired").striped(true).num_columns(2).spacing([14.0, 9.0]).show(ui, |ui| {
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
        ui.add_space(6.0);
        egui::Grid::new("addwatch").num_columns(2).spacing([10.0, 10.0]).show(ui, |ui| {
            ui.label("Local folder (under your profile):");
            ui.add(egui::TextEdit::singleline(&mut self.watch_local).hint_text(r"C:\Users\you\Code\my-project").desired_width(440.0));
            ui.end_row();
            ui.label("OneDrive destination (under OneDrive):");
            ui.add(egui::TextEdit::singleline(&mut self.watch_dest).hint_text(r"…\OneDrive\Tools\my-project").desired_width(440.0));
            ui.end_row();
        });
        ui.add_space(6.0);
        if ui.button(RichText::new("Add watch mapping").strong()).clicked() {
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

/// Tuned dark theme: high-contrast text, generous spacing, comfortable click
/// targets, and a readable type scale.
fn configure_style(ctx: &egui::Context) {
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    let mut style = (*ctx.global_style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::new(22.0, Proportional)),
        (TextStyle::Body, FontId::new(15.0, Proportional)),
        (TextStyle::Monospace, FontId::new(14.0, egui::FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(15.0, Proportional)),
        (TextStyle::Small, FontId::new(12.0, Proportional)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.interact_size.y = 28.0;

    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(C_TEXT);
    v.panel_fill = Color32::from_rgb(24, 26, 31);
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(74, 142, 240, 90);
    v.selection.stroke = egui::Stroke::new(1.0, C_ACCENT);
    v.hyperlink_color = C_ACCENT;
    style.visuals = v;
    ctx.set_global_style(style);
}

/// Launch the management window + tray (blocks on the event loop).
pub fn run_gui(paths: Paths, config: Config) -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 660.0])
            .with_min_inner_size([720.0, 480.0])
            .with_icon(egui::IconData { rgba: icon::rgba(256, icon::BRAND), width: 256, height: 256 }),
        ..Default::default()
    };
    eframe::run_native(
        "OneDrive Sync",
        native_options,
        Box::new(move |cc| {
            configure_style(&cc.egui_ctx);
            Ok(Box::new(GuiApp::new(paths, config)))
        }),
    )
}
