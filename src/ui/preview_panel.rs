//! Right-hand preview pane: zoomable images and PDF pages, Word and Excel
//! documents, text, folders and file details.

use std::time::{Duration, Instant};

use egui::{Align2, Color32, ColorImage, FontId, Rect, RichText, Sense, Stroke, TextureOptions, Ui, Vec2, pos2, vec2};
use winsearch::preview::office::{DocBlock, Sheet, col_name};
use winsearch::preview::{Content, DETAIL_PX, ImageData, PREVIEW_PX, Preview, PreviewOptions};
use winsearch::util;

use super::{WinSearchApp, icons, theme};

const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 16.0;

/// Per-selection view state of the preview (reset when the selection
/// changes, kept when a sharper rendering of the same file arrives).
pub struct ViewState {
    /// 1.0 = fit to the pane (images, PDF) or 100 % text size (documents).
    pub zoom: f32,
    pub pan: Vec2,
    pub page: u32,
    pub sheet: usize,
    pub changed_at: Instant,
    pub show_details: bool,
    /// A sharper rendering was attempted and failed; do not retry.
    pub detail_failed: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            zoom: 1.0,
            pan: Vec2::ZERO,
            page: 0,
            sheet: 0,
            changed_at: Instant::now(),
            show_details: true,
            detail_failed: false,
        }
    }
}

impl ViewState {
    pub fn reset_for(&mut self, _path: &str) {
        let details = self.show_details;
        *self = ViewState { show_details: details, ..Default::default() };
    }
}

pub fn show(app: &mut WinSearchApp, ui: &mut Ui) {
    let Some(hit) = app.selected_hit().cloned() else {
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
            icons::file_icon(ui.painter(), r, icons::FileCategory::Other);
            ui.add_space(8.0);
            ui.label(RichText::new("Nothing selected").font(theme::strong(15.0)).color(theme::TEXT_DIM));
            ui.label(RichText::new("Pick a result to preview it here.").color(theme::TEXT_FAINT));
        });
        return;
    };

    // Header: icon, name, folder, actions.
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
        icons::file_icon(ui.painter(), r, icons::category(&hit.ext, hit.is_dir));
        ui.vertical(|ui| {
            ui.add(egui::Label::new(RichText::new(&hit.name).font(theme::strong(15.5))).truncate())
                .on_hover_text(&hit.name);
            ui.add(egui::Label::new(RichText::new(&hit.parent).size(12.0).color(theme::TEXT_DIM)).truncate())
                .on_hover_text(&hit.parent);
        });
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let open = egui::Button::new(RichText::new("Open").color(Color32::WHITE)).fill(theme::ACCENT.gamma_multiply(0.7));
        if ui.add(open).on_hover_text("Open with the default app (Enter)").clicked() {
            app.open_selected();
        }
        if ui.button("Show in folder").on_hover_text("Ctrl+Enter").clicked() {
            app.reveal_selected();
        }
        if ui.button("Copy path").on_hover_text("Ctrl+C").clicked() {
            let ctx = ui.ctx().clone();
            app.copy_selected_path(&ctx);
        }
    });
    ui.add_space(8.0);

    let preview = app.current_preview.clone().filter(|p| p.path == hit.path);
    let details_h = if app.view.show_details { 150.0 } else { 26.0 };
    let body_h = (ui.available_height() - details_h - 8.0).max(140.0);

    match &preview {
        None => {
            ui.allocate_ui(vec2(ui.available_width(), body_h), |ui| {
                ui.set_min_height(body_h);
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.add(egui::Spinner::new().size(20.0));
                    ui.label(RichText::new("Loading preview…").color(theme::TEXT_DIM));
                });
            });
        }
        Some(p) => {
            ui.allocate_ui(vec2(ui.available_width(), body_h), |ui| {
                ui.set_min_height(body_h);
                body(app, ui, p, body_h);
            });
        }
    }

    ui.add_space(4.0);
    details(app, ui, &hit, preview.as_deref());
}

