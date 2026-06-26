//! Native management window (egui) + system tray, wired to the shared action
//! layer. Syncs run on a background thread so the UI never blocks. egui is
//! immediate-mode, so there is no retained-mode/dynamic-scope crash surface —
//! the class that plagued the WPF tray.
//!
//! Layout follows mainstream desktop-app convention: a top title bar with the
//! global actions, a left navigation rail, a persistent bottom status bar
//! (Nielsen "visibility of system status"), and a central content area.
//!
//! Accessibility (grounded in WCAG 2.2 / Israel IS 5568 / US §508 — all == WCAG
//! 2.0 AA — plus GOV.UK and Material design-system guidance; see [`Palette`]):
//! - **Themes**: a full light AND dark [`Palette`], toggled at runtime and
//!   persisted (`config.theme`); the default "system" follows the OS theme
//!   where eframe reports one, otherwise dark. Every
//!   (foreground, background) pair is verified >= AA (body text clears AAA), and
//!   status is shown by colour AND a text badge (never colour alone). Dark
//!   surfaces follow Material (no pure black, lighter = higher elevation).
//! - **Font**: the OS-native Segoe UI (regular body, Semibold headings/buttons)
//!   replaces egui's thin Ubuntu-Light default; see [`install_fonts`].
//! - **Keyboard**: every action is reachable by Tab and activated with Enter /
//!   Space (egui built-in). F5 refreshes; Esc backs out of a confirm bar, then
//!   the detail drawer. Text zoom is Ctrl +/-/0 (egui native) as well as the
//!   A+/A- buttons; the on-screen % mirrors `ctx.zoom_factor()` either way.
//! - **Focus**: a keyboard-focused control draws a 2px accent ring (the `active`
//!   widget visuals), visibly distinct from the 1px hover stroke.
//! - **Screen reader**: eframe is built with AccessKit (incl. the Windows UIA
//!   backend), so the widget tree is exposed and every control's accessible name
//!   comes from its visible text (there are no icon-only controls). KNOWN LIMITS
//!   inherent to egui: `egui::Grid` emits no table/row/column semantics (the
//!   project list reads as a flat label sequence), and tooltip (`on_hover_text`)
//!   text is not a reliable a11y channel — so no control's meaning lives only in
//!   a tooltip.

use crate::config::Config;
use crate::discovery::discover;
use crate::paths::Paths;
use crate::run::{run, RunOpts};
use crate::state::{Catalog, MachineState, Status};
use crate::{actions, conflicts, events, icon};
use eframe::egui;
use egui::{Color32, RichText};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// A complete theme. Every (foreground, background) pair a label can land on is
/// verified >= WCAG-AA (4.5:1 text, 3:1 non-text); body text clears AAA (>= 7:1).
/// Grounded in: WCAG 2.2 / Israel IS 5568 (== WCAG 2.0 AA) / US §508 (USWDS) for
/// the thresholds; GOV.UK Design System for the light semantic hues; Material's
/// dark-theme guidance (no pure black, lighter surfaces = higher elevation,
/// desaturated accents, no pure-white-on-black) for the dark surfaces.
///
/// `accent` and several semantic colours are SPLIT by role: a colour bright
/// enough to read as text on the surface can't also be a white-text fill (the
/// two pull oppositely), so e.g. `accent` (ring/links) vs `accent_btn` (fill),
/// and `attention`/`ok` (fills) vs `attention_text`/`ok_text` (foreground).
#[derive(Clone, Copy)]
struct Palette {
    dark: bool,
    bg: Color32,   // central content
    nav: Color32,  // left rail
    bar: Color32,  // title + status bars
    card: Color32, // groups / detail panel
    text: Color32, // primary (high-emphasis) text
    dim: Color32,  // secondary (medium-emphasis) text
    accent: Color32,       // focus ring, selection, links, accent text
    accent_btn: Color32,   // primary-button fill (white label)
    ok: Color32,           // "active"/success badge fill
    ok_text: Color32,      // success as foreground text
    skip: Color32,         // "skip" badge fill
    undecided: Color32,    // "undecided"/warn fill AND foreground (passes both)
    attention: Color32,    // attention/error badge + button fill
    attention_text: Color32, // attention/error as foreground text
    warn_surface: Color32, // confirm-bar background
}

impl Palette {
    fn dark() -> Self {
        Self {
            dark: true,
            bg: rgb(22, 24, 29), nav: rgb(28, 30, 37), bar: rgb(17, 19, 23), card: rgb(33, 36, 44),
            text: rgb(232, 234, 238), dim: rgb(166, 172, 182),
            accent: rgb(59, 130, 246), accent_btn: rgb(37, 99, 201),
            ok: rgb(46, 125, 50), ok_text: rgb(78, 193, 123), skip: rgb(86, 92, 102),
            undecided: rgb(176, 132, 24), attention: rgb(206, 56, 56),
            attention_text: rgb(232, 112, 107), warn_surface: rgb(58, 37, 38),
        }
    }
    fn light() -> Self {
        Self {
            dark: false,
            bg: rgb(251, 252, 253), nav: rgb(238, 241, 244), bar: rgb(231, 235, 239), card: rgb(255, 255, 255),
            text: rgb(22, 25, 29), dim: rgb(86, 93, 100),
            accent: rgb(29, 112, 184), accent_btn: rgb(29, 112, 184),
            ok: rgb(15, 122, 82), ok_text: rgb(12, 106, 71), skip: rgb(95, 101, 109),
            undecided: rgb(120, 91, 0), attention: rgb(197, 48, 30),
            attention_text: rgb(197, 48, 30), warn_surface: rgb(252, 232, 230),
        }
    }
}

