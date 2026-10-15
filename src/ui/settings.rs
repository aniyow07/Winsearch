//! Settings window with a section list on the left. Edits go to a draft
//! and apply on "Save".

use egui::{RichText, Ui};
use winsearch::config::AppConfig;
use winsearch::indexer::{Command, LiveMode};
use winsearch::platform::{self, DriveInfo};
use winsearch::util;

use super::{WinSearchApp, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Locations,
    Search,
    Indexing,
    Status,
}

impl Section {
    const ALL: [Section; 4] = [Section::Locations, Section::Search, Section::Indexing, Section::Status];

    fn title(self) -> &'static str {
        match self {
            Section::Locations => "Locations",
            Section::Search => "Search results",
            Section::Indexing => "Indexing",
            Section::Status => "Index status",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Section::Locations => "Drives and folders to index",
            Section::Search => "What results show",
            Section::Indexing => "Skipped folders, updates",
            Section::Status => "Health, errors, maintenance",
        }
    }
}

pub struct SettingsState {
    pub open: bool,
    draft: AppConfig,
    drives: Vec<DriveInfo>,
    new_exclude: String,
    new_clutter: String,
    confirm_rebuild: bool,
    elevated: bool,
    section: Section,
}

impl SettingsState {
    pub fn new(cfg: &AppConfig) -> SettingsState {
        SettingsState {
            open: false,
            draft: cfg.clone(),
            drives: Vec::new(),
            new_exclude: String::new(),
            new_clutter: String::new(),
            confirm_rebuild: false,
            elevated: platform::is_elevated(),
            section: Section::Locations,
        }
    }

    pub fn open_with(&mut self, cfg: &AppConfig) {
        self.draft = cfg.clone();
        self.drives = platform::list_drives();
        self.open = true;
        self.confirm_rebuild = false;
    }

    /// Mirrors options changed outside the window (toolbar toggles).
    pub fn sync_option(&mut self, cfg: &AppConfig) {
        self.draft.hide_clutter = cfg.hide_clutter;
        self.draft.recent_scopes = cfg.recent_scopes.clone();
        self.draft.clutter_folders = cfg.clutter_folders.clone();
    }
}

pub fn show(app: &mut WinSearchApp, ctx: &egui::Context) {
    if !app.settings.open {
        return;
    }
    let mut open = true;
    let mut save = false;
    let screen = ctx.content_rect();
    let size = egui::vec2(820.0f32.min(screen.width() - 40.0), 580.0f32.min(screen.height() - 60.0));
    egui::Window::new(RichText::new("Settings").font(theme::strong(16.0)))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .fixed_size(size)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(egui::Frame::window(&ctx.global_style()).inner_margin(egui::Margin::same(16)))
        .show(ctx, |ui| {
            let body_h = size.y - 70.0;
            ui.horizontal_top(|ui| {
                // Section list.
                ui.vertical(|ui| {
                    ui.set_width(180.0);
                    for s in Section::ALL {
                        let selected = app.settings.section == s;
                        let (r, resp) = ui.allocate_exact_size(egui::vec2(180.0, 46.0), egui::Sense::click());
                        let bg = if selected {
                            theme::ACCENT_DIM
                        } else if resp.hovered() {
                            theme::SURFACE
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        ui.painter().rect_filled(r, 8.0, bg);
                        if selected {
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(r.min + egui::vec2(0.0, 10.0), egui::vec2(3.0, 26.0)),
                                2.0,
                                theme::ACCENT,
                            );
                        }
                        ui.painter().text(
                            r.min + egui::vec2(14.0, 9.0),
                            egui::Align2::LEFT_TOP,
                            s.title(),
                            theme::strong(14.0),
                            theme::TEXT,
                        );
                        ui.painter().text(
                            r.min + egui::vec2(14.0, 27.0),
                            egui::Align2::LEFT_TOP,
                            s.subtitle(),
                            egui::FontId::proportional(11.5),
                            theme::TEXT_DIM,
                        );
                        if resp.clicked() {
                            app.settings.section = s;
                        }
                    }
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("settings-body")
                        .max_height(body_h)
                        .auto_shrink([false, false])
                        .show(ui, |ui| match app.settings.section {
                            Section::Locations => locations(app, ui),
                            Section::Search => search_options(app, ui),
                            Section::Indexing => indexing(app, ui),
                            Section::Status => status(app, ui),
                        });
                });
            });
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                let dirty = app.settings.draft != app.cfg;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let save_btn = egui::Button::new(RichText::new("Save").color(egui::Color32::WHITE))
                        .fill(if dirty { theme::ACCENT.gamma_multiply(0.75) } else { theme::SURFACE });
                    if ui.add_enabled(dirty, save_btn).clicked() {
                        save = true;
                    }
                    if ui.button(if dirty { "Discard" } else { "Close" }).clicked() {
                        app.settings.open = false;
                    }
                    if dirty {
                        ui.label(RichText::new("You have unsaved changes").color(theme::WARN));
                    }
                });
            });
        });
    if save {
        let mut cfg = app.settings.draft.clone();
        cfg.normalize();
        if let Err(e) = cfg.save() {
            app.toast(format!("Could not save settings: {e}"));
        }
        let index_changed = cfg.roots != app.cfg.roots || cfg.excludes != app.cfg.excludes;
        app.cfg = cfg.clone();
        app.settings.draft = cfg.clone();
        app.indexer.send(Command::Reconfigure(cfg));
        app.submit_search(true);
        app.toast(if index_changed { "Saved. Updating the index in the background.".into() } else { "Settings saved".into() });
    }
    if !open {
        app.settings.open = false;
        app.focus_search = true;
    }
}