fn body(app: &mut WinSearchApp, ui: &mut Ui, p: &Preview, h: f32) {
    match &p.content {
        Err(msg) => message(ui, msg, theme::WARN),
        Ok(Content::Image { image, original, format }) => {
            let logical = vec2(original.0 as f32, original.1 as f32);
            ensure_texture(app, ui, p, image);
            let info = format!("{} × {} px · {format}", original.0, original.1);
            zoomable_image(app, ui, p, logical, false, h, &info, None);
            request_detail_if_needed(app, p, logical, image, false);
        }
        Ok(Content::Pdf { page, page_count, index }) => {
            ensure_texture(app, ui, p, page);
            let t = vec2(page.width as f32, page.height as f32);
            let logical = t * (PREVIEW_PX as f32 / t.x.max(t.y));
            let info = format!("Page {} of {}", index + 1, page_count);
            zoomable_image(app, ui, p, logical, true, h, &info, Some((*index, *page_count)));
            request_detail_if_needed(app, p, logical, page, true);
        }
        Ok(Content::Document { blocks, truncated }) => {
            doc_toolbar(app, ui, "Word document", *truncated);
            document(app, ui, blocks, h - 40.0);
        }
        Ok(Content::Sheets { sheets }) => {
            sheet_view(app, ui, sheets, h);
        }
        Ok(Content::Text { text, .. }) if text.trim().is_empty() => {
            message(ui, "This file is empty.", theme::TEXT_DIM);
        }
        Ok(Content::Text { text, truncated, encoding }) => {
            doc_toolbar(app, ui, encoding, *truncated);
            let size = 12.5 * app.view.zoom;
            egui::ScrollArea::both()
                .id_salt(("text", &p.path))
                .max_height(h - 40.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(text.as_str()).font(FontId::monospace(size))).wrap());
                });
            zoom_with_ctrl_wheel(app, ui);
        }
        Ok(Content::Folder { entries, total, .. }) => {
            ui.label(RichText::new(format!("{} items", util::format_count(*total as u64))).color(theme::TEXT_DIM));
            egui::ScrollArea::vertical().id_salt(("folder", &p.path)).max_height(h - 30.0).auto_shrink([false, false]).show(ui, |ui| {
                for (name, is_dir) in entries {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(16.0, 18.0), Sense::hover());
                        let ext = util::ext_of(name, *is_dir);
                        icons::file_icon(ui.painter(), r, icons::category(&ext, *is_dir));
                        ui.add(egui::Label::new(name.as_str()).truncate());
                    });
                }
                if *total > entries.len() {
                    ui.label(RichText::new(format!("… and {} more", total - entries.len())).color(theme::TEXT_DIM));
                }
            });
        }
        Ok(Content::Info { note, icon }) => {
            if let Some(img) = icon {
                ensure_texture(app, ui, p, img);
            }
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                if let Some((_, t)) = app.preview_texture.as_ref().filter(|(id, _)| *id == p.id) {
                    let s = t.size_vec2();
                    let scale = (180.0 / s.x.max(s.y)).min(1.0);
                    ui.image((t.id(), s * scale));
                } else {
                    let (r, _) = ui.allocate_exact_size(vec2(72.0, 72.0), Sense::hover());
                    icons::file_icon(ui.painter(), r, icons::FileCategory::Other);
                }
                ui.add_space(10.0);
                ui.label(RichText::new(format!("No content preview for this {} type", p.kind_label.to_lowercase())).color(theme::TEXT_DIM));
                if let Some(n) = note {
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(n).size(12.5).color(theme::WARN)).wrap());
                }
            });
        }
    }
}

fn message(ui: &mut Ui, msg: &str, color: Color32) {
    ui.add_space(20.0);
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(msg).color(color)).wrap());
        });
}

