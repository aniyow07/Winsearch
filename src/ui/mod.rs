//! The WinSearch window: search box, virtualized results, preview pane,
//! status bar and settings. All slow work happens on background threads;
//! this module only polls channels and draws.

mod icons;
mod preview_panel;
mod results;
mod search_worker;
mod settings;
pub mod theme;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use egui::{Key, Modifiers, RichText};
use winsearch::config::AppConfig;
use winsearch::indexer::{IndexerHandle, Phase, StatusSnapshot};
use winsearch::model::SearchHit;
use winsearch::preview::{Preview, PreviewOptions, PreviewService};
use winsearch::search::{KindFilter, Query, SortKey};
use winsearch::{platform, util};

use search_worker::{SearchRequest, SearchWorker};

const DEBOUNCE: Duration = Duration::from_millis(60);
/// While the index changes, re-run the visible query at most this often.
const LIVE_REFRESH: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeFilter {
    All,
    Folders,
    Documents,
    Images,
    Audio,
    Video,
    Archives,
    Code,
    Programs,
}

impl TypeFilter {
    pub const ALL: [TypeFilter; 9] = [
        TypeFilter::All,
        TypeFilter::Folders,
        TypeFilter::Documents,
        TypeFilter::Images,
        TypeFilter::Audio,
        TypeFilter::Video,
        TypeFilter::Archives,
        TypeFilter::Code,
        TypeFilter::Programs,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TypeFilter::All => "All types",
            TypeFilter::Folders => "Folders",
            TypeFilter::Documents => "Documents",
            TypeFilter::Images => "Images",
            TypeFilter::Audio => "Audio",
            TypeFilter::Video => "Video",
            TypeFilter::Archives => "Archives",
            TypeFilter::Code => "Code",
            TypeFilter::Programs => "Programs",
        }
    }

    fn exts(self) -> &'static [&'static str] {
        match self {
            TypeFilter::Documents => &[
                "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt",
                "md", "csv", "epub",
            ],
            TypeFilter::Images => &[
                "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico", "svg", "heic", "raw",
                "psd",
            ],
            TypeFilter::Audio => &["mp3", "wav", "flac", "ogg", "m4a", "aac", "wma", "opus"],
            TypeFilter::Video => &["mp4", "mkv", "avi", "mov", "wmv", "webm", "m4v", "mpg", "mpeg"],
            TypeFilter::Archives => &["zip", "7z", "rar", "tar", "gz", "xz", "bz2", "zst", "iso", "cab"],
            TypeFilter::Code => &[
                "rs", "c", "h", "cpp", "hpp", "cs", "java", "go", "py", "js", "ts", "tsx", "jsx", "html",
                "css", "json", "xml", "yaml", "yml", "toml", "sql", "sh", "ps1", "bat", "lua", "rb",
                "php", "kt", "swift",
            ],
            TypeFilter::Programs => &["exe", "msi", "bat", "cmd", "ps1", "lnk", "appx", "msix"],
            TypeFilter::All | TypeFilter::Folders => &[],
        }
    }
}

pub struct WinSearchApp {
    cfg: AppConfig,
    db_path: std::path::PathBuf,
    indexer: IndexerHandle,
    search: SearchWorker,
    preview: PreviewService,

    query_text: String,
    last_edit: Option<Instant>,
    type_filter: TypeFilter,
    scope: Option<String>,
    sort: SortKey,
    descending: bool,
    next_query_id: u64,
    pending_query_id: u64,
    received_query_id: u64,
    /// True when the next search should keep the selection on the same path.
    keep_selection: bool,

    results: Vec<SearchHit>,
    total: usize,
    capped: bool,
    query_ms: f64,
    degraded: bool,
    search_error: Option<String>,
    highlight_terms: Vec<String>,
    selected: Option<usize>,
    scroll_to_selected: bool,

    current_preview: Option<Arc<Preview>>,
    preview_texture: Option<(u64, egui::TextureHandle)>,
    preview_requested_for: Option<String>,
    view: preview_panel::ViewState,
    pending_variant: Option<PreviewOptions>,
    debug_zoom: Option<f32>,
    debug_page: Option<u32>,

    status: StatusSnapshot,
    last_index_version: u64,
    last_live_refresh: Instant,
    memory: platform::MemoryUsage,
    last_memory_sample: Instant,

    settings: settings::SettingsState,
    toast: Option<(String, Instant)>,
    focus_search: bool,
    search_id: egui::Id,
}