fn section_title(ui: &mut Ui, title: &str, help: &str) {
    ui.label(RichText::new(title).font(theme::strong(17.0)));
    ui.add(egui::Label::new(RichText::new(help).color(theme::TEXT_DIM)).wrap());
    ui.add_space(10.0);
}

fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn path_list(ui: &mut Ui, id: &str, items: &mut Vec<String>, empty: &str) {
    let mut remove = None;
    card(ui, |ui| {
        if items.is_empty() {
            ui.label(RichText::new(empty).color(theme::TEXT_DIM));
        }
        for (i, x) in items.iter().enumerate() {
            ui.horizontal(|ui| {
                if id == "clutter" {
                    ui.label(winsearch::config::clutter_label(x));
                } else {
                    ui.label(x);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Remove").on_hover_text(format!("Remove {x}")).clicked() {
                        remove = Some(i);
                    }
                });
            });
            if i + 1 < items.len() {
                ui.separator();
            }
        }
    });
    if let Some(i) = remove {
        items.remove(i);
    }
    let _ = id;
}

fn locations(app: &mut WinSearchApp, ui: &mut Ui) {
    section_title(
        ui,
        "Locations",
        "WinSearch keeps an index of file and folder names in these places. Searching never scans the disk.",
    );
    let status = app.status.roots.clone();
    let mut remove = None;
    card(ui, |ui| {
        if app.settings.draft.roots.is_empty() {
            ui.colored_label(theme::WARN, "Nothing is indexed yet. Add a drive or a folder below.");
        }
        let n = app.settings.draft.roots.len();
        for (i, root) in app.settings.draft.roots.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(root).font(theme::strong(14.0)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            });
            match status.iter().find(|s| s.path.eq_ignore_ascii_case(root)) {
                Some(s) => {
                    let (txt, color) = match s.state.as_str() {
                        "complete" => (format!("Indexed · last full scan {}", util::format_time(s.last_complete_scan)), theme::OK),
                        "scanning" => ("Scan in progress, or interrupted (it resumes automatically)".to_string(), theme::WARN),
                        "unavailable" => ("Drive not available right now".to_string(), theme::ERR),
                        other => (other.to_string(), theme::TEXT_DIM),
                    };
                    ui.colored_label(color, txt);
                    let live = match &s.live {
                        LiveMode::Usn => "Live updates: NTFS change journal".to_string(),
                        LiveMode::Notifications => "Live updates: change notifications".to_string(),
                        LiveMode::Unavailable(e) => format!("No live updates: {e}"),
                    };
                    let r = ui.label(RichText::new(live).small().color(theme::TEXT_DIM));
                    if let Some(note) = &s.usn_note {
                        r.on_hover_text(note);
                    }
                    if s.errors > 0 {
                        ui.label(RichText::new(format!("{} folders could not be read (usually access denied)", s.errors)).small().color(theme::WARN))
                            .on_hover_text(s.last_error.clone().unwrap_or_default());
                    }
                }
                None => {
                    ui.label(RichText::new("New: indexed after you save").small().color(theme::ACCENT));
                }
            }
            if i + 1 < n {
                ui.separator();
            }
        }
    });
    if let Some(i) = remove {
        app.settings.draft.roots.remove(i);
    }
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        let drives = app.settings.drives.clone();
        ui.menu_button("Add a drive  ▾", |ui| {
            ui.set_min_width(280.0);
            for d in drives {
                let already = app.settings.draft.roots.iter().any(|r| r.eq_ignore_ascii_case(&d.root));
                let resp = ui.add_enabled(!already && d.ready, egui::Button::new(d.describe()));
                if resp.clicked() {
                    app.settings.draft.roots.push(d.root.clone());
                    app.settings.draft.excludes.push(format!("{}$Recycle.Bin", d.root));
                    app.settings.draft.excludes.push(format!("{}System Volume Information", d.root));
                    app.settings.draft.normalize();
                    ui.close();
                }
            }
        });
        if ui.button("Add a folder…").clicked()
            && let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder to index").pick_folder()
        {
            app.settings.draft.roots.push(util::normalize_dir(&dir.to_string_lossy()));
            app.settings.draft.normalize();
        }
    });
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(
            RichText::new("Entries of removable and network drives are kept while the drive is disconnected and refreshed when it returns.")
                .small()
                .color(theme::TEXT_DIM),
        )
        .wrap(),
    );
}

