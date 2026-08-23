//! Dense, dark, maximalist visual theme + small shared widgets.

use egui::{Color32, FontFamily, FontId, RichText, TextStyle};

use crate::models::Action;

pub const BG: Color32 = Color32::from_rgb(13, 17, 23);
pub const PANEL: Color32 = Color32::from_rgb(18, 23, 31);
pub const RAISED: Color32 = Color32::from_rgb(24, 31, 41);
pub const ACCENT: Color32 = Color32::from_rgb(88, 166, 255);
pub const TEXT_DIM: Color32 = Color32::from_rgb(125, 139, 154);
pub const GOOD: Color32 = Color32::from_rgb(63, 185, 80);
pub const WARN: Color32 = Color32::from_rgb(210, 153, 34);
pub const BAD: Color32 = Color32::from_rgb(248, 81, 73);
pub const MARK_BG: Color32 = Color32::from_rgb(45, 51, 24);

pub fn model_color(idx: usize) -> Color32 {
    match idx % 6 {
        0 => Color32::from_rgb(222, 130, 76),  // claude — clay
        1 => Color32::from_rgb(64, 190, 168),  // codex — teal
        2 => Color32::from_rgb(170, 120, 235), // agy — violet
        3 => Color32::from_rgb(88, 166, 255),
        4 => Color32::from_rgb(219, 97, 162),
        _ => Color32::from_rgb(210, 153, 34),
    }
}

pub fn action_color(a: Action) -> Color32 {
    match a {
        Action::Keep => GOOD,
        Action::Rewrite => WARN,
        Action::Delete => BAD,
        Action::Flag => ACCENT,
    }
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(6.0, 3.0);
    style.spacing.button_padding = egui::vec2(7.0, 2.0);
    style.spacing.menu_margin = egui::Margin::same(4.0);
    style.spacing.window_margin = egui::Margin::same(6.0);
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    style.text_styles = [
        (TextStyle::Heading, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
        (TextStyle::Button, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(10.5, FontFamily::Proportional)),
    ]
    .into();

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(8, 11, 15);
    v.faint_bg_color = RAISED;
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.inactive.bg_fill = RAISED;
    v.widgets.inactive.weak_bg_fill = RAISED;
    v.widgets.hovered.bg_fill = Color32::from_rgb(33, 42, 55);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(33, 42, 55);
    v.selection.bg_fill = Color32::from_rgb(31, 58, 94);
    v.hyperlink_color = ACCENT;
    style.visuals = v;
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}

pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.label(
        RichText::new(format!(" {text} "))
            .small()
            .strong()
            .color(Color32::BLACK)
            .background_color(color),
    );
}

/// A `key — meaning` hint pair for the hotkey bar.
pub fn kbd(ui: &mut egui::Ui, key: &str, meaning: &str) {
    ui.label(
        RichText::new(format!(" {key} "))
            .monospace()
            .small()
            .color(Color32::WHITE)
            .background_color(Color32::from_rgb(45, 55, 70)),
    );
    ui.label(RichText::new(meaning).small().color(TEXT_DIM));
    ui.add_space(6.0);
}

pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().strong().color(ACCENT));
}

pub fn dim(text: &str) -> RichText {
    RichText::new(text).small().color(TEXT_DIM)
}