impl WinSearchApp {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: AppConfig, start: StartOptions) -> WinSearchApp {
        theme::apply(&cc.egui_ctx);
        let db_path = winsearch::config::default_db_path();
        let indexer = IndexerHandle::start(db_path.clone(), cfg.clone());
        let ctx = cc.egui_ctx.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let search = SearchWorker::start(db_path.clone(), notify.clone());
        let preview = PreviewService::new(2, notify);
        let mut app = WinSearchApp {
            settings: settings::SettingsState::new(&cfg),
            cfg,
            db_path,
            indexer,
            search,
            preview,
            query_text: String::new(),
            last_edit: None,
            type_filter: TypeFilter::All,
            scope: None,
            sort: SortKey::Relevance,
            descending: false,
            next_query_id: 0,
            pending_query_id: 0,
            received_query_id: 0,
            keep_selection: false,
            results: Vec::new(),
            total: 0,
            capped: false,
            query_ms: 0.0,
            degraded: false,
            search_error: None,
            highlight_terms: Vec::new(),
            selected: None,
            scroll_to_selected: false,
            current_preview: None,
            preview_texture: None,
            preview_requested_for: None,
            view: preview_panel::ViewState::default(),
            pending_variant: None,
            status: StatusSnapshot::default(),
            last_index_version: 0,
            last_live_refresh: Instant::now(),
            memory: platform::process_memory(),
            last_memory_sample: Instant::now(),
            toast: None,
            focus_search: true,
            search_id: egui::Id::new("search-box"),
            debug_zoom: start.zoom,
            debug_page: start.page,
        };
        if let Some(q) = start.query {
            app.query_text = q;
        }
        app.scope = start.scope;
        app.submit_search(false);
        app
    }

    fn build_query(&self) -> Query {
        let mut q = Query::parse(&self.query_text);
        q.sort = self.sort;
        q.descending = self.descending;
        q.show_hidden = self.cfg.show_hidden;
        q.scope = self.scope.clone();
        q.boost_prefix = std::env::var("USERPROFILE").ok();
        if self.cfg.hide_clutter {
            // Picking a scope inside a clutter folder means the user wants it.
            let scope = self.scope.clone().unwrap_or_default().to_lowercase();
            q.exclude_folders = self
                .cfg
                .clutter_folders
                .iter()
                .filter(|c| {
                    if c.as_str() == ".*" {
                        return !scope.contains("\\.");
                    }
                    let c = c.to_lowercase();
                    let name = format!("\\{}\\", c.trim_matches('\\'));
                    scope.is_empty() || !(util::is_under(&scope, &c) || format!("{scope}\\").contains(&name))
                })
                .cloned()
                .collect();
        }
        match self.type_filter {
            TypeFilter::All => {}
            TypeFilter::Folders => q.kind = KindFilter::FoldersOnly,
            other => {
                if q.exts.is_empty() {
                    q.exts = other.exts().iter().map(|s| s.to_string()).collect();
                }
                if q.kind == KindFilter::Any {
                    q.kind = KindFilter::FilesOnly;
                }
            }
        }
        q
    }

    fn submit_search(&mut self, keep_selection: bool) {
        self.next_query_id += 1;
        self.pending_query_id = self.next_query_id;
        self.keep_selection = keep_selection;
        self.last_edit = None;
        let query = self.build_query();
        self.highlight_terms = query.name_terms.clone();
        if query.is_empty() {
            self.results.clear();
            self.total = 0;
            self.capped = false;
            self.search_error = None;
            self.set_selection(None);
            return;
        }
        self.search.submit(SearchRequest { id: self.pending_query_id, query, limit: self.cfg.result_limit });
    }

    fn query_changed(&mut self) {
        self.last_edit = Some(Instant::now());
    }

    fn poll_search(&mut self) {
        while let Ok(resp) = self.search.rx.try_recv() {
            if resp.id != self.pending_query_id {
                continue; // an older query finished late
            }
            self.received_query_id = resp.id;
            match resp.result {
                Ok(r) => {
                    let prev_path = self.selected_hit().map(|h| h.path.clone());
                    self.results = r.hits;
                    self.total = r.total;
                    self.capped = r.capped;
                    self.query_ms = r.elapsed.as_secs_f64() * 1000.0;
                    self.degraded = r.degraded;
                    self.search_error = None;
                    let sel = if self.keep_selection {
                        prev_path
                            .and_then(|p| self.results.iter().position(|h| h.path == p))
                            .or(if self.results.is_empty() { None } else { Some(0) })
                    } else if self.results.is_empty() {
                        None
                    } else {
                        Some(0)
                    };
                    if !self.keep_selection {
                        self.scroll_to_selected = true;
                    }
                    self.set_selection(sel);
                }
                Err(e) => {
                    self.search_error = Some(e);
                    self.results.clear();
                    self.set_selection(None);
                }
            }
        }
    }

    fn selected_hit(&self) -> Option<&SearchHit> {
        self.selected.and_then(|i| self.results.get(i))
    }

    /// Changes the selection and requests a preview for it (never opens it).
    fn set_selection(&mut self, idx: Option<usize>) {
        self.selected = idx.filter(|&i| i < self.results.len());
        let Some(hit) = self.selected_hit().cloned() else {
            self.preview.cancel();
            self.current_preview = None;
            self.preview_texture = None;
            self.preview_requested_for = None;
            return;
        };
        if self.preview_requested_for.as_deref() == Some(hit.path.as_str()) {
            return;
        }
        self.preview_requested_for = Some(hit.path.clone());
        self.view.reset_for(&hit.path);
        if let Some(z) = self.debug_zoom {
            self.view.zoom = z;
        }
        self.pending_variant = None;
        match self.preview.request(&hit.path, hit.is_dir, hit.size, hit.modified, PreviewOptions::default()) {
            Some(cached) => self.show_preview(cached),
            None => {
                // Keep showing metadata of the new selection while loading.
                self.current_preview = None;
                self.preview_texture = None;
            }
        }
    }

    fn show_preview(&mut self, p: Arc<Preview>) {
        self.preview_texture = None;
        self.current_preview = Some(p);
    }

    /// Asks for another rendering of the selected file (sharper zoom, other
    /// PDF page) while the current one stays on screen.
    pub fn request_preview_variant(&mut self, path: &str, opts: PreviewOptions) {
        let Some(hit) = self.selected_hit().filter(|h| h.path == path).cloned() else { return };
        self.pending_variant = Some(opts);
        if let Some(cached) = self.preview.request(&hit.path, hit.is_dir, hit.size, hit.modified, opts) {
            self.pending_variant = None;
            self.show_preview(cached);
        }
    }

    fn poll_preview(&mut self) {
        if let Some(p) = self.preview.poll()
            && self.preview_requested_for.as_deref() == Some(p.path.as_str())
        {
            use winsearch::preview::Content;
            let was_variant = self.pending_variant.take().is_some();
            // A failed re-render (sharper zoom, other page) must not replace
            // the page that is already on screen.
            if was_variant
                && let Some(cur) = &self.current_preview
                && cur.path == p.path
            {
                let same_kind = matches!(
                    (&cur.content, &p.content),
                    (Ok(Content::Pdf { .. }), Ok(Content::Pdf { .. })) | (Ok(Content::Image { .. }), Ok(Content::Image { .. }))
                );
                if !same_kind {
                    self.view.detail_failed = true;
                    self.toast("Could not render this page at a higher quality".into());
                    return;
                }
            }
            let first_pdf = matches!(&p.content, Ok(Content::Pdf { index: 0, .. }));
            let path = p.path.clone();
            self.show_preview(p);
            if first_pdf && let Some(page) = self.debug_page.take() {
                self.view.page = page;
                self.request_preview_variant(&path, PreviewOptions { max_px: winsearch::preview::PREVIEW_PX, page });
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let cur = self.selected.map(|i| i as isize).unwrap_or(-1);
        let next = (cur + delta).clamp(0, self.results.len() as isize - 1) as usize;
        self.scroll_to_selected = true;
        self.set_selection(Some(next));
    }

    pub fn open_selected(&mut self) {
        if let Some(h) = self.selected_hit() {
            let path = h.path.clone();
            self.toast(format!("Opening {}", h.name));
            std::thread::spawn(move || {
                if let Err(e) = platform::open_with_default_app(&path) {
                    tracing::warn!("open {path}: {e}");
                }
            });
        }
    }

    pub fn reveal_selected(&mut self) {
        if let Some(h) = self.selected_hit() {
            let path = h.path.clone();
            std::thread::spawn(move || {
                if let Err(e) = platform::reveal_in_explorer(&path) {
                    tracing::warn!("reveal {path}: {e}");
                }
            });
        }
    }

    pub fn copy_selected_path(&mut self, ctx: &egui::Context) {
        if let Some(h) = self.selected_hit() {
            let p = h.path.clone();
            ctx.copy_text(p.clone());
            self.toast(format!("Copied {p}"));
        }
    }

    fn toast(&mut self, msg: String) {
        self.toast = Some((msg, Instant::now()));
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        // A menu or dropdown is open: let it handle Esc/arrows itself.
        if egui::Popup::is_any_open(ctx) {
            return;
        }
        if self.settings.open {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.settings.open = false;
                self.focus_search = true;
            }
            return;
        }
        let page = 15isize;
        let (down, up, pgdn, pgup, home, end, enter, reveal, reveal2, esc, settings, refresh) =
            ctx.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::PageDown),
                    i.consume_key(Modifiers::NONE, Key::PageUp),
                    i.consume_key(Modifiers::CTRL, Key::Home),
                    i.consume_key(Modifiers::CTRL, Key::End),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::CTRL, Key::Enter),
                    i.consume_key(Modifiers::CTRL, Key::E),
                    i.consume_key(Modifiers::NONE, Key::Escape),
                    i.consume_key(Modifiers::CTRL, Key::Comma),
                    i.consume_key(Modifiers::NONE, Key::F5),
                )
            });
        if down {
            self.move_selection(1);
        }
        if up {
            self.move_selection(-1);
        }
        if pgdn {
            self.move_selection(page);
        }
        if pgup {
            self.move_selection(-page);
        }
        if home {
            self.move_selection(-(self.results.len() as isize));
        }
        if end {
            self.move_selection(self.results.len() as isize);
        }
        if enter {
            if self.last_edit.is_some() {
                self.submit_search(false);
            } else {
                self.open_selected();
            }
        }
        if reveal || reveal2 {
            self.reveal_selected();
        }
        if esc {
            if self.query_text.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.query_text.clear();
                self.submit_search(false);
            }
            self.focus_search = true;
        }
        if settings {
            self.settings.open_with(&self.cfg);
        }
        if refresh {
            self.submit_search(true);
        }

        // Preview zoom and PDF pages, usable while typing.
        let (zin, zin2, zout, zreset, next_page, prev_page) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::CTRL, Key::Plus),
                i.consume_key(Modifiers::CTRL, Key::Equals),
                i.consume_key(Modifiers::CTRL, Key::Minus),
                i.consume_key(Modifiers::CTRL, Key::Num0),
                i.consume_key(Modifiers::CTRL, Key::PageDown),
                i.consume_key(Modifiers::CTRL, Key::PageUp),
            )
        });
        if zin || zin2 {
            self.view.zoom = (self.view.zoom * 1.25).min(16.0);
            self.view.changed_at = Instant::now();
        }
        if zout {
            self.view.zoom = (self.view.zoom / 1.25).max(0.25);
            self.view.changed_at = Instant::now();
        }
        if zreset {
            self.view.zoom = 1.0;
            self.view.pan = egui::Vec2::ZERO;
        }
        if (next_page || prev_page)
            && let Some(p) = self.current_preview.clone()
            && let Ok(winsearch::preview::Content::Pdf { index, page_count, .. }) = &p.content
        {
            let target = if next_page { (*index + 1).min(page_count.saturating_sub(1)) } else { index.saturating_sub(1) };
            if target != *index {
                self.view.page = target;
                self.view.zoom = 1.0;
                self.view.pan = egui::Vec2::ZERO;
                self.request_preview_variant(&p.path, PreviewOptions { max_px: winsearch::preview::PREVIEW_PX, page: target });
            }
        }

        // Ctrl+C: copies selected text in the search box if there is any,
        // otherwise the selected file's path.
        let copy = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if copy {
            let has_text_selection = egui::TextEdit::load_state(ctx, self.search_id)
                .and_then(|s| s.cursor.char_range())
                .map(|r| !r.is_empty())
                .unwrap_or(false);
            if !has_text_selection {
                self.copy_selected_path(ctx);
            }
        }
    }

    fn poll_status(&mut self) {
        self.status = self.indexer.status.snapshot();
        let v = self.indexer.status.version.load(Ordering::Relaxed);
        if v != self.last_index_version && self.last_live_refresh.elapsed() >= LIVE_REFRESH {
            self.last_index_version = v;
            self.last_live_refresh = Instant::now();
            if !self.query_text.trim().is_empty() || self.type_filter != TypeFilter::All {
                self.submit_search(true);
            }
        }
        if self.last_memory_sample.elapsed() > Duration::from_secs(2) {
            self.memory = platform::process_memory();
            self.last_memory_sample = Instant::now();
        }
    }

    /// Folder scope button: Everywhere, the user's folders, indexed
    /// locations, recent picks, or any folder chosen in a dialog.
    fn scope_picker(&mut self, ui: &mut egui::Ui) {
        let label = match &self.scope {
            None => "Everywhere".to_string(),
            Some(p) => short_folder_label(p),
        };
        let active = self.scope.is_some();
        let text = RichText::new(format!("{label}  ▾"))
            .size(13.0)
            .color(if active { theme::TEXT } else { theme::TEXT_DIM });
        let mut chosen: Option<Option<String>> = None;
        let button = egui::Button::new(text)
            .fill(if active { theme::ACCENT_DIM } else { theme::PANEL })
            .stroke(egui::Stroke::new(1.0, if active { theme::ACCENT } else { theme::BORDER }))
            .corner_radius(13.0)
            .min_size(egui::vec2(0.0, 26.0));
        let resp = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            ui.set_min_width(260.0);
            if ui.add(egui::Button::selectable(self.scope.is_none(), "Everywhere")).clicked() {
                chosen = Some(None);
            }
            ui.separator();
            ui.label(RichText::new("Your folders").small().color(theme::TEXT_FAINT));
            for (name, path) in user_folders() {
                let sel = self.scope.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&path));
                if ui.add(egui::Button::selectable(sel, name)).on_hover_text(&path).clicked() {
                    chosen = Some(Some(path));
                }
            }
            if !self.cfg.roots.is_empty() {
                ui.separator();
                ui.label(RichText::new("Indexed locations").small().color(theme::TEXT_FAINT));
                for r in self.cfg.roots.clone() {
                    let sel = self.scope.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&r));
                    if ui.add(egui::Button::selectable(sel, r.as_str())).clicked() {
                        chosen = Some(Some(r));
                    }
                }
            }
            if !self.cfg.recent_scopes.is_empty() {
                ui.separator();
                ui.label(RichText::new("Recent").small().color(theme::TEXT_FAINT));
                for r in self.cfg.recent_scopes.clone() {
                    let sel = self.scope.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&r));
                    if ui.add(egui::Button::selectable(sel, short_folder_label(&r))).on_hover_text(&r).clicked() {
                        chosen = Some(Some(r));
                    }
                }
            }
            ui.separator();
            if ui.button("Choose a folder…").clicked()
                && let Some(dir) = rfd::FileDialog::new().set_title("Search only inside this folder").pick_folder()
            {
                chosen = Some(Some(util::normalize_dir(&dir.to_string_lossy())));
            }
        });
        resp.0.on_hover_text(match &self.scope {
            None => "Searching every indexed location. Click to search inside one folder.".to_string(),
            Some(p) => format!("Searching only inside {p}"),
        });
        if active {
            let (r, x) = ui.allocate_exact_size(egui::vec2(20.0, 26.0), egui::Sense::click());
            let c = if x.hovered() { theme::TEXT } else { theme::TEXT_DIM };
            let d = 4.0;
            let s = egui::Stroke::new(1.5, c);
            ui.painter().line_segment([r.center() + egui::vec2(-d, -d), r.center() + egui::vec2(d, d)], s);
            ui.painter().line_segment([r.center() + egui::vec2(-d, d), r.center() + egui::vec2(d, -d)], s);
            if x.on_hover_text("Search everywhere again").clicked() {
                chosen = Some(None);
            }
        }
        if let Some(scope) = chosen {
            self.set_scope(scope);
        }
    }

    /// Limits searching to one folder (None = everywhere).
    pub fn set_scope(&mut self, scope: Option<String>) {
        if let Some(p) = &scope {
            let is_root_or_user = self.cfg.roots.iter().any(|r| r.eq_ignore_ascii_case(p))
                || user_folders().iter().any(|(_, u)| u.eq_ignore_ascii_case(p));
            if !is_root_or_user {
                self.cfg.recent_scopes.retain(|r| !r.eq_ignore_ascii_case(p));
                self.cfg.recent_scopes.insert(0, p.clone());
                self.cfg.recent_scopes.truncate(8);
                let _ = self.cfg.save();
                self.settings.sync_option(&self.cfg);
            }
        }
        self.scope = scope;
        self.submit_search(false);
        self.focus_search = true;
    }

    /// Adds a folder to the hidden-from-results list (kept in the index).
    pub fn hide_folder(&mut self, folder: &str) {
        if !self.cfg.clutter_folders.iter().any(|c| c.eq_ignore_ascii_case(folder)) {
            self.cfg.clutter_folders.push(folder.to_string());
        }
        self.cfg.hide_clutter = true;
        let _ = self.cfg.save();
        self.settings.sync_option(&self.cfg);
        self.toast(format!("Hiding {folder}. Undo in Settings > Search results."));
        self.submit_search(true);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let width = ui.available_width() - 52.0;
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.query_text)
                    .id(self.search_id)
                    .hint_text(RichText::new("Search files and folders").color(theme::TEXT_FAINT))
                    .font(egui::FontId::proportional(17.0))
                    .desired_width(width)
                    .margin(egui::Margin { left: 42, right: 40, top: 10, bottom: 10 })
                    .background_color(theme::SURFACE),
            );
            let icon_color = if resp.has_focus() { theme::ACCENT } else { theme::TEXT_DIM };
            icons::search(ui.painter(), egui::pos2(resp.rect.left() + 22.0, resp.rect.center().y), 18.0, icon_color);
            if self.focus_search {
                resp.request_focus();
                self.focus_search = false;
            }
            if resp.changed() {
                self.query_changed();
            }
            if !self.query_text.is_empty() {
                let r = egui::Rect::from_center_size(
                    egui::pos2(resp.rect.right() - 20.0, resp.rect.center().y),
                    egui::vec2(24.0, 24.0),
                );
                let clear = ui.interact(r, egui::Id::new("clear-query"), egui::Sense::click());
                let c = if clear.hovered() { theme::TEXT } else { theme::TEXT_DIM };
                if clear.hovered() {
                    ui.painter().circle_filled(r.center(), 11.0, theme::SURFACE_HI);
                }
                let d = 4.5;
                let s = egui::Stroke::new(1.6, c);
                ui.painter().line_segment([r.center() + egui::vec2(-d, -d), r.center() + egui::vec2(d, d)], s);
                ui.painter().line_segment([r.center() + egui::vec2(-d, d), r.center() + egui::vec2(d, -d)], s);
                if clear.on_hover_text("Clear (Esc)").clicked() {
                    self.query_text.clear();
                    self.submit_search(false);
                    self.focus_search = true;
                }
            }
            let (r, gear) = ui.allocate_exact_size(egui::vec2(resp.rect.height(), resp.rect.height()), egui::Sense::click());
            let bg = if gear.hovered() { theme::SURFACE_HI } else { theme::SURFACE };
            ui.painter().rect(r, 8.0, bg, egui::Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
            icons::gear(ui.painter(), r.center(), 18.0, if gear.hovered() { theme::TEXT } else { theme::TEXT_DIM });
            if gear.on_hover_text("Settings: indexed drives, exclusions, maintenance (Ctrl+,)").clicked() {
                self.settings.open_with(&self.cfg);
            }
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            self.scope_picker(ui);
            ui.add_space(6.0);
            for f in TypeFilter::ALL {
                let selected = self.type_filter == f;
                let text = RichText::new(f.label()).size(13.0).color(if selected { theme::TEXT } else { theme::TEXT_DIM });
                let b = egui::Button::new(text)
                    .fill(if selected { theme::ACCENT_DIM } else { theme::PANEL })
                    .stroke(egui::Stroke::new(1.0, if selected { theme::ACCENT } else { theme::BORDER }))
                    .corner_radius(13.0)
                    .min_size(egui::vec2(0.0, 26.0));
                if ui.add(b).clicked() && !selected {
                    self.type_filter = f;
                    self.submit_search(false);
                    self.focus_search = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let on = self.cfg.hide_clutter;
                let label = RichText::new(if on { "✓ Hiding system & app data" } else { "Showing system & app data" })
                    .size(13.0)
                    .color(if on { theme::TEXT } else { theme::TEXT_DIM });
                let toggle = ui
                    .add(
                        egui::Button::new(label)
                            .fill(theme::PANEL)
                            .stroke(egui::Stroke::new(1.0, theme::BORDER))
                            .corner_radius(13.0)
                            .min_size(egui::vec2(0.0, 26.0)),
                    )
                    .on_hover_text(format!(
                        "Hides results inside these folders (they stay indexed):\n{}\n\nEdit the list in Settings > Search.",
                        self.cfg.clutter_folders.iter().map(|c| winsearch::config::clutter_label(c)).collect::<Vec<_>>().join("\n")
                    ));
                if toggle.clicked() {
                    self.cfg.hide_clutter = !on;
                    let _ = self.cfg.save();
                    self.settings.sync_option(&self.cfg);
                    self.submit_search(true);
                    self.focus_search = true;
                }
                ui.add_space(6.0);
                ui.label(RichText::new("?").size(13.0).color(theme::TEXT_FAINT))
                    .on_hover_text(
                        "Search syntax\n\
                         words        names containing every word\n\
                         *.pdf / ext:pdf,docx   extension filter\n\
                         folder: / file:        only folders / only files\n\
                         path:proj or a\\b       match the full path\n\
                         \"two words\"            keep words together",
                    );
            });
        });
        ui.add_space(10.0);
        for notice in self.status.notices.clone() {
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(0x3a, 0x2f, 0x14))
                .stroke(egui::Stroke::new(1.0, theme::WARN.gamma_multiply(0.5)))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(notice).color(theme::WARN));
                });
            ui.add_space(8.0);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let s = &self.status;
            let p = &self.indexer.status.progress;
            match &s.phase {
                Phase::Scanning => {
                    ui.add(egui::Spinner::new().size(14.0));
                    let files = p.files.load(Ordering::Relaxed);
                    let dirs = p.dirs.load(Ordering::Relaxed);
                    let errs = p.errors.load(Ordering::Relaxed);
                    let root = s.current_root.clone().unwrap_or_default();
                    ui.label(format!(
                        "Indexing {root}  {} items in {} folders",
                        util::format_count(files),
                        util::format_count(dirs)
                    ));
                    if errs > 0 {
                        ui.colored_label(theme::WARN, format!("{errs} unreadable"))
                            .on_hover_text("Folders that could not be read (usually access denied). See Settings > Recent errors.");
                    }
                    if ui.small_button("Cancel").on_hover_text("Pause indexing; it resumes on the next scan").clicked() {
                        self.indexer.cancel_scan();
                    }
                }
                Phase::Rebuilding => {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(if s.message.is_empty() { "Updating the search index…".to_string() } else { s.message.clone() });
                }
                Phase::Failed(m) => {
                    ui.colored_label(theme::ERR, format!("✖ {m}"));
                }
                Phase::Starting => {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label("Opening index…");
                }
                Phase::Idle | Phase::Applying => {
                    if s.up_to_date() {
                        status_dot(ui, theme::OK);
                        ui.label(format!("Index up to date · {} items", util::format_count(s.total_entries)))
                            .on_hover_text(root_summary(s));
                    } else {
                        status_dot(ui, theme::WARN);
                        let why = if s.roots.is_empty() {
                            "No locations are indexed. Add drives or folders in Settings.".to_string()
                        } else if s.roots.iter().any(|r| r.state != "complete") {
                            format!("Index incomplete · {} items (a scan was interrupted or a drive is missing)", util::format_count(s.total_entries))
                        } else {
                            format!("Live updates unavailable for some locations · {} items", util::format_count(s.total_entries))
                        };
                        ui.label(why).on_hover_text(root_summary(s));
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{:.0} MB", self.memory.private_bytes as f64 / 1_048_576.0))
                        .color(theme::TEXT_DIM),
                )
                .on_hover_text(format!(
                    "Private memory {:.1} MB, working set {:.1} MB, preview cache {:.1} MB",
                    self.memory.private_bytes as f64 / 1_048_576.0,
                    self.memory.working_set as f64 / 1_048_576.0,
                    self.preview.cache_bytes() as f64 / 1_048_576.0
                ));
                ui.separator();
                if !self.query_text.trim().is_empty() || self.type_filter != TypeFilter::All {
                    let count = if self.capped {
                        format!("{}+", util::format_count(self.total as u64))
                    } else {
                        util::format_count(self.total as u64)
                    };
                    let shown = if self.total > self.results.len() {
                        format!(" (first {} shown)", util::format_count(self.results.len() as u64))
                    } else {
                        String::new()
                    };
                    let noun = if self.total == 1 { "result" } else { "results" };
                    ui.label(
                        RichText::new(format!("{count} {noun}{shown} · {:.1} ms", self.query_ms)).color(theme::TEXT_DIM),
                    );
                    if self.pending_query_id != self.received_query_id {
                        ui.add(egui::Spinner::new().size(12.0));
                    }
                    if self.degraded {
                        ui.colored_label(theme::WARN, "slow search while the name index rebuilds");
                    }
                }
                if let Some((msg, at)) = &self.toast {
                    if at.elapsed() < Duration::from_secs(3) {
                        ui.separator();
                        ui.label(RichText::new(msg).color(theme::ACCENT));
                    }
                }
            });
        });
    }
}

