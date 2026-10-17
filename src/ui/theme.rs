//! Dark theme, typography and shared colors.

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

pub const ACCENT: Color32 = Color32::from_rgb(0x5b, 0xa4, 0xff);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x1d, 0x34, 0x55);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x26, 0x40, 0x63);
pub const BG: Color32 = Color32::from_rgb(0x14, 0x16, 0x1a);
pub const PANEL: Color32 = Color32::from_rgb(0x1a, 0x1d, 0x22);
pub const SURFACE: Color32 = Color32::from_rgb(0x22, 0x26, 0x2d);
pub const SURFACE_HI: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x37);
pub const BORDER: Color32 = Color32::from_rgb(0x2e, 0x33, 0x3b);
pub const ROW_ALT: Color32 = Color32::from_rgb(0x17, 0x19, 0x1e);
pub const ROW_HOVER: Color32 = Color32::from_rgb(0x21, 0x25, 0x2c);
pub const TEXT: Color32 = Color32::from_rgb(0xe7, 0xe9, 0xed);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8d, 0x95, 0xa1);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x5f, 0x67, 0x72);
pub const MATCH: Color32 = Color32::from_rgb(0xff, 0xd0, 0x66);
pub const OK: Color32 = Color32::from_rgb(0x4c, 0xc3, 0x8a);
pub const WARN: Color32 = Color32::from_rgb(0xf0, 0xb4, 0x29);
pub const ERR: Color32 = Color32::from_rgb(0xf0, 0x5a, 0x5a);
pub const FOLDER: Color32 = Color32::from_rgb(0xe8, 0xb8, 0x4a);

/// Font family for emphasized text (Segoe UI Semibold when available).
pub fn strong_family() -> FontFamily {
    FontFamily::Name("strong".into())
}

pub fn strong(size: f32) -> FontId {
    FontId::new(size, strong_family())
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);

    let mut v = Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = BG;
    v.faint_bg_color = ROW_ALT;
    v.code_bg_color = SURFACE;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT_SOFT;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_shadow = egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(140) };
    v.popup_shadow = egui::Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(120) };
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::same(6);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    v.widgets.inactive.weak_bg_fill = SURFACE;
    v.widgets.inactive.bg_fill = SURFACE;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.weak_bg_fill = SURFACE_HI;
    v.widgets.hovered.bg_fill = SURFACE_HI;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3a, 0x41, 0x4b));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.weak_bg_fill = ACCENT_DIM;
    v.widgets.active.bg_fill = ACCENT_DIM;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.open.weak_bg_fill = SURFACE_HI;
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);

    ctx.all_styles_mut(|s| {
        s.interaction.selectable_labels = false;
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(12.0, 5.0);
        s.spacing.interact_size.y = 28.0;
        s.spacing.scroll.bar_width = 8.0;
        s.spacing.scroll.floating = true;
        s.text_styles = [
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Heading, FontId::new(15.0, strong_family())),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
    });
}

/// Uses the Windows UI fonts when present (Segoe UI, Segoe UI Semibold,
/// Cascadia/Consolas), keeping egui's bundled fonts as fallbacks for
/// symbols. Fonts are read from the system at runtime, not redistributed.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
    let mut load = |key: &str, files: &[&str]| -> bool {
        for f in files {
            if let Ok(bytes) = std::fs::read(dir.join(f)) {
                fonts.font_data.insert(key.to_string(), Arc::new(egui::FontData::from_owned(bytes)));
                return true;
            }
        }
        false
    };
    let ui = load("ui", &["segoeui.ttf"]);
    let semibold = load("ui-semibold", &["seguisb.ttf", "segoeuib.ttf"]);
    let mono = load("mono", &["CascadiaMono.ttf", "consola.ttf"]);
    let symbols = load("symbols", &["seguisym.ttf"]);

    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    if ui {
        prop.insert(0, "ui".into());
    }
    if symbols {
        prop.push("symbols".into());
    }
    let mut strong_list = Vec::new();
    if semibold {
        strong_list.push("ui-semibold".to_string());
    }
    strong_list.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts.families.insert(strong_family(), strong_list);
    let monos = fonts.families.entry(FontFamily::Monospace).or_default();
    if mono {
        monos.insert(0, "mono".into());
    }
    if symbols {
        monos.push("symbols".into());
    }
    ctx.set_fonts(fonts);
}

/// The window icon (the same art as the .ico embedded in the executable).
pub fn window_icon() -> egui::IconData {
    const PNG: &[u8] = include_bytes!("../../assets/icon.png");
    let n = 128;
    match image::load_from_memory_with_format(PNG, image::ImageFormat::Png) {
        Ok(img) => {
            let img = img.resize_exact(n, n, image::imageops::FilterType::Triangle).to_rgba8();
            egui::IconData { rgba: img.into_raw(), width: n, height: n }
        }
        Err(_) => egui::IconData::default(),
    }
}