/// WCAG relative luminance of a colour (sRGB, gamma-expanded).
fn luminance(c: Color32) -> f32 {
    fn lin(u: u8) -> f32 {
        let c = u as f32 / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

/// Pick black or white text for the higher contrast on `fill` (so a badge stays
/// AA-legible whichever theme set the fill).
fn best_fg(fill: Color32) -> Color32 {
    let l = luminance(fill);
    let on_black = (l + 0.05) / 0.05;
    let on_white = 1.05 / (l + 0.05);
    if on_black >= on_white { Color32::BLACK } else { Color32::WHITE }
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Projects,
    Pending,
    Retired,
    AddFolders,
    Settings,
}

/// A pending destructive action awaiting a second (confirm) click.
enum Confirm {
    Restore { id: String, at: String, label: String },
    DeleteConflict { id: String, path: PathBuf },
    UnmapDeleteLocal { id: String },
    CleanDest { id: String },
}

/// Editable buffers for the Settings view (string-backed so partial edits are kept).
#[derive(Default)]
struct SettingsForm {
    loaded: bool,
    compare: String,
    max_delete: String,
    retention_days: String,
    max_gb: String,
    transfers: String,
    time_budget: String,
    idle_stability: String,
    project_parents: String,
    watch_roots: String,
    exclude_dirs: String,
    exclude_files: String,
    sync_anyway: String,
}

struct Row {
    id: String,
    kind: &'static str,
    git: bool,
    local: bool,
    status: String,
    attention: bool,
    conflicts: usize,
    compare: String,
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
    root_path: String,
    conflict_view: Option<(String, Vec<PathBuf>)>,
    clean_scan: Option<(String, actions::CleanScan)>,
    restore_runs: Option<Vec<actions::ArchiveRun>>,
    filter_text: Option<String>,
    confirm: Option<Confirm>,
    unmap_delete_local: bool,
    approve_deletes: bool,
    settings: SettingsForm,
    filter: String,
    zoom: f32,
    pal: Palette,
    bold: egui::FontFamily,
    applied_dark: Option<bool>,
    logo: Option<egui::TextureHandle>,
    _tray: TrayIcon,
    show_id: MenuId,
    sync_id: MenuId,
    quit_id: MenuId,
}

fn make_icon(rgb: [u8; 3]) -> Icon {
    Icon::from_rgba(icon::rgba(32, rgb), 32, 32).expect("icon")
}

/// A filled accent "primary" button. `fill` is the theme's `accent_btn`, chosen
/// so the white label clears AA (>= 5:1) in both themes.
fn primary(fill: Color32, text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).strong().color(Color32::WHITE)).fill(fill)
}

/// Open a folder in Explorer, fire-and-forget. (explorer.exe returns a nonzero
/// exit code even on success, so we never inspect the result.)
fn open_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// Reveal (select) a single file in Explorer, fire-and-forget.
fn reveal_in_explorer(file: &Path) {
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", file.display())).spawn();
}

/// A copy of `c` with the delete-brake lifted to 100% when "approve deletes" is on.
fn approve_cfg(c: &Config, approve: bool) -> Config {
    let mut c = c.clone();
    if approve {
        c.max_delete_percent = 100;
    }
    c
}

/// Split a multiline editor buffer into trimmed, non-empty lines.
fn lines_to_vec(s: &str) -> Vec<String> {
    s.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).map(String::from).collect()
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

/// Compact human-readable byte size (B / KB / MB / GB).
fn human_bytes(b: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Middle-ellipsis a long id so its meaningful leaf stays visible (full id on hover).
fn shorten(id: &str, max: usize) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= max {
        return id.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep / 2;
    let tail = keep - head;
    let h: String = chars[..head].iter().collect();
    let t: String = chars[chars.len() - tail..].iter().collect();
    format!("{h}…{t}")
}

/// A colour+text status chip (never colour alone — the word carries the meaning
/// too). The text colour is auto-picked for max contrast on `fill`, so the chip
/// stays AA-legible whichever theme supplied the fill.
fn badge(ui: &mut egui::Ui, text: &str, fill: Color32) {
    ui.label(RichText::new(format!(" {text} ")).color(best_fg(fill)).background_color(fill).strong());
}

impl GuiApp {
    fn new(paths: Paths, config: Config, pal: Palette, bold: egui::FontFamily) -> Self {
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
            root_path: String::new(),
            conflict_view: None,
            clean_scan: None,
            restore_runs: None,
            filter_text: None,
            confirm: None,
            unmap_delete_local: false,
            approve_deletes: false,
            settings: SettingsForm::default(),
            filter: String::new(),
            zoom: 1.0,
            pal,
            bold,
            // Start unset so the per-frame guard applies the theme on the FIRST
            // ui() frame. eframe resets visuals to its own default after the
            // creation closure, clobbering a style set only at creation — so if
            // we skipped the frame-1 apply, the theme wouldn't show until the
            // first manual toggle. (Regression fix: themes not taking effect
            // until you switch them.)
            applied_dark: None,
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
                local: p.local.exists(),
                status: self.state.status_of(&p.id).as_str().to_string(),
                attention: att.iter().any(|a| a.eq_ignore_ascii_case(&p.id)),
                conflicts: if p.local.exists() { conflicts::scan(p).len() } else { 0 },
                compare: self.state.compare.get(&p.id).cloned().unwrap_or_else(|| self.config.compare_mode.clone()),
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
        self.clean_scan = None;
        self.restore_runs = None;
        self.filter_text = None;
        self.confirm = None;
        self.unmap_delete_local = false;
    }

    /// Local + OneDrive paths for a project id (re-discovered on demand, click-time only).
    fn project_paths(&self, id: &str) -> Option<(PathBuf, PathBuf)> {
        discover(&self.paths, &self.config, &Catalog::load(&self.paths))
            .into_iter()
            .find(|p| p.id == id)
            .map(|p| (p.local, p.dest))
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

    /// Flip light/dark, persist the explicit choice, and let the per-frame guard
    /// re-apply the style (it detects the `pal.dark` change).
    fn toggle_theme(&mut self) {
        let to_dark = !self.pal.dark;
        self.pal = if to_dark { Palette::dark() } else { Palette::light() };
        self.config.theme = if to_dark { "dark" } else { "light" }.to_string();
        if let Err(e) = self.config.save(&self.paths) {
            self.status_msg = format!("theme not saved: {e}");
        }
    }

    /// Start a full run (sync or dry-run) honouring the "approve deletes" toggle.
    fn run_all(&mut self, msg: &str, dry: bool) {
        let approve = self.approve_deletes;
        self.run_job(msg, move |p, c| {
            let cfg = approve_cfg(c, approve);
            run(p, &cfg, RunOpts { dry_run: dry, ignore_pause: true })
        });
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Apply the theme's style whenever it changes (and on the first frame).
        // Re-derived from `self.pal` so a runtime toggle can never desync the
        // chrome from the surface colours the panels paint with.
        if self.applied_dark != Some(self.pal.dark) {
            configure_style(&ctx, self.bold.clone(), self.pal);
            self.applied_dark = Some(self.pal.dark);
        }

        // --- Once-per-frame housekeeping (must stay before the panels so the tray
        //     menu keeps pumping and background jobs are reaped). ---
        if self.logo.is_none() {
            let img = egui::ColorImage::from_rgba_unmultiplied([48, 48], &icon::rgba(48, icon::BRAND));
            self.logo = Some(ctx.load_texture("ods-logo", img, egui::TextureOptions::LINEAR));
        }
        let mut want_sync = false;
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.quit_id {
                std::process::exit(0);
            } else if ev.id == self.show_id {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if ev.id == self.sync_id {
                want_sync = true;
            }
        }
        while TrayIconEvent::receiver().try_recv().is_ok() {}
        if want_sync {
            self.run_all("syncing all…", false);
        }
        if let Some(rx) = &self.rx {
            if let Ok(result) = rx.try_recv() {
                self.busy = false;
                self.status_msg = result;
                self.rx = None;
                self.refresh();
            }
        }
        ctx.request_repaint_after(Duration::from_millis(400));

        // --- Keyboard accessibility: the whole app is operable without a mouse.
        //     egui already zooms text on Ctrl +/-/0; mirror its factor into our
        //     indicator so the % stays truthful whichever path changed it. Then
        //     add the two shortcuts users expect: F5 refresh, Esc to back out. ---
        self.zoom = ctx.zoom_factor();
        let has_modal = self.confirm.is_some();
        let has_selection = self.view == View::Projects && self.selected.is_some();
        let mut do_refresh = false;
        let mut do_escape = false;
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::F5) {
                do_refresh = true;
            }
            // Only swallow Escape when there's something to dismiss, so it still
            // reaches an open combo-box / popup the rest of the time.
            if (has_modal || has_selection) && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                do_escape = true;
            }
        });
        if do_refresh {
            self.refresh();
        }
        if do_escape {
            if self.confirm.is_some() {
                self.confirm = None; // cancel the pending destructive action first
            } else {
                self.selected = None; // then close the detail drawer
            }
        }

        // --- Chrome: title bar / nav rail / status bar / central content. Nested
        //     with show_inside since eframe::App::ui already hands us a CentralPanel Ui. ---
        let bar = egui::Frame::default().fill(self.pal.bar).inner_margin(egui::Margin::symmetric(14, 9));
        egui::Panel::top("titlebar").frame(bar).show_inside(ui, |ui| self.titlebar(ui));

        let status = egui::Frame::default().fill(self.pal.bar).inner_margin(egui::Margin::symmetric(14, 7));
        egui::Panel::bottom("statusbar").frame(status).show_inside(ui, |ui| self.statusbar(ui));

        let nav = egui::Frame::default().fill(self.pal.nav).inner_margin(egui::Margin::symmetric(10, 14));
        egui::Panel::left("nav").exact_size(172.0).resizable(false).frame(nav).show_inside(ui, |ui| self.nav(ui));

        // Selected-project detail as a fixed bottom drawer (actions stay visible
        // instead of scrolling off the end of a long grid).
        if self.view == View::Projects && self.selected.is_some() {
            let drawer = egui::Frame::default().fill(self.pal.card).inner_margin(egui::Margin::symmetric(18, 12));
            // Cap the drawer height and scroll inside it, so the inspect sections
            // (clean / conflicts / versions / filter) stay reachable at the default
            // window size instead of clipping below the fold.
            egui::Panel::bottom("detail").resizable(true).default_size(300.0).max_size(460.0).frame(drawer).show_inside(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.detail_panel(ui));
            });
        }

        let central = egui::Frame::default().fill(self.pal.bg).inner_margin(egui::Margin::symmetric(18, 14));
        egui::CentralPanel::default().frame(central).show_inside(ui, |ui| {
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| match self.view {
                View::Projects => self.view_projects(ui),
                View::Pending => self.view_pending(ui),
                View::Retired => self.view_retired(ui),
                View::AddFolders => self.view_add_folders(ui),
                View::Settings => self.view_settings(ui),
            });
        });
    }
}