fn root_summary(s: &StatusSnapshot) -> String {
    let mut out = String::new();
    for r in &s.roots {
        let live = match &r.live {
            winsearch::indexer::LiveMode::Usn => "live: NTFS change journal".to_string(),
            winsearch::indexer::LiveMode::Notifications => "live: change notifications".to_string(),
            winsearch::indexer::LiveMode::Unavailable(e) => format!("live updates off: {e}"),
        };
        out.push_str(&format!(
            "{}  [{}] last full scan {}  ({live})\n",
            r.path,
            r.state,
            util::format_time(r.last_complete_scan)
        ));
    }
    if !s.message.is_empty() {
        out.push_str(&s.message);
    }
    out
}

impl eframe::App for WinSearchApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_status();
        self.poll_search();
        self.poll_preview();
        if let Some(t) = self.last_edit
            && t.elapsed() >= DEBOUNCE
        {
            self.submit_search(false);
        }
        self.handle_keys(ctx);
        // Keep polling while background work is visible.
        let busy = !matches!(self.status.phase, Phase::Idle)
            || self.last_edit.is_some()
            || (self.preview_requested_for.is_some() && self.current_preview.is_none());
        if busy {
            ctx.request_repaint_after(Duration::from_millis(if self.last_edit.is_some() { 16 } else { 250 }));
        } else {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(8, 0)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(10, 5)))
            .show(ui, |ui| self.status_bar(ui));
        egui::Panel::right("preview")
            .resizable(true)
            .default_size(400.0)
            .size_range(260.0..=900.0)
            .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::same(12)))
            .show(ui, |ui| preview_panel::show(self, ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(0, 0)))
            .show(ui, |ui| results::show(self, ui));
        settings::show(self, &ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        tracing::info!("window closed; stopping background work");
        self.indexer.cancel_scan();
    }
}

fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    icons::dot(ui.painter(), r.center(), color);
}

/// The user's standard folders that exist on this machine.
fn user_folders() -> Vec<(&'static str, String)> {
    let Ok(home) = std::env::var("USERPROFILE") else { return Vec::new() };
    let mut out = vec![("Home", home.clone())];
    for name in ["Desktop", "Documents", "Downloads", "Pictures", "Music", "Videos"] {
        let p = format!("{home}\\{name}");
        if std::path::Path::new(&p).is_dir() {
            out.push((name, p));
        }
    }
    if let Ok(od) = std::env::var("OneDrive")
        && std::path::Path::new(&od).is_dir()
    {
        out.push(("OneDrive", od));
    }
    out
}

/// "Documents" for C:\Users\me\Documents, "C:\" for a drive root.
fn short_folder_label(p: &str) -> String {
    match util::split_path(p) {
        Some((_, name)) => name.to_string(),
        None => p.to_string(),
    }
}

/// Options from the command line (`--query`, `--scope`) plus two testing
/// aids read from the environment: `WINSEARCH_DEBUG_ZOOM` sets the preview
/// zoom and `WINSEARCH_DEBUG_PAGE` the PDF page shown for the selection.
#[derive(Default, Clone, Debug)]
pub struct StartOptions {
    pub query: Option<String>,
    pub scope: Option<String>,
    pub zoom: Option<f32>,
    pub page: Option<u32>,
}

impl StartOptions {
    pub fn from_args(args: &[String]) -> StartOptions {
        let mut o = StartOptions::default();
        let mut it = args.iter().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--query" | "-q" => o.query = it.next().cloned(),
                "--scope" => o.scope = it.next().map(|s| util::normalize_dir(s)),
                _ => {}
            }
        }
        o.zoom = std::env::var("WINSEARCH_DEBUG_ZOOM").ok().and_then(|v| v.parse().ok());
        o.page = std::env::var("WINSEARCH_DEBUG_PAGE").ok().and_then(|v| v.parse().ok());
        o
    }
}