fn search_options(app: &mut WinSearchApp, ui: &mut Ui) {
    section_title(ui, "Search results", "These options change what you see. They do not change what is indexed.");
    let d = &mut app.settings.draft;
    card(ui, |ui| {
        ui.checkbox(&mut d.hide_clutter, RichText::new("Hide system, app-data and cache folders").font(theme::strong(14.0)));
        ui.label(
            RichText::new("Also available as the toggle next to the search box. Choosing one of these folders as the search scope always shows its contents.")
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(4.0);
        ui.checkbox(&mut d.show_hidden, "Show files marked hidden or system");
    });
    ui.add_space(12.0);
    ui.label(RichText::new("Folders hidden from results").font(theme::strong(14.0)));
    ui.label(
        RichText::new("A full path hides that folder; a plain name such as node_modules hides every folder with that name.")
            .small()
            .color(theme::TEXT_DIM),
    );
    ui.add_space(4.0);
    path_list(ui, "clutter", &mut d.clutter_folders, "Nothing is hidden.");
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut app.settings.new_clutter).hint_text("Folder name or full path").desired_width(280.0));
        if ui.button("Add").clicked() && !app.settings.new_clutter.trim().is_empty() {
            let v = app.settings.new_clutter.trim().to_string();
            let v = if v.contains(':') { util::normalize_dir(&v) } else { v };
            app.settings.draft.clutter_folders.push(v);
            app.settings.new_clutter.clear();
        }
        if ui.button("Restore defaults").clicked() {
            app.settings.draft.clutter_folders = winsearch::config::default_clutter_folders();
        }
    });
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label("Maximum results per search");
        ui.add(egui::DragValue::new(&mut app.settings.draft.result_limit).range(100..=100_000).speed(100));
    });
}