/// Uploads the preview's image to the GPU once per preview result.
fn ensure_texture(app: &mut WinSearchApp, ui: &Ui, p: &Preview, img: &ImageData) {
    if app.preview_texture.as_ref().map(|(id, _)| *id) == Some(p.id) {
        return;
    }
    let ci = ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
    let tex = ui.ctx().load_texture(format!("preview-{}", p.id), ci, TextureOptions::LINEAR);
    app.preview_texture = Some((p.id, tex));
}

fn zoom_buttons(app: &mut WinSearchApp, ui: &mut Ui, fit_label: &str, scale: f32) {
    let z = &mut app.view;
    if ui.small_button("−").on_hover_text("Zoom out (Ctrl+minus or Ctrl+wheel)").clicked() {
        z.zoom = (z.zoom / 1.25).max(MIN_ZOOM);
        z.changed_at = Instant::now();
    }
    ui.label(RichText::new(format!("{:.0}%", z.zoom * scale * 100.0)).size(12.5).color(theme::TEXT_DIM));
    if ui.small_button("+").on_hover_text("Zoom in (Ctrl+plus or Ctrl+wheel)").clicked() {
        z.zoom = (z.zoom * 1.25).min(MAX_ZOOM);
        z.changed_at = Instant::now();
    }
    if ui.small_button(fit_label).clicked() {
        z.zoom = 1.0;
        z.pan = Vec2::ZERO;
        z.changed_at = Instant::now();
    }
}

#[allow(clippy::too_many_arguments)]
fn zoomable_image(
    app: &mut WinSearchApp,
    ui: &mut Ui,
    p: &Preview,
    logical: Vec2,
    allow_upscale: bool,
    h: f32,
    info: &str,
    pages: Option<(u32, u32)>,
) {
    // Images show their size relative to actual pixels; PDF pages relative to fit.
    let pre_fit = {
        let f = (ui.available_width() / logical.x).min(((h - 40.0).max(80.0)) / logical.y);
        if allow_upscale { 1.0 } else { f.min(1.0) }
    };
    // Toolbar.
    ui.horizontal(|ui| {
        zoom_buttons(app, ui, "Fit", pre_fit);
        if let Some((index, count)) = pages {
            ui.separator();
            if ui.add_enabled(index > 0, egui::Button::new("‹").small()).on_hover_text("Previous page (Ctrl+PageUp)").clicked() {
                change_page(app, p, index.saturating_sub(1));
            }
            ui.label(RichText::new(info).size(12.5));
            if ui.add_enabled(index + 1 < count, egui::Button::new("›").small()).on_hover_text("Next page (Ctrl+PageDown)").clicked() {
                change_page(app, p, index + 1);
            }
        } else {
            ui.label(RichText::new(info).size(12.5).color(theme::TEXT_DIM));
        }
    });
    ui.add_space(4.0);

    let Some((_, tex)) = app.preview_texture.clone() else { return };
    let avail = vec2(ui.available_width(), (h - 40.0).max(80.0));
    let (rect, resp) = ui.allocate_exact_size(avail, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8.0, theme::BG);

    let mut fit = (rect.width() / logical.x).min(rect.height() / logical.y);
    if !allow_upscale {
        fit = fit.min(1.0);
    }
    let v = &mut app.view;

    // Wheel or pinch zooms around the pointer; drag pans.
    if resp.hovered() {
        let (scroll, zoom_delta, pointer) =
            ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos()));
        let mut factor = zoom_delta;
        if (factor - 1.0).abs() < 1e-4 && scroll != 0.0 {
            factor = (scroll * 0.0018).exp();
        }
        if (factor - 1.0).abs() > 1e-4 {
            let old = v.zoom;
            v.zoom = (v.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
            if let Some(ptr) = pointer {
                let rel = ptr - rect.center() - v.pan;
                v.pan += rel - rel * (v.zoom / old);
            }
            v.changed_at = Instant::now();
        }
    }
    if resp.dragged() {
        v.pan += resp.drag_delta();
    }
    if resp.double_clicked() {
        // Toggle between fit and actual pixels.
        let actual = 1.0 / fit.max(1e-3);
        v.zoom = if (v.zoom - 1.0).abs() < 0.01 && (actual - 1.0).abs() > 0.05 { actual.clamp(MIN_ZOOM, MAX_ZOOM) } else { 1.0 };
        v.pan = Vec2::ZERO;
        v.changed_at = Instant::now();
    }

    let size = logical * fit * v.zoom;
    // Keep the image within reach: it may not leave the frame entirely.
    let slack = ((size - rect.size()) * 0.5).max(Vec2::ZERO);
    v.pan = v.pan.clamp(-slack, slack);
    let img_rect = Rect::from_center_size(rect.center() + v.pan, size);
    painter.image(tex.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    if v.zoom > 1.0 {
        let hint = "Drag to pan · double-click to fit";
        let galley = ui.fonts_mut(|f| f.layout_no_wrap(hint.to_string(), FontId::proportional(11.5), Color32::from_gray(230)));
        let pill = Rect::from_min_size(rect.right_bottom() - galley.size() - vec2(18.0, 14.0), galley.size() + vec2(12.0, 6.0));
        painter.rect_filled(pill, 6.0, Color32::from_black_alpha(170));
        painter.galley(pill.min + vec2(6.0, 3.0), galley, Color32::from_gray(230));
    }
    let cursor = if resp.dragged() { egui::CursorIcon::Grabbing } else if v.zoom > 1.0 { egui::CursorIcon::Grab } else { egui::CursorIcon::ZoomIn };
    resp.on_hover_cursor(cursor);
}