impl GuiApp {
    fn titlebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(egui::vec2(26.0, 26.0)));
            }
            ui.add_space(4.0);
            ui.label(RichText::new("OneDrive Sync").size(19.0).strong());

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Pause / resume (rightmost).
                if self.paused {
                    if ui.add(primary(self.pal.accent_btn, "Resume")).on_hover_text("Re-enable the scheduled sync").clicked() {
                        actions::resume(&self.paths);
                        self.refresh();
                    }
                    badge(ui, "PAUSED", self.pal.undecided);
                } else if ui.button("Pause").on_hover_text("Skip scheduled runs until resumed").clicked() {
                    actions::pause(&self.paths);
                    self.refresh();
                }
                ui.separator();
                // Accessibility: text size.
                let z = self.zoom;
                if ui.add_enabled(z < 1.79, egui::Button::new("A+")).on_hover_text("Larger text  (Ctrl +)").clicked() {
                    self.set_zoom(ui.ctx(), z + 0.1);
                }
                ui.label(RichText::new(format!("{}%", (z * 100.0).round() as i32)).color(self.pal.dim).small())
                    .on_hover_text("Text size — Ctrl 0 resets to 100%");
                if ui.add_enabled(z > 0.81, egui::Button::new("A-")).on_hover_text("Smaller text  (Ctrl -)").clicked() {
                    self.set_zoom(ui.ctx(), z - 0.1);
                }
                ui.separator();
                // Appearance: light/dark theme toggle (label names the TARGET theme).
                let (tlabel, thover) = if self.pal.dark {
                    ("\u{2600} Light", "Switch to the light theme")
                } else {
                    ("\u{263E} Dark", "Switch to the dark theme")
                };
                if ui.button(tlabel).on_hover_text(thover).clicked() {
                    self.toggle_theme();
                }
                ui.separator();
                // Global run actions.
                if ui.add_enabled(!self.busy, primary(self.pal.accent_btn, "Sync all")).on_hover_text("Sync every active project now").clicked() {
                    self.run_all("syncing all…", false);
                }
                if ui.add_enabled(!self.busy, egui::Button::new("Dry-run all")).on_hover_text("Preview every active project; changes nothing").clicked() {
                    self.run_all("previewing…", true);
                }
                if ui.button("Refresh").clicked() {
                    self.refresh();
                }
                ui.add(egui::Checkbox::new(&mut self.approve_deletes, "Approve deletes"))
                    .on_hover_text("Lift the delete-brake to 100% for runs started here (allows mass deletions)");
            });
        });
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.label(RichText::new("VIEWS").size(11.0).color(self.pal.dim).strong());
        ui.add_space(8.0);
        self.nav_item(ui, View::Projects, "Projects".to_string());
        let plabel = if self.pending.is_empty() { "Pending".to_string() } else { format!("Pending  ({})", self.pending.len()) };
        self.nav_item(ui, View::Pending, plabel);
        self.nav_item(ui, View::Retired, "Retired".to_string());
        self.nav_item(ui, View::AddFolders, "Add folders".to_string());
        self.nav_item(ui, View::Settings, "Settings".to_string());
    }

    fn nav_item(&mut self, ui: &mut egui::Ui, view: View, label: String) {
        let selected = self.view == view;
        let resp = ui.add_sized(
            [ui.available_width(), 34.0],
            egui::Button::selectable(selected, RichText::new(label).size(15.0)),
        );
        if resp.clicked() {
            self.view = view;
        }
        ui.add_space(3.0);
    }

    fn statusbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Last run").color(self.pal.dim).small());
            let lr = self.last_run.clone();
            let col = if lr.contains("error") { self.pal.attention_text } else if lr.contains("warn") { self.pal.undecided } else { self.pal.ok_text };
            ui.label(RichText::new(&lr).color(col).strong());
            ui.separator();
            let n = self.rows.len();
            let att = self.rows.iter().filter(|r| r.attention || r.conflicts > 0).count();
            ui.label(RichText::new(format!("{n} projects")).color(self.pal.dim));
            if att > 0 {
                ui.label(RichText::new(format!("· {att} need attention")).color(self.pal.attention_text).strong());
            }
            if self.paused {
                ui.separator();
                badge(ui, "PAUSED", self.pal.undecided);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.busy {
                    ui.spinner();
                    ui.label(RichText::new(&self.busy_msg).color(self.pal.accent));
                } else if !self.status_msg.is_empty() {
                    ui.label(RichText::new(&self.status_msg).color(self.pal.dim).italics());
                }
            });
        });
    }

    fn view_projects(&mut self, ui: &mut egui::Ui) {
        // Legend (explains the colour coding — colour is never the only cue).
        ui.horizontal(|ui| {
            ui.label(RichText::new("Legend:").color(self.pal.dim).small());
            badge(ui, "active", self.pal.ok);
            badge(ui, "skip", self.pal.skip);
            badge(ui, "undecided", self.pal.undecided);
            badge(ui, "attention", self.pal.attention);
        });
        ui.add_space(4.0);
        // Filter box (recognition over recall; scales to many projects).
        ui.horizontal(|ui| {
            ui.label(RichText::new("Filter:").color(self.pal.dim));
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("type to filter projects…").desired_width(260.0));
            if !self.filter.is_empty() && ui.button("clear").clicked() {
                self.filter.clear();
            }
        });
        ui.add_space(6.0);

        let needle = self.filter.to_lowercase();
        let visible: Vec<usize> = (0..self.rows.len())
            .filter(|&i| needle.is_empty() || self.rows[i].id.to_lowercase().contains(&needle))
            .collect();

        let mut clicked: Option<String> = None;
        egui::Grid::new("projects").striped(true).num_columns(8).spacing([14.0, 10.0]).show(ui, |ui| {
            for h in ["STATUS", "KIND", "GIT", "LOCAL", "LAST SYNC", "CONFLICTS", "COMPARE", "PROJECT"] {
                ui.label(RichText::new(h).color(self.pal.dim).small().strong());
            }
            ui.end_row();
            for &i in &visible {
                let r = &self.rows[i];
                // status badge (attention overrides; both colour AND word).
                if r.attention {
                    badge(ui, "attention", self.pal.attention);
                } else {
                    let fill = match r.status.as_str() {
                        "active" => self.pal.ok,
                        "skip" => self.pal.skip,
                        _ => self.pal.undecided,
                    };
                    badge(ui, &r.status, fill);
                }
                ui.label(r.kind);
                ui.label(if r.git { "git" } else { "-" });
                if r.local {
                    ui.label(RichText::new("yes").color(self.pal.ok_text));
                } else {
                    ui.label(RichText::new("no").color(self.pal.dim));
                }
                ui.label(RichText::new(r.last_sync.clone().unwrap_or_else(|| "-".into())).color(self.pal.dim));
                if r.conflicts > 0 {
                    badge(ui, &format!("{}", r.conflicts), self.pal.attention);
                } else {
                    ui.label(RichText::new("-").color(self.pal.dim));
                }
                ui.label(RichText::new(&r.compare).color(self.pal.dim));
                let sel = self.selected.as_deref() == Some(r.id.as_str());
                if ui.selectable_label(sel, RichText::new(shorten(&r.id, 34)).monospace()).on_hover_text(&r.id).clicked() {
                    clicked = Some(r.id.clone());
                }
                ui.end_row();
            }
        });
        if visible.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No projects match the filter.").color(self.pal.dim).italics());
        }

        if let Some(id) = clicked {
            self.select(&id);
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

        // Pending destructive action: show a prominent confirm bar (two-click safety).
        self.confirm_bar(ui);

        ui.add_space(8.0);

        // Primary actions: sync / resync / open folders.
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.busy, primary(self.pal.accent_btn, "Sync")).on_hover_text("Sync this project now").clicked() {
                let pid = id.clone();
                let approve = self.approve_deletes;
                self.run_job(&format!("syncing {pid}…"), move |p, c| {
                    let cfg = approve_cfg(c, approve);
                    let st = MachineState::load(p);
                    let list = discover(p, &cfg, &Catalog::load(p));
                    match list.iter().find(|x| x.id == pid) {
                        Some(proj) => { let (s, _) = crate::run::sync_project(p, &cfg, &st, proj, false, false); format!("{pid}: {s}") }
                        None => format!("no project '{pid}'"),
                    }
                });
            }
            if ui.add_enabled(!self.busy, egui::Button::new("Resync")).on_hover_text("Rebuild the baseline (recover a stuck project)").clicked() {
                let pid = id.clone();
                self.run_job(&format!("resyncing {pid}…"), move |p, c| { actions::resync(p, c, Some(&pid)); format!("resync {pid} done") });
            }
            ui.separator();
            if ui.button("Open local").on_hover_text("Open the local folder in Explorer").clicked() {
                if let Some((local, _)) = self.project_paths(&id) {
                    open_in_explorer(&local);
                }
            }
            if ui.button("Open OneDrive").on_hover_text("Open the OneDrive destination in Explorer").clicked() {
                if let Some((_, dest)) = self.project_paths(&id) {
                    open_in_explorer(&dest);
                }
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Per-project settings.
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

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        // Inspect: conflicts / generated filter / version history (each a toggle).
        ui.horizontal(|ui| {
            if ui.button("View conflicts").on_hover_text("List unresolved rclone conflict copies").clicked() {
                if self.conflict_view.is_some() {
                    self.conflict_view = None;
                } else if let Some(p) = self.find_project(&id) {
                    self.conflict_view = Some((id.clone(), conflicts::scan(&p)));
                }
            }
            if ui.button("Show filter").on_hover_text("Show the generated rclone filter for this project").clicked() {
                if self.filter_text.is_some() {
                    self.filter_text = None;
                } else if let Some(p) = self.find_project(&id) {
                    self.filter_text = Some(crate::filter::generate(&p, &self.config));
                }
            }
            if ui.button("Versions…").on_hover_text("Restore this project from a local archived version").clicked() {
                if self.restore_runs.is_some() {
                    self.restore_runs = None;
                } else {
                    self.restore_runs = Some(actions::archive_runs(&self.paths, &id));
                }
            }
            if ui.button("Clean OneDrive").on_hover_text("Find filtered files (e.g. node_modules) that shouldn't be on OneDrive").clicked() {
                if self.clean_scan.is_some() {
                    self.clean_scan = None;
                } else if let Some(p) = self.find_project(&id) {
                    match actions::scan_dest_filtered(&self.config, &p) {
                        Ok(scan) => self.clean_scan = Some((id.clone(), scan)),
                        Err(e) => self.status_msg = e,
                    }
                }
            }
        });

        ui.add_space(8.0);

        // Lifecycle actions.
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.busy, egui::Button::new("Pull")).on_hover_text("Activate + sync here").clicked() {
                let pid = id.clone();
                self.run_job(&format!("pulling {pid}…"), move |p, c| match actions::pull(p, c, &pid) {
                    Ok(s) => format!("pulled {pid}: {s}"),
                    Err(e) => e,
                });
            }
            if ui.button("Unmap").on_hover_text("Stop syncing here").clicked() {
                if self.unmap_delete_local {
                    self.confirm = Some(Confirm::UnmapDeleteLocal { id: id.clone() });
                } else {
                    match actions::unmap(&self.paths, &self.config, &id, false) {
                        Ok(()) => self.status_msg = format!("unmapped {id} (OneDrive copy kept)"),
                        Err(e) => self.status_msg = e,
                    }
                    self.selected = None;
                    self.refresh();
                }
            }
            ui.checkbox(&mut self.unmap_delete_local, "delete local too")
                .on_hover_text("Also remove the local folder on Unmap (refused on a protected root)");
            if ui.button(RichText::new("Forget").color(self.pal.attention_text)).on_hover_text("Retire globally (tombstone); reversible with Pull").clicked() {
                actions::forget(&self.paths, &id);
                self.status_msg = format!("forgot {id}");
                self.selected = None;
                self.refresh();
            }
        });

        self.conflict_section(ui);
        self.clean_section(ui);
        self.restore_section(ui, &id);
        self.filter_section(ui);
    }

    /// The two-click confirm bar for a pending destructive action.
    fn confirm_bar(&mut self, ui: &mut egui::Ui) {
        let Some(c) = &self.confirm else { return };
        let msg = match c {
            Confirm::Restore { label, .. } => format!("Restore from {label}? This overwrites the local copy (a backup is taken first)."),
            Confirm::DeleteConflict { path, .. } => format!("Delete conflict copy '{}'?", path.file_name().unwrap_or_default().to_string_lossy()),
            Confirm::UnmapDeleteLocal { id } => format!("Unmap {id} AND delete the local folder? The OneDrive copy is kept."),
            Confirm::CleanDest { id } => {
                let (files, bytes) = self.clean_scan.as_ref().map(|(_, s)| (s.total_files, s.total_bytes)).unwrap_or((0, 0));
                format!("Delete {files} filtered file(s) ({}) from the OneDrive copy of {id}? They're excluded from sync (you regenerate them locally). git-ignored-only junk isn't included.", human_bytes(bytes))
            }
        };
        let mut go = false;
        let mut cancel = false;
        // Copy themed colours out before the closure (Palette is Copy; avoids
        // borrowing self inside the frame closure).
        let (warn_fill, text_col, btn_fill) = (self.pal.warn_surface, self.pal.text, self.pal.attention);
        ui.add_space(8.0);
        let warn = egui::Frame::default()
            .fill(warn_fill)
            .inner_margin(egui::Margin::same(10))
            .corner_radius(egui::CornerRadius::same(6));
        warn.show(ui, |ui| {
            ui.label(RichText::new(&msg).color(text_col).strong());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(RichText::new("Confirm").color(Color32::WHITE)).fill(btn_fill)).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if cancel {
            self.confirm = None;
        }
        if go {
            if let Some(c) = self.confirm.take() {
                self.dispatch_confirm(c);
            }
        }
    }

    fn dispatch_confirm(&mut self, c: Confirm) {
        match c {
            Confirm::Restore { id, at, label } => {
                self.run_job(&format!("restoring {id}…"), move |p, cfg| match actions::restore(p, cfg, &id, Some(&at), None) {
                    Ok(()) => format!("restored {id} from {label}"),
                    Err(e) => e,
                });
            }
            Confirm::DeleteConflict { id, path } => {
                match actions::delete_conflict(&self.paths, &path) {
                    Ok(()) => self.status_msg = format!("deleted conflict copy in {id}"),
                    Err(e) => self.status_msg = e,
                }
                if let Some(p) = self.find_project(&id) {
                    self.conflict_view = Some((id.clone(), conflicts::scan(&p)));
                }
                self.refresh();
            }
            Confirm::UnmapDeleteLocal { id } => {
                match actions::unmap(&self.paths, &self.config, &id, true) {
                    Ok(()) => self.status_msg = format!("unmapped {id} and deleted local"),
                    Err(e) => self.status_msg = e,
                }
                self.selected = None;
                self.refresh();
            }
            Confirm::CleanDest { id } => {
                let items = self.clean_scan.as_ref().map(|(_, s)| s.items.clone()).unwrap_or_default();
                self.clean_scan = None;
                self.run_job(&format!("cleaning OneDrive for {id}…"), move |p, c| {
                    let list = discover(p, c, &Catalog::load(p));
                    match list.iter().find(|x| x.id == id) {
                        Some(proj) => match actions::clean_scanned(p, c, proj, &items) {
                            Ok((f, b)) => format!("cleaned {f} filtered file(s), {} freed from {id}", human_bytes(b)),
                            Err(e) => e,
                        },
                        None => format!("no project '{id}'"),
                    }
                });
            }
        }
    }

    /// Conflict list with per-file open/delete (delete goes through the confirm bar).
    fn conflict_section(&mut self, ui: &mut egui::Ui) {
        let Some((cid, files)) = self.conflict_view.clone() else { return };
        ui.add_space(8.0);
        ui.label(RichText::new(format!("Conflicts in {cid}:")).strong());
        if files.is_empty() {
            ui.label(RichText::new("  (none)").color(self.pal.dim));
            return;
        }
        let mut del: Option<PathBuf> = None;
        egui::ScrollArea::vertical().max_height(150.0).id_salt("conflicts").show(ui, |ui| {
            for f in &files {
                ui.horizontal(|ui| {
                    if ui.button("Open").on_hover_text("Reveal in Explorer").clicked() {
                        reveal_in_explorer(f);
                    }
                    if ui.button(RichText::new("Delete").color(self.pal.attention_text)).clicked() {
                        del = Some(f.clone());
                    }
                    ui.monospace(f.display().to_string());
                });
            }
        });
        if let Some(path) = del {
            self.confirm = Some(Confirm::DeleteConflict { id: cid, path });
        }
    }

    /// Preview of filtered junk on the OneDrive copy; "Clean from OneDrive" stages
    /// a brake-by-confirm deletion in the confirm bar.
    fn clean_section(&mut self, ui: &mut egui::Ui) {
        let Some((cid, scan)) = &self.clean_scan else { return };
        let cid = cid.clone();
        ui.add_space(8.0);
        ui.label(RichText::new(format!("Filtered files on the OneDrive copy of {cid}:")).strong());
        if scan.items.is_empty() {
            ui.label(RichText::new("  (clean — nothing filtered is on OneDrive)").color(self.pal.ok_text));
            return;
        }
        ui.label(
            RichText::new(format!(
                "  {} entr(ies) · {} file(s) · {} — these are excluded from sync (git-ignored-only junk is not listed)",
                scan.items.len(),
                scan.total_files,
                human_bytes(scan.total_bytes)
            ))
            .color(self.pal.dim)
            .small(),
        );
        let (dim, mono) = (self.pal.dim, self.pal.dim);
        egui::ScrollArea::vertical().max_height(150.0).id_salt("clean").show(ui, |ui| {
            for it in &scan.items {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if it.is_dir { "DIR " } else { "file" }).color(mono).monospace().small());
                    ui.monospace(&it.rel);
                    ui.label(RichText::new(human_bytes(it.bytes)).color(dim).small());
                });
            }
        });
        let mut stage = false;
        if ui
            .add(egui::Button::new(RichText::new("Clean from OneDrive").color(Color32::WHITE)).fill(self.pal.attention))
            .on_hover_text("Delete these filtered files from the OneDrive copy (keeps everything tracked)")
            .clicked()
        {
            stage = true;
        }
        if stage {
            self.confirm = Some(Confirm::CleanDest { id: cid });
        }
    }

    /// Archived-version list; choosing one stages a restore in the confirm bar.
    fn restore_section(&mut self, ui: &mut egui::Ui, id: &str) {
        let Some(runs) = self.restore_runs.clone() else { return };
        ui.add_space(8.0);
        ui.label(RichText::new("Restore from a local archived version:").strong());
        if runs.is_empty() {
            ui.label(RichText::new("  (no archived versions yet — try OneDrive version history)").color(self.pal.dim));
            return;
        }
        let mut pick: Option<(String, String)> = None;
        egui::ScrollArea::vertical().max_height(150.0).id_salt("versions").show(ui, |ui| {
            for r in &runs {
                ui.horizontal(|ui| {
                    if ui.button("Restore").clicked() {
                        pick = Some((r.at.clone(), r.label.clone()));
                    }
                    ui.label(&r.label);
                });
            }
        });
        if let Some((at, label)) = pick {
            self.confirm = Some(Confirm::Restore { id: id.to_string(), at, label });
        }
    }

    fn filter_section(&mut self, ui: &mut egui::Ui) {
        let Some(text) = self.filter_text.clone() else { return };
        ui.add_space(8.0);
        ui.label(RichText::new("Generated rclone filter:").strong());
        egui::ScrollArea::vertical().max_height(150.0).id_salt("filter").show(ui, |ui| {
            let mut t = text;
            ui.add(
                egui::TextEdit::multiline(&mut t)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .interactive(false),
            );
        });
    }

    /// Re-discover and find the selected project (click-time only).
    fn find_project(&self, id: &str) -> Option<crate::discovery::Project> {
        discover(&self.paths, &self.config, &Catalog::load(&self.paths)).into_iter().find(|p| p.id == id)
    }

    fn view_pending(&mut self, ui: &mut egui::Ui) {
        if self.pending.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No new projects awaiting a decision.").color(self.pal.dim));
            return;
        }
        ui.label("New projects available to sync on this machine:");
        ui.add_space(6.0);
        let mut activate: Option<String> = None;
        let mut skip: Option<String> = None;
        egui::Grid::new("pending").striped(true).num_columns(4).spacing([14.0, 10.0]).show(ui, |ui| {
            for h in ["NAME", "KIND", "PROJECT", ""] {
                ui.label(RichText::new(h).color(self.pal.dim).small().strong());
            }
            ui.end_row();
            for p in &self.pending {
                ui.label(&p.name);
                ui.label(&p.kind);
                ui.monospace(&p.id);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.busy, primary(self.pal.accent_btn, "Activate")).clicked() {
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
            ui.label(RichText::new("No retired (tombstoned) projects.").color(self.pal.dim));
            return;
        }
        ui.label("Retired projects — Revive re-activates and syncs them here:");
        ui.add_space(6.0);
        let mut revive: Option<String> = None;
        egui::Grid::new("retired").striped(true).num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
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

    fn view_add_folders(&mut self, ui: &mut egui::Ui) {
        ui.heading("Add folders");
        ui.add_space(8.0);

        // --- Section A: scan a parent folder for projects (a discovery root). ---
        ui.label(RichText::new("Scan a parent folder for projects").strong());
        ui.label(
            RichText::new("Auto-discovers every git project inside a folder — use this for a folder that holds many repos. New projects appear under Pending to activate.")
                .color(self.pal.dim)
                .small(),
        );
        ui.add_space(6.0);
        let roots: Vec<String> = self.config.project_parents.iter().chain(self.config.watch_roots.iter()).cloned().collect();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Current roots:").color(self.pal.dim).small());
            if roots.is_empty() {
                ui.label(RichText::new("(none)").color(self.pal.dim).small());
            } else {
                for r in &roots {
                    badge(ui, r, self.pal.skip);
                }
            }
        });
        ui.add_space(6.0);
        let mut add_root = false;
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.root_path)
                    .hint_text(r"C:\Users\you\OneDrive\Projects   or   C:\Users\you\Code")
                    .desired_width(440.0),
            );
            if ui.add(primary(self.pal.accent_btn, "Add discovery root")).clicked() {
                add_root = true;
            }
        });
        if add_root {
            let raw = self.root_path.trim().to_string();
            if raw.is_empty() {
                self.status_msg = "enter a folder path".into();
            } else {
                self.add_discovery_root(&raw);
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(12.0);

        // --- Section B: map a single folder to a specific OneDrive destination. ---
        ui.label(RichText::new("Map a single folder").strong());
        ui.label(RichText::new("Sync one local folder to a specific OneDrive destination (a watch project).").color(self.pal.dim).small());
        ui.add_space(6.0);
        egui::Grid::new("addwatch").num_columns(2).spacing([10.0, 12.0]).show(ui, |ui| {
            ui.label("Local folder (under your profile):");
            ui.add(egui::TextEdit::singleline(&mut self.watch_local).hint_text(r"C:\Users\you\Code\my-project").desired_width(440.0));
            ui.end_row();
            ui.label("OneDrive destination (under OneDrive):");
            ui.add(egui::TextEdit::singleline(&mut self.watch_dest).hint_text(r"…\OneDrive\Tools\my-project").desired_width(440.0));
            ui.end_row();
        });
        ui.add_space(8.0);
        if ui.add(primary(self.pal.accent_btn, "Map folder")).clicked() {
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

    /// Add a discovery root. Detects whether the folder is under OneDrive (a
    /// project-parent, scanned for .git children) or under the user profile (a
    /// watch-root) and stores the RELATIVE segment in the matching list — storing
    /// an absolute path or the wrong list would silently break discovery.
    fn add_discovery_root(&mut self, raw: &str) {
        let p = std::path::Path::new(raw);
        // OneDrive first: it lives under the profile, so it's the more specific root.
        if let Some(rel) = crate::paths::rel_under(p, &self.paths.onedrive).filter(|s| !s.is_empty()) {
            if self.config.project_parents.iter().any(|x| x.eq_ignore_ascii_case(&rel)) {
                self.status_msg = format!("already a project parent: {rel}");
                return;
            }
            self.config.project_parents.push(rel.clone());
            self.save_config_after_root(format!("added project parent (under OneDrive): {rel}"));
        } else if let Some(rel) = crate::paths::rel_under(p, &self.paths.user_profile).filter(|s| !s.is_empty()) {
            if self.config.watch_roots.iter().any(|x| x.eq_ignore_ascii_case(&rel)) {
                self.status_msg = format!("already a watch root: {rel}");
                return;
            }
            self.config.watch_roots.push(rel.clone());
            self.save_config_after_root(format!("added watch root (under profile): {rel}"));
        } else {
            self.status_msg = format!(
                "Folder must be UNDER your OneDrive ({}) or your profile ({}).",
                self.paths.onedrive.display(),
                self.paths.user_profile.display()
            );
        }
    }

    fn save_config_after_root(&mut self, ok_msg: String) {
        match self.config.save(&self.paths) {
            Ok(()) => {
                self.root_path.clear();
                self.settings.loaded = false; // Settings view re-reads the new roots
                self.status_msg = ok_msg;
                self.refresh();
            }
            Err(e) => self.status_msg = format!("save failed: {e}"),
        }
    }

    fn view_settings(&mut self, ui: &mut egui::Ui) {
        if !self.settings.loaded {
            self.load_settings();
        }
        ui.heading("Settings");
        ui.label(RichText::new(r"Edits write %LOCALAPPDATA%\onedrive-sync\config.toml; discovery picks them up on the next refresh.").color(self.pal.dim).small());
        ui.add_space(10.0);

        egui::Grid::new("set-paths").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            for (k, v) in [
                ("Local root", self.paths.local_root.display().to_string()),
                ("OneDrive", self.paths.onedrive.display().to_string()),
                ("Profile", self.paths.user_profile.display().to_string()),
            ] {
                ui.label(RichText::new(k).color(self.pal.dim));
                ui.monospace(v);
                ui.end_row();
            }
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(RichText::new("Sync defaults").strong());
        ui.horizontal_wrapped(|ui| {
            ui.label("Compare:");
            egui::ComboBox::from_id_salt("set-compare").selected_text(&self.settings.compare).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.settings.compare, "modtime".to_string(), "modtime (fast)");
                ui.selectable_value(&mut self.settings.compare, "checksum".to_string(), "checksum (exact)");
            });
            ui.add_space(10.0);
            ui.label("Max-delete %:");
            ui.add(egui::TextEdit::singleline(&mut self.settings.max_delete).desired_width(54.0));
            ui.add_space(10.0);
            ui.label("Transfers:");
            ui.add(egui::TextEdit::singleline(&mut self.settings.transfers).desired_width(46.0));
            ui.add_space(10.0);
            ui.label("Run budget (s):");
            ui.add(egui::TextEdit::singleline(&mut self.settings.time_budget).desired_width(62.0));
            ui.add_space(10.0);
            ui.label("Idle gate (s):");
            ui.add(egui::TextEdit::singleline(&mut self.settings.idle_stability).desired_width(54.0));
        });

        ui.add_space(8.0);
        ui.label(RichText::new("Versioning").strong());
        ui.horizontal(|ui| {
            ui.label("Retention (days):");
            ui.add(egui::TextEdit::singleline(&mut self.settings.retention_days).desired_width(54.0));
            ui.add_space(12.0);
            ui.label("Archive cap (GB):");
            ui.add(egui::TextEdit::singleline(&mut self.settings.max_gb).desired_width(54.0));
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(RichText::new("Discovery roots (one per line)").strong());
        egui::Grid::new("set-roots").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
            ui.label("Project parents (under OneDrive):");
            ui.add(egui::TextEdit::multiline(&mut self.settings.project_parents).desired_rows(2).desired_width(360.0));
            ui.end_row();
            ui.label("Watch roots (under profile):");
            ui.add(egui::TextEdit::multiline(&mut self.settings.watch_roots).desired_rows(2).desired_width(360.0));
            ui.end_row();
        });

        ui.add_space(8.0);
        ui.collapsing("Filters (advanced)", |ui| {
            egui::Grid::new("set-filters").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
                ui.label("Exclude dirs:");
                ui.add(egui::TextEdit::multiline(&mut self.settings.exclude_dirs).desired_rows(3).desired_width(360.0));
                ui.end_row();
                ui.label("Exclude files:");
                ui.add(egui::TextEdit::multiline(&mut self.settings.exclude_files).desired_rows(3).desired_width(360.0));
                ui.end_row();
                ui.label("Sync anyway (force-include):");
                ui.add(egui::TextEdit::multiline(&mut self.settings.sync_anyway).desired_rows(2).desired_width(360.0));
                ui.end_row();
            });
        });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.add(primary(self.pal.accent_btn, "Save settings")).clicked() {
                self.save_settings();
            }
            if ui.button("Reload").on_hover_text("Discard edits and reload from disk").clicked() {
                self.settings.loaded = false;
            }
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(RichText::new("Maintenance").strong());
        ui.horizontal_wrapped(|ui| {
            if ui.button("Write diagnostics").on_hover_text("Write a diagnostic bundle to %TEMP% and reveal it").clicked() {
                match actions::diag(&self.paths, &self.config) {
                    Ok(p) => { self.status_msg = format!("diagnostics: {}", p.display()); reveal_in_explorer(&p); }
                    Err(e) => self.status_msg = e,
                }
            }
            if ui.button("Open config file").clicked() {
                let cf = self.paths.config_file();
                if cf.exists() { reveal_in_explorer(&cf); } else { open_in_explorer(&self.paths.local_root); }
            }
            if ui.button("Open log folder").clicked() {
                open_in_explorer(&self.paths.logs_dir());
            }
            if ui.button("Open state folder").clicked() {
                open_in_explorer(&self.paths.local_root);
            }
        });
    }

    fn load_settings(&mut self) {
        let c = &self.config;
        self.settings = SettingsForm {
            loaded: true,
            compare: c.compare_mode.clone(),
            max_delete: c.max_delete_percent.to_string(),
            retention_days: c.version_retention_days.to_string(),
            max_gb: c.version_max_gb.to_string(),
            transfers: c.rclone_transfers.to_string(),
            time_budget: c.run_time_budget.to_string(),
            idle_stability: c.idle_stability_seconds.to_string(),
            project_parents: c.project_parents.join("\n"),
            watch_roots: c.watch_roots.join("\n"),
            exclude_dirs: c.exclude_dirs.join("\n"),
            exclude_files: c.exclude_files.join("\n"),
            sync_anyway: c.sync_anyway.join("\n"),
        };
    }

    fn save_settings(&mut self) {
        let mut c = self.config.clone();
        {
            let s = &self.settings;
            if !s.compare.trim().is_empty() {
                c.compare_mode = s.compare.trim().to_string();
            }
            if let Ok(v) = s.max_delete.trim().parse() { c.max_delete_percent = v; }
            if let Ok(v) = s.retention_days.trim().parse() { c.version_retention_days = v; }
            if let Ok(v) = s.max_gb.trim().parse() { c.version_max_gb = v; }
            if let Ok(v) = s.transfers.trim().parse() { c.rclone_transfers = v; }
            if let Ok(v) = s.time_budget.trim().parse() { c.run_time_budget = v; }
            if let Ok(v) = s.idle_stability.trim().parse() { c.idle_stability_seconds = v; }
            c.project_parents = lines_to_vec(&s.project_parents);
            c.watch_roots = lines_to_vec(&s.watch_roots);
            c.exclude_dirs = lines_to_vec(&s.exclude_dirs);
            c.exclude_files = lines_to_vec(&s.exclude_files);
            c.sync_anyway = lines_to_vec(&s.sync_anyway);
        }
        match c.save(&self.paths) {
            Ok(()) => {
                self.config = c;
                self.settings.loaded = false; // re-read normalised values next frame
                self.status_msg = "settings saved".into();
                self.refresh();
            }
            Err(e) => self.status_msg = format!("save failed: {e}"),
        }
    }
}

