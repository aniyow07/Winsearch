//! Virtualized result table: only visible rows are laid out each frame,
//! so tens of thousands of results cost the same as a screenful.

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Stroke, Ui, vec2};
use winsearch::search::SortKey;
use winsearch::util;

use super::{WinSearchApp, icons, theme};

pub const ROW_H: f32 = 30.0;
const PAD_X: f32 = 14.0;

struct Columns {
    name: f32,
    path: f32,
    size: f32,
    modified: f32,
}

fn columns(width: f32) -> Columns {
    let size = 86.0;
    let modified = 142.0;
    let rest = (width - size - modified - PAD_X * 2.0).max(240.0);
    Columns { name: rest * 0.40, path: rest * 0.60, size, modified }
}

pub fn show(app: &mut WinSearchApp, ui: &mut Ui) {
    if app.query_text.trim().is_empty() && app.type_filter == super::TypeFilter::All {
        empty_state(app, ui);
        return;
    }
    if let Some(err) = &app.search_error {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| ui.colored_label(theme::ERR, err));
        return;
    }

    let cols = columns(ui.available_width());
    header(app, ui, &cols);

    if app.results.is_empty() {
        ui.add_space(48.0);
        ui.vertical_centered(|ui| {
            if app.last_edit.is_some() {
                ui.label(RichText::new("Searching…").color(theme::TEXT_DIM));
            } else {
                ui.label(RichText::new("No matches").size(17.0).color(theme::TEXT));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Check the spelling, try part of the name, or widen the type filter.")
                        .color(theme::TEXT_DIM),
                );
            }
        });
        return;
    }

    let n = app.results.len();
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("results-scroll");
    if app.scroll_to_selected {
        if let Some(sel) = app.selected {
            let top = sel as f32 * ROW_H;
            let id = ui.make_persistent_id("results-scroll");
            let cur = egui::scroll_area::State::load(ui.ctx(), id).map(|s| s.offset.y).unwrap_or(0.0);
            let view_h = ui.available_height();
            if top < cur {
                scroll = scroll.vertical_scroll_offset(top);
            } else if top + ROW_H > cur + view_h {
                scroll = scroll.vertical_scroll_offset(top + ROW_H - view_h);
            }
        }
        app.scroll_to_selected = false;
    }

    let mut clicked: Option<usize> = None;
    let mut double: Option<usize> = None;
    let mut menu_action: Option<(usize, MenuAction)> = None;
    let terms = app.highlight_terms.clone();
    scroll.show_rows(ui, ROW_H, n, |ui, range| {
        for i in range {
            let hit = &app.results[i];
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
            let selected = app.selected == Some(i);
            let painter = ui.painter_at(rect);
            let bg = if selected {
                theme::ACCENT_DIM
            } else if resp.hovered() {
                theme::ROW_HOVER
            } else if i % 2 == 1 {
                theme::ROW_ALT
            } else {
                theme::BG
            };
            painter.rect_filled(rect, 0.0, bg);
            if selected {
                painter.rect_filled(Rect::from_min_size(rect.min, vec2(3.0, ROW_H)), 0.0, theme::ACCENT);
            }

            let y = rect.center().y;
            let mut x = rect.min.x + PAD_X;
            let cat = icons::category(&hit.ext, hit.is_dir);
            icons::file_icon(&painter, Rect::from_center_size(egui::pos2(x + 8.0, y), vec2(16.0, 16.0)), cat);
            let name_font = FontId::proportional(14.0);
            let name_color = if selected { Color32::WHITE } else { theme::TEXT };
            let job = highlighted(&hit.name, &terms, name_font, name_color, cols.name - 34.0);
            paint_job(ui, &painter, x + 26.0, y, job);
            x += cols.name;

            let dim = if selected { theme::TEXT } else { theme::TEXT_DIM };
            let folder = fit_path(ui, &hit.parent, &FontId::proportional(13.0), cols.path - 16.0);
            let job = plain(&folder, FontId::proportional(13.0), dim, cols.path - 16.0);
            paint_job(ui, &painter, x, y, job);
            x += cols.path;

            let meta_font = FontId::proportional(12.5);
            if !hit.is_dir {
                painter.text(egui::pos2(x + cols.size - 14.0, y), Align2::RIGHT_CENTER, util::format_size(hit.size), meta_font.clone(), dim);
            }
            x += cols.size;
            painter.text(egui::pos2(x, y), Align2::LEFT_CENTER, util::format_time(hit.modified), meta_font, dim);

            if resp.clicked() {
                clicked = Some(i);
            }
            if resp.double_clicked() {
                double = Some(i);
            }
            resp.context_menu(|ui| {
                ui.set_min_width(190.0);
                for (label, hint, action) in [
                    ("Open", "Enter", MenuAction::Open),
                    ("Show in folder", "Ctrl+Enter", MenuAction::Reveal),
                    ("Copy path", "Ctrl+C", MenuAction::CopyPath),
                    ("Copy name", "", MenuAction::CopyName),
                    ("Search in this folder", "", MenuAction::ScopeHere),
                    ("Hide this folder from results", "", MenuAction::HideFolder),
                ] {
                    if ui.add(egui::Button::new(label).shortcut_text(hint)).clicked() {
                        menu_action = Some((i, action));
                        ui.close();
                    }
                }
            });
        }
    });
    if let Some(i) = clicked {
        app.set_selection(Some(i));
        app.focus_search = true;
    }
    if let Some(i) = double {
        app.set_selection(Some(i));
        app.open_selected();
    }
    if let Some((i, action)) = menu_action {
        app.set_selection(Some(i));
        let ctx = ui.ctx().clone();
        match action {
            MenuAction::Open => app.open_selected(),
            MenuAction::Reveal => app.reveal_selected(),
            MenuAction::CopyPath => app.copy_selected_path(&ctx),
            MenuAction::HideFolder => {
                if let Some(h) = app.selected_hit().cloned() {
                    app.hide_folder(&h.parent);
                }
            }
            MenuAction::ScopeHere => {
                if let Some(h) = app.selected_hit().cloned() {
                    app.set_scope(Some(h.parent.clone()));
                }
            }
            MenuAction::CopyName => {
                if let Some(h) = app.selected_hit() {
                    ctx.copy_text(h.name.clone());
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum MenuAction {
    Open,
    Reveal,
    CopyPath,
    CopyName,
    HideFolder,
    ScopeHere,
}

/// Shortens a folder path in the middle so the drive and the last, most
/// specific folders stay visible: `C:\Users\…\project\src`.
fn fit_path(ui: &Ui, path: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x);
    if width(path) <= max_w {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('\\').collect();
    if parts.len() <= 3 {
        return path.to_string();
    }
    let head = parts[..2].join("\\");
    let mut best = format!("{head}\\…\\{}", parts[parts.len() - 1]);
    for keep in 2..parts.len() - 2 {
        let tail = parts[parts.len() - keep..].join("\\");
        let candidate = format!("{head}\\…\\{tail}");
        if width(&candidate) > max_w {
            break;
        }
        best = candidate;
    }
    best
}

fn plain(text: &str, font: FontId, color: Color32, max_w: f32) -> LayoutJob {
    let mut job = LayoutJob::single_section(text.to_string(), TextFormat { font_id: font, color, ..Default::default() });
    job.wrap = TextWrapping::truncate_at_width(max_w.max(10.0));
    job
}

/// Name with the query terms highlighted (case-insensitive).
fn highlighted(text: &str, terms: &[String], font: FontId, color: Color32, max_w: f32) -> LayoutJob {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let lower = text.to_lowercase();
    // Byte offsets only line up when lower-casing kept every length.
    if lower.len() == text.len() {
        for t in terms.iter().filter(|t| !t.is_empty()) {
            let mut start = 0;
            while let Some(pos) = lower[start..].find(t.as_str()) {
                let s = start + pos;
                ranges.push((s, s + t.len()));
                start = s + t.len().max(1);
            }
        }
    }
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.0 <= last.1 => last.1 = last.1.max(r.1),
            _ => merged.push(r),
        }
    }
    let normal = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let hl = TextFormat {
        font_id: FontId::new(font.size, theme::strong_family()),
        color: theme::MATCH,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    let mut pos = 0;
    for (s, e) in merged {
        if !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        if s > pos {
            job.append(&text[pos..s], 0.0, normal.clone());
        }
        job.append(&text[s..e], 0.0, hl.clone());
        pos = e;
    }
    if pos < text.len() {
        job.append(&text[pos..], 0.0, normal);
    }
    job.wrap = TextWrapping::truncate_at_width(max_w.max(10.0));
    job
}

fn paint_job(ui: &Ui, painter: &egui::Painter, x: f32, y: f32, job: LayoutJob) {
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let pos = egui::pos2(x, y - galley.size().y / 2.0);
    painter.galley(pos, galley, theme::TEXT);
}

fn header(app: &mut WinSearchApp, ui: &mut Ui, cols: &Columns) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 0.0, theme::PANEL);
    p.hline(rect.x_range(), rect.max.y - 0.5, Stroke::new(1.0, theme::BORDER));
    let mut x = rect.min.x + PAD_X;
    let entries = [
        ("Name", cols.name, SortKey::Name, false),
        ("Folder", cols.path, SortKey::Path, false),
        ("Size", cols.size, SortKey::Size, true),
        ("Modified", cols.modified, SortKey::Modified, false),
    ];
    for (idx, (label, w, key, right)) in entries.into_iter().enumerate() {
        let r = Rect::from_min_size(egui::pos2(x - 8.0, rect.min.y), vec2(w, rect.height()));
        let resp = ui.interact(r, ui.id().with(("hdr", label)), Sense::click());
        let active = app.sort == key;
        if resp.hovered() {
            ui.painter().rect_filled(r, 0.0, theme::SURFACE);
        }
        let color = if active || resp.hovered() { theme::TEXT } else { theme::TEXT_DIM };
        let arrow = if active { if app.descending { "  ↓" } else { "  ↑" } } else { "" };
        let text = format!("{label}{arrow}");
        let font = FontId::new(12.5, theme::strong_family());
        if right {
            ui.painter().text(egui::pos2(x + w - 14.0, rect.center().y), Align2::RIGHT_CENTER, text, font, color);
        } else {
            ui.painter().text(egui::pos2(x, rect.center().y), Align2::LEFT_CENTER, text, font, color);
        }
        if idx > 0 {
            ui.painter().vline(x - 8.0, rect.y_range().shrink(8.0), Stroke::new(1.0, theme::BORDER));
        }
        if resp.clicked() {
            cycle_sort(app, key);
        }
        resp.on_hover_text("Click to sort. Click again to reverse, a third time to return to best match.");
        x += w;
    }
    if app.sort != SortKey::Relevance {
        let r = Rect::from_min_size(egui::pos2(rect.max.x - 120.0, rect.min.y), vec2(110.0, rect.height()));
        let resp = ui.interact(r, ui.id().with("hdr-best"), Sense::click());
        let color = if resp.hovered() { theme::ACCENT } else { theme::TEXT_DIM };
        ui.painter().text(r.right_center(), Align2::RIGHT_CENTER, "Best match", FontId::proportional(12.0), color);
        if resp.clicked() {
            app.sort = SortKey::Relevance;
            app.descending = false;
            app.submit_search(true);
        }
    }
}

fn cycle_sort(app: &mut WinSearchApp, key: SortKey) {
    let natural_desc = matches!(key, SortKey::Size | SortKey::Modified);
    if app.sort != key {
        app.sort = key;
        app.descending = natural_desc;
    } else if app.descending == natural_desc {
        app.descending = !app.descending;
    } else {
        app.sort = SortKey::Relevance;
        app.descending = false;
    }
    app.submit_search(true);
}

fn empty_state(app: &WinSearchApp, ui: &mut Ui) {
    let avail = ui.available_rect_before_wrap();
    let card_w = 520.0f32.min(avail.width() - 40.0);
    ui.add_space((avail.height() * 0.14).clamp(20.0, 120.0));
    ui.vertical_centered(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
        icons::search(ui.painter(), r.center(), 46.0, theme::ACCENT);
        ui.add_space(10.0);
        ui.label(RichText::new("Search your files instantly").font(theme::strong(22.0)));
        ui.add_space(2.0);
        let total = app.status.total_entries;
        let sub = if total == 0 {
            "Building the index for the first time. You can search while it runs.".to_string()
        } else {
            format!("{} files and folders indexed", util::format_count(total))
        };
        ui.label(RichText::new(sub).color(theme::TEXT_DIM));
        ui.add_space(26.0);
    });
    let x0 = avail.center().x - card_w / 2.0;
    let tips = [
        ("report 2024", "Names containing both words"),
        ("*.pdf   ext:pdf,docx", "Filter by extension"),
        ("folder:   file:", "Only folders, or only files"),
        ("src\\main   path:proj", "Match anywhere in the full path"),
        ("↑ ↓   Enter", "Move the selection, open with the default app"),
        ("Ctrl+Enter", "Show the file in Explorer"),
        ("Ctrl+C", "Copy the selected path"),
        ("Esc", "Clear the search, or close when empty"),
    ];
    let row_h = 26.0;
    let card = Rect::from_min_size(egui::pos2(x0, ui.cursor().min.y), vec2(card_w, row_h * tips.len() as f32 + 24.0));
    ui.painter().rect(card, 10.0, theme::PANEL, Stroke::new(1.0, theme::BORDER), egui::StrokeKind::Inside);
    for (i, (k, v)) in tips.iter().enumerate() {
        let y = card.min.y + 12.0 + row_h * (i as f32 + 0.5);
        ui.painter().text(egui::pos2(card.min.x + 20.0, y), Align2::LEFT_CENTER, *k, FontId::monospace(13.0), theme::ACCENT);
        ui.painter().text(egui::pos2(card.min.x + 210.0, y), Align2::LEFT_CENTER, *v, FontId::proportional(13.5), theme::TEXT_DIM);
    }
    ui.allocate_rect(card, Sense::hover());
}