fn change_page(app: &mut WinSearchApp, p: &Preview, page: u32) {
    app.view.page = page;
    app.view.zoom = 1.0;
    app.view.pan = Vec2::ZERO;
    let opts = PreviewOptions { max_px: PREVIEW_PX, page };
    app.request_preview_variant(&p.path, opts);
}

/// When zoomed past the resolution of the current rendering, asks for a
/// sharper one once the zoom has settled.
fn request_detail_if_needed(app: &mut WinSearchApp, p: &Preview, logical: Vec2, img: &ImageData, pdf: bool) {
    if p.opts.max_px > PREVIEW_PX || app.view.detail_failed || app.view.changed_at.elapsed() < Duration::from_millis(250) {
        return;
    }
    let shown = logical.max_elem() * app.view.zoom.max(1.0);
    let have = img.width.max(img.height) as f32;
    let source_has_more = pdf || logical.max_elem() > have + 1.0;
    if source_has_more && shown > have * 1.15 && app.pending_variant.is_none() {
        let opts = PreviewOptions { max_px: if pdf { 3072 } else { DETAIL_PX }, page: app.view.page };
        app.request_preview_variant(&p.path, opts);
    }
}

fn zoom_with_ctrl_wheel(app: &mut WinSearchApp, ui: &Ui) {
    let pane = ui.max_rect();
    let (hover, zd) = ui.input(|i| (i.pointer.hover_pos(), i.zoom_delta()));
    if hover.is_some_and(|h| pane.contains(h)) && (zd - 1.0).abs() > 1e-4 {
        app.view.zoom = (app.view.zoom * zd).clamp(0.5, 4.0);
    }
}

fn doc_toolbar(app: &mut WinSearchApp, ui: &mut Ui, label: &str, truncated: bool) {
    ui.horizontal(|ui| {
        zoom_buttons(app, ui, "Reset", 1.0);
        ui.separator();
        ui.label(RichText::new(label).size(12.5).color(theme::TEXT_DIM));
        if truncated {
            ui.label(RichText::new("· partial preview").size(12.5).color(theme::WARN))
                .on_hover_text("Only the beginning of this file is shown.");
        }
    });
    ui.add_space(4.0);
}