/// Install the native Windows UI font (Segoe UI) so body text isn't egui's
/// default Ubuntu-Light, which reads far too thin. Returns the family to use
/// for headings/buttons — Segoe UI Semibold if present, else the regular
/// proportional family. Falls back silently to egui's defaults if the font
/// files are missing (we read the OS-installed copies, we don't bundle them).
fn install_fonts(ctx: &egui::Context) -> egui::FontFamily {
    use egui::FontFamily;
    fn add(fonts: &mut egui::FontDefinitions, key: &str, file: &str) -> bool {
        match std::fs::read(format!(r"C:\Windows\Fonts\{file}")) {
            Ok(b) => {
                fonts.font_data.insert(key.to_string(), std::sync::Arc::new(egui::FontData::from_owned(b)));
                true
            }
            Err(_) => false,
        }
    }
    let mut fonts = egui::FontDefinitions::default();
    let have_regular = add(&mut fonts, "segoe", "segoeui.ttf"); // regular (400), NOT Light
    let have_semibold = add(&mut fonts, "segoe-sb", "seguisb.ttf"); // semibold (600)
    let have_mono = add(&mut fonts, "consolas", "consola.ttf");
    if have_regular {
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    if have_mono {
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "consolas".into());
    }
    let bold = if have_semibold {
        let fam = FontFamily::Name("semibold".into());
        fonts.families.insert(fam.clone(), vec!["segoe-sb".into(), "segoe".into()]);
        fam
    } else {
        FontFamily::Proportional
    };
    ctx.set_fonts(fonts);
    bold
}