fn indexing(app: &mut WinSearchApp, ui: &mut Ui) {
    section_title(
        ui,
        "Indexing",
        "Folders excluded here are never scanned or stored. To only hide results, use Search results instead.",
    );
    ui.label(RichText::new("Excluded folders").font(theme::strong(14.0)));
    ui.add_space(4.0);
    path_list(ui, "excludes", &mut app.settings.draft.excludes, "Nothing is excluded.");
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut app.settings.new_exclude).hint_text("C:\\path\\to\\skip").desired_width(280.0));
        if ui.button("Add").clicked() && !app.settings.new_exclude.trim().is_empty() {
            let p = util::normalize_dir(&app.settings.new_exclude);
            app.settings.draft.excludes.push(p);
            app.settings.draft.normalize();
            app.settings.new_exclude.clear();
        }
        if ui.button("Browse…").clicked()
            && let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder to exclude").pick_folder()
        {
            app.settings.draft.excludes.push(util::normalize_dir(&dir.to_string_lossy()));
            app.settings.draft.normalize();
        }
    });
    ui.add_space(14.0);
    ui.label(RichText::new("Keeping the index current").font(theme::strong(14.0)));
    ui.add_space(4.0);
    let elevated = app.settings.elevated;
    let d = &mut app.settings.draft;
    card(ui, |ui| {
        ui.checkbox(&mut d.reconcile_on_startup, "Rescan in the background when WinSearch starts");
        ui.label(
            RichText::new("Finds changes made while the app was closed. Not needed on drives that use the NTFS change journal.")
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Full reconciliation every");
            ui.add(egui::DragValue::new(&mut d.reconcile_interval_hours).range(0..=168).suffix(" h"));
            ui.label(RichText::new("0 turns it off").small().color(theme::TEXT_DIM));
        });
        ui.horizontal(|ui| {
            ui.label("Scanner threads");
            ui.add(egui::DragValue::new(&mut d.scan_threads).range(1..=32));
        });
        ui.add_space(6.0);
        let note = if elevated {
            "Running as administrator: the NTFS change journal is used where available, so changes made while WinSearch was closed are picked up without rescanning."
        } else {
            "Running without administrator rights (normal). Live updates use Windows change notifications, backed up by reconciliation scans. Run as administrator to use the NTFS change journal instead."
        };
        ui.add(egui::Label::new(RichText::new(note).small().color(theme::TEXT_DIM)).wrap());
    });
}

fn status(app: &mut WinSearchApp, ui: &mut Ui) {
    section_title(ui, "Index status", "Health of the index and maintenance actions.");
    card(ui, |ui| {
        egui::Grid::new("diag").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
            let row = |ui: &mut Ui, k: &str, v: String| {
                ui.label(RichText::new(k).color(theme::TEXT_DIM));
                ui.add(egui::Label::new(v).truncate());
                ui.end_row();
            };
            row(ui, "Entries", util::format_count(app.status.total_entries));
            let size = std::fs::metadata(&app.db_path).map(|m| m.len()).unwrap_or(0);
            row(ui, "Index file", format!("{}  ({})", app.db_path.display(), util::format_size(size)));
            row(ui, "Logs", winsearch::config::log_dir().display().to_string());
            row(ui, "Changes applied", util::format_count(app.status.changes_applied));
            if app.status.scan_rate > 0.0 {
                row(ui, "Last scan speed", format!("{:.0} entries/s", app.status.scan_rate));
            }
            if !app.status.message.is_empty() {
                row(ui, "Last event", app.status.message.clone());
            }
        });
    });
    ui.add_space(12.0);
    ui.label(RichText::new("Maintenance").font(theme::strong(14.0)));
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if ui.button("Rescan now").on_hover_text("Compare every location with the disk").clicked() {
            app.indexer.send(Command::ScanAll { force_new: true });
            app.toast("Rescan started".into());
        }
        if ui.button("Resume interrupted scan").clicked() {
            app.indexer.send(Command::ScanAll { force_new: false });
        }
        if ui.button("Pause scan").clicked() {
            app.indexer.cancel_scan();
        }
        if ui.button("Check integrity").on_hover_text("Run an SQLite integrity check").clicked() {
            app.indexer.send(Command::Verify);
        }
    });
    ui.add_space(6.0);
    if !app.settings.confirm_rebuild {
        if ui.button("Rebuild index from scratch…").clicked() {
            app.settings.confirm_rebuild = true;
        }
    } else {
        card(ui, |ui| {
            ui.colored_label(theme::WARN, "Delete the whole index and scan every location again? Search stays available but incomplete until it finishes.");
            ui.horizontal(|ui| {
                if ui.button("Rebuild").clicked() {
                    app.indexer.send(Command::Rebuild);
                    app.settings.confirm_rebuild = false;
                }
                if ui.button("Cancel").clicked() {
                    app.settings.confirm_rebuild = false;
                }
            });
        });
    }
    ui.add_space(12.0);
    let errs = app.status.recent_errors.clone();
    egui::CollapsingHeader::new(RichText::new(format!("Recent errors ({})", errs.len())).font(theme::strong(14.0)))
        .id_salt("recent-errors")
        .show(ui, |ui| {
            if errs.is_empty() {
                ui.label(RichText::new("No errors.").color(theme::TEXT_DIM));
            }
            for e in errs.iter().rev().take(100) {
                ui.add(egui::Label::new(RichText::new(e).small().monospace()).wrap());
            }
        });
}