fn document(app: &mut WinSearchApp, ui: &mut Ui, blocks: &[DocBlock], h: f32) {
    let z = app.view.zoom.clamp(0.5, 4.0);
    egui::Frame::new()
        .fill(Color32::from_rgb(0xf7, 0xf7, 0xf5))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::same((14.0 * z.min(2.0)) as i8))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("docx")
                .max_height(h - 20.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0 * z;
                    let ink = Color32::from_rgb(0x1f, 0x23, 0x28);
                    for b in blocks {
                        match b {
                            DocBlock::Paragraph { runs, level, list } => {
                                if runs.iter().all(|r| r.text.trim().is_empty()) {
                                    ui.add_space(4.0 * z);
                                    continue;
                                }
                                let size = match level {
                                    1 => 22.0,
                                    2 => 18.0,
                                    3 => 16.0,
                                    4..=6 => 15.0,
                                    _ => 13.5,
                                } * z;
                                let mut job = egui::text::LayoutJob::default();
                                if *list {
                                    job.append("•  ", 0.0, egui::TextFormat { font_id: FontId::proportional(size), color: ink, ..Default::default() });
                                }
                                for r in runs {
                                    let family = if r.bold || *level > 0 { theme::strong_family() } else { egui::FontFamily::Proportional };
                                    job.append(
                                        &r.text,
                                        0.0,
                                        egui::TextFormat { font_id: FontId::new(size, family), color: ink, italics: r.italic, ..Default::default() },
                                    );
                                }
                                job.wrap.max_width = ui.available_width();
                                ui.label(job);
                            }
                            DocBlock::Table { rows } => {
                                egui::Grid::new(ui.next_auto_id()).spacing([12.0 * z, 4.0 * z]).show(ui, |ui| {
                                    for row in rows.iter().take(200) {
                                        for c in row.iter().take(20) {
                                            ui.label(RichText::new(c).size(12.5 * z).color(ink));
                                        }
                                        ui.end_row();
                                    }
                                });
                            }
                        }
                    }
                });
        });
    zoom_with_ctrl_wheel(app, ui);
}

