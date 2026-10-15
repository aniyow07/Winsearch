//! Small vector icons painted directly, so they stay crisp at any DPI and
//! do not depend on emoji fonts.

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};

use super::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileCategory {
    Folder,
    Image,
    Document,
    Code,
    Archive,
    Media,
    Program,
    Other,
}

pub fn category(ext: &str, is_dir: bool) -> FileCategory {
    if is_dir {
        return FileCategory::Folder;
    }
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff" | "svg" | "heic" | "psd" | "raw" => {
            FileCategory::Image
        }
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "rtf" | "txt" | "md" | "csv"
        | "epub" => FileCategory::Document,
        "rs" | "c" | "h" | "cpp" | "hpp" | "cs" | "java" | "go" | "py" | "js" | "ts" | "tsx" | "jsx" | "html"
        | "css" | "json" | "xml" | "yaml" | "yml" | "toml" | "sql" | "sh" | "ps1" | "lua" | "rb" | "php" | "kt"
        | "swift" | "ini" | "cfg" => FileCategory::Code,
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "bz2" | "zst" | "iso" | "cab" | "msi" => FileCategory::Archive,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" => {
            FileCategory::Media
        }
        "exe" | "dll" | "bat" | "cmd" | "lnk" | "sys" | "appx" | "msix" => FileCategory::Program,
        _ => FileCategory::Other,
    }
}

pub fn category_color(c: FileCategory) -> Color32 {
    match c {
        FileCategory::Folder => theme::FOLDER,
        FileCategory::Image => Color32::from_rgb(0x5c, 0xc9, 0x8f),
        FileCategory::Document => Color32::from_rgb(0x6a, 0xa8, 0xff),
        FileCategory::Code => Color32::from_rgb(0xb3, 0x8b, 0xff),
        FileCategory::Archive => Color32::from_rgb(0xf0, 0x99, 0x55),
        FileCategory::Media => Color32::from_rgb(0xff, 0x7a, 0xa8),
        FileCategory::Program => Color32::from_rgb(0xff, 0x6b, 0x6b),
        FileCategory::Other => Color32::from_rgb(0x9a, 0xa3, 0xaf),
    }
}

/// Paints a folder or document glyph inside `rect` (roughly square).
pub fn file_icon(p: &Painter, rect: Rect, cat: FileCategory) {
    let color = category_color(cat);
    let s = rect.width().min(rect.height());
    let c = rect.center();
    if cat == FileCategory::Folder {
        let body = Rect::from_center_size(c + vec2(0.0, s * 0.06), vec2(s * 0.92, s * 0.66));
        let tab = Rect::from_min_size(body.min - vec2(0.0, s * 0.12), vec2(s * 0.42, s * 0.2));
        p.rect_filled(tab, CornerRadius::same(2), color.gamma_multiply(0.8));
        p.rect_filled(body, CornerRadius::same(2), color);
        return;
    }
    let w = s * 0.7;
    let h = s * 0.9;
    let r = Rect::from_center_size(c, vec2(w, h));
    let fold = s * 0.26;
    let pts = vec![
        r.left_top(),
        pos2(r.right() - fold, r.top()),
        pos2(r.right(), r.top() + fold),
        r.right_bottom(),
        r.left_bottom(),
    ];
    p.add(egui::Shape::convex_polygon(pts, color.gamma_multiply(0.22), Stroke::new(1.2, color)));
    let f = vec![pos2(r.right() - fold, r.top()), pos2(r.right() - fold, r.top() + fold), pos2(r.right(), r.top() + fold)];
    p.add(egui::Shape::line(f, Stroke::new(1.2, color)));
}

/// Magnifier glyph.
pub fn search(p: &Painter, center: Pos2, size: f32, color: Color32) {
    let r = size * 0.32;
    let c = center - vec2(size * 0.08, size * 0.08);
    p.circle_stroke(c, r, Stroke::new(size * 0.11, color));
    let d = r * std::f32::consts::FRAC_1_SQRT_2;
    p.line_segment([c + vec2(d, d), c + vec2(r * 1.55, r * 1.55)], Stroke::new(size * 0.13, color));
}

/// Gear glyph used on the settings button.
pub fn gear(p: &Painter, center: Pos2, size: f32, color: Color32) {
    let teeth = 8;
    let r_out = size * 0.46;
    let r_in = size * 0.33;
    let mut pts = Vec::new();
    for i in 0..teeth * 4 {
        let a = i as f32 / (teeth * 4) as f32 * std::f32::consts::TAU;
        let r = if (i / 2) % 2 == 0 { r_out } else { r_in };
        pts.push(center + vec2(a.cos(), a.sin()) * r);
    }
    p.add(egui::Shape::closed_line(pts, Stroke::new(size * 0.09, color)));
    p.circle_stroke(center, size * 0.14, Stroke::new(size * 0.09, color));
}

pub fn dot(p: &Painter, center: Pos2, color: Color32) {
    p.circle_filled(center, 4.0, color);
    p.circle_stroke(center, 6.5, Stroke::new(1.0, color.gamma_multiply(0.35)));
}

#[allow(dead_code)]
pub fn outline(p: &Painter, r: Rect, color: Color32) {
    p.rect_stroke(r, CornerRadius::same(6), Stroke::new(1.0, color), StrokeKind::Inside);
}