/// Tuned theme: layered surfaces, rounded widgets, high-contrast text, generous
/// spacing, comfortable click targets, and a readable type scale. `bold` is the
/// heavier family installed by [`install_fonts`], used for headings and buttons.
fn configure_style(ctx: &egui::Context, bold: egui::FontFamily, pal: Palette) {
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    let mut style = (*ctx.global_style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, bold.clone())),
        (TextStyle::Body, FontId::new(15.0, Proportional)),
        (TextStyle::Monospace, FontId::new(14.0, egui::FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(15.0, bold)),
        (TextStyle::Small, FontId::new(12.5, Proportional)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    style.visuals = make_visuals(pal);
    ctx.set_global_style(style);
}

/// Visuals derived from a palette. Starts from egui's accessible light/dark
/// defaults (so the per-state widget fills are sensible in BOTH themes) and
/// overrides only the surfaces, accent, selection, corner radii, and the
/// keyboard-focus ring — the parts that carry our identity and accessibility.
fn make_visuals(pal: Palette) -> egui::Visuals {
    use egui::{CornerRadius, Stroke};
    let mut v = if pal.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.override_text_color = Some(pal.text);
    v.panel_fill = pal.bg;
    v.window_fill = pal.card;
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    let a = pal.accent;
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), if pal.dark { 70 } else { 60 });
    v.selection.stroke = Stroke::new(1.0, a);
    v.hyperlink_color = a;
    let cr = CornerRadius::same(7);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = cr;
    }
    // Hover = 1px accent stroke; `active` is egui's keyboard-FOCUS state, so a 2px
    // accent ring there is the visible focus indicator, distinct from hover.
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, a);
    v.widgets.hovered.expansion = 1.0;
    v.widgets.active.bg_stroke = Stroke::new(2.0, a);
    v.widgets.active.expansion = 1.0;
    v
}

/// Launch the management window + tray (blocks on the event loop).
pub fn run_gui(paths: Paths, config: Config) -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Sized so the 8-column project grid fits without horizontal scroll at the
            // default size; shrinking below this scrolls rather than clips.
            .with_inner_size([1260.0, 740.0])
            .with_min_inner_size([1000.0, 560.0])
            .with_icon(egui::IconData { rgba: icon::rgba(256, icon::BRAND), width: 256, height: 256 }),
        ..Default::default()
    };
    eframe::run_native(
        "OneDrive Sync",
        native_options,
        Box::new(move |cc| {
            let bold = install_fonts(&cc.egui_ctx);
            // Resolve the theme: an explicit saved choice wins; "system" (the
            // default) follows whatever light/dark eframe detected from the OS.
            let dark = match config.theme.as_str() {
                "light" => false,
                "dark" => true,
                _ => cc.egui_ctx.global_style().visuals.dark_mode,
            };
            let pal = if dark { Palette::dark() } else { Palette::light() };
            configure_style(&cc.egui_ctx, bold.clone(), pal);
            Ok(Box::new(GuiApp::new(paths, config, pal, bold)))
        }),
    )
}