fn sheet_view(app: &mut WinSearchApp, ui: &mut Ui, sheets: &[Sheet], h: f32) {
    if app.view.sheet >= sheets.len() {
        app.view.sheet = 0;
    }
    ui.horizontal(|ui| {
        zoom_buttons(app, ui, "Reset", 1.0);
        ui.separator();
        let sheet = &sheets[app.view.sheet];
        let rows = sheet.rows.len();
        let cols = sheet.rows.first().map_or(0, |r| r.len());
        ui.label(RichText::new(format!("{rows} rows × {cols} columns")).size(12.5).color(theme::TEXT_DIM));
        if sheet.truncated {
            ui.label(RichText::new("· first rows only").size(12.5).color(theme::WARN));
        }
    });
    if sheets.len() > 1 {
        ui.horizontal_wrapped(|ui| {
            for (i, s) in sheets.iter().enumerate() {
                if ui.add(egui::Button::selectable(app.view.sheet == i, s.name.as_str())).clicked() {
                    app.view.sheet = i;
                }
            }
        });
    }
    ui.add_space(4.0);
    let sheet = &sheets[app.view.sheet];
    let z = app.view.zoom.clamp(0.5, 4.0);
    let font = FontId::proportional(12.5 * z);
    let row_h = 22.0 * z;
    let ncols = sheet.rows.first().map_or(0, |r| r.len());
    let char_w = 7.2 * z;
    let widths: Vec<f32> = (0..ncols)
        .map(|c| {
            let longest = sheet.rows.iter().take(200).map(|r| r[c].chars().count()).max().unwrap_or(0);
            (longest as f32 * char_w + 14.0 * z).clamp(48.0 * z, 260.0 * z)
        })
        .collect();
    let gutter = 40.0 * z;
    let total_w = gutter + widths.iter().sum::<f32>();
    let total_h = row_h * (sheet.rows.len() + 1) as f32;
    let avail_h = (ui.available_height()).min(h).max(80.0);
    egui::ScrollArea::both()
        .id_salt(("sheet", app.view.sheet))
        .max_height(avail_h)
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let (rect, _) = ui.allocate_exact_size(vec2(total_w, total_h), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 0.0, theme::BG);
            let origin = rect.min;
            let first = ((viewport.min.y / row_h).floor() as usize).saturating_sub(1);
            let last = (((viewport.max.y / row_h).ceil() as usize) + 1).min(sheet.rows.len() + 1);
            let grid = Stroke::new(1.0, theme::BORDER);
            for r in first..last {
                let y = origin.y + r as f32 * row_h;
                let mut x = origin.x;
                let header = r == 0;
                // Row number gutter.
                let gr = Rect::from_min_size(pos2(x, y), vec2(gutter, row_h));
                p.rect_filled(gr, 0.0, theme::SURFACE);
                if !header {
                    p.text(gr.center(), Align2::CENTER_CENTER, r.to_string(), FontId::proportional(11.0 * z), theme::TEXT_DIM);
                }
                x += gutter;
                for (c, w) in widths.iter().enumerate() {
                    let cell = Rect::from_min_size(pos2(x, y), vec2(*w, row_h));
                    if cell.max.x >= origin.x + viewport.min.x && cell.min.x <= origin.x + viewport.max.x {
                        if header {
                            p.rect_filled(cell, 0.0, theme::SURFACE);
                            p.text(cell.center(), Align2::CENTER_CENTER, col_name(c), FontId::proportional(11.5 * z), theme::TEXT_DIM);
                        } else {
                            let text = &sheet.rows[r - 1][c];
                            if !text.is_empty() {
                                let numeric = text.parse::<f64>().is_ok();
                                let clip = p.with_clip_rect(cell.shrink(1.0));
                                let (anchor, pos) = if numeric {
                                    (Align2::RIGHT_CENTER, cell.right_center() - vec2(6.0 * z, 0.0))
                                } else {
                                    (Align2::LEFT_CENTER, cell.left_center() + vec2(6.0 * z, 0.0))
                                };
                                clip.text(pos, anchor, text, font.clone(), theme::TEXT);
                            }
                        }
                        p.rect_stroke(cell, 0.0, grid, egui::StrokeKind::Inside);
                    }
                    x += w;
                }
            }
        });
    zoom_with_ctrl_wheel(app, ui);
}

fn details(app: &mut WinSearchApp, ui: &mut Ui, hit: &winsearch::model::SearchHit, p: Option<&Preview>) {
    let header = ui.add(
        egui::Button::new(
            RichText::new(if app.view.show_details { "▾ Details" } else { "▸ Details" })
                .font(theme::strong(13.0))
                .color(theme::TEXT_DIM),
        )
        .frame(false),
    );
    if header.clicked() {
        app.view.show_details = !app.view.show_details;
    }
    if !app.view.show_details {
        return;
    }
    let meta = p.and_then(|p| p.meta.clone());
    egui::Grid::new("meta").num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
        let row = |ui: &mut Ui, k: &str, v: String| {
            ui.label(RichText::new(k).size(12.5).color(theme::TEXT_DIM));
            ui.add(egui::Label::new(RichText::new(v).size(12.5)).truncate());
            ui.end_row();
        };
        if !hit.is_dir {
            let size = meta.as_ref().map(|m| m.size).unwrap_or(hit.size);
            row(ui, "Size", format!("{}  ({} bytes)", util::format_size(size), util::format_count(size)));
        }
        row(ui, "Modified", util::format_time(meta.as_ref().and_then(|m| m.modified).or(hit.modified)));
        row(ui, "Created", util::format_time(meta.as_ref().and_then(|m| m.created).or(hit.created)));
        if let Some(m) = &meta {
            row(ui, "Accessed", util::format_time(m.accessed));
            let attrs = m.attribute_labels();
            if !attrs.is_empty() {
                row(ui, "Attributes", attrs.join(", "));
            }
        }
        if let Some(p) = p {
            row(ui, "Preview", format!("{} · rendered in {:.0} ms", p.kind_label, p.elapsed.as_secs_f64() * 1000.0));
        }
    });
}
