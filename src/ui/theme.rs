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
/// Backdrop for code views — darker than the panel, so an excerpt reads as a
/// quotation from a file rather than as part of the chrome.
pub const CODE_BG: Color32 = Color32::from_rgb(8, 11, 15);

// Diff roles in a code view. The text itself carries syntax colour, so the
// role has to live in the background tint and in the gutter marker.
pub const ADD_BG: Color32 = Color32::from_rgb(14, 33, 21);
pub const DEL_BG: Color32 = Color32::from_rgb(39, 18, 18);
pub const ADDED: Color32 = Color32::from_rgb(126, 231, 135);
pub const REMOVED: Color32 = Color32::from_rgb(220, 118, 112);
/// Base colour for code the highlighter does not claim.
pub const CODE: Color32 = Color32::from_rgb(201, 209, 217);
/// The same, for lines that are only context — dimmed so the region under
/// review stays the brightest thing in the pane.
pub const CODE_CTX: Color32 = Color32::from_rgb(150, 161, 174);
pub const GUTTER: Color32 = Color32::from_rgb(88, 99, 112);

/// Colours for configured models, in settings order.
///
/// Two things this palette deliberately is not. It is not branded: an earlier
/// version painted the first three models in each vendor's own colour, which
/// read as a claim that the colour identifies the *model* when it only ever
/// identified the model — and blinding, which shuffles models, made that
/// reading actively wrong. And it is not the verdict palette: these are eight
/// light hues spread around the wheel, where [`GOOD`], [`WARN`], [`BAD`] and
/// [`ACCENT`] are saturated and mid-dark, so a card outlined in one of these
/// reads as an identity rather than as a judgement about the code.
const MODEL_COLORS: [Color32; 8] = [
    Color32::from_rgb(120, 198, 226), // sky
    Color32::from_rgb(224, 133, 166), // rose
    Color32::from_rgb(207, 224, 174), // lime
    Color32::from_rgb(166, 129, 207), // violet
    Color32::from_rgb(232, 211, 125), // gold
    Color32::from_rgb(133, 224, 191), // jade
    Color32::from_rgb(209, 97, 194),  // orchid
    Color32::from_rgb(174, 180, 224), // iris
];

/// The colour for a configured model — by position, not by model identity.
/// Ordered so the first few models, the only ones most reviews use, are far
/// apart on the wheel rather than neighbours on it.
pub fn model_color(idx: usize) -> Color32 {
    MODEL_COLORS[idx % MODEL_COLORS.len()]
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
        (
            TextStyle::Heading,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(12.5, FontFamily::Proportional)),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
        (
            TextStyle::Button,
            FontId::new(12.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(10.5, FontFamily::Proportional),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The model palette has to stay distinguishable from itself and from the
    /// verdict colours: a model card is outlined in its assigned colour with a
    /// verdict badge painted inside it, and neither may be read as the other.
    #[test]
    fn model_colours_are_distinct_from_each_other_and_from_the_verdict_palette() {
        let verdicts = [GOOD, WARN, BAD, ACCENT, ADDED, REMOVED];
        for (i, a) in MODEL_COLORS.iter().enumerate() {
            for b in &MODEL_COLORS[i + 1..] {
                assert!(
                    distance(*a, *b) > 40.0,
                    "model colours {a:?} and {b:?} are too close"
                );
            }
            for v in &verdicts {
                assert!(
                    distance(*a, *v) > 50.0,
                    "model colour {a:?} reads as verdict {v:?}"
                );
            }
            // Light enough to carry small text and a hairline stroke on the
            // panel behind it.
            assert!(distance(*a, BG) > 250.0, "{a:?} is too dim on the panel");
        }
    }

    /// Model positions wrap around the palette rather than running off the end.
    #[test]
    fn model_colours_wrap() {
        assert_eq!(model_color(0), model_color(MODEL_COLORS.len()));
        assert_eq!(model_color(3), model_color(MODEL_COLORS.len() + 3));
    }

    fn distance(a: Color32, b: Color32) -> f32 {
        let d = |x: u8, y: u8| (x as f32 - y as f32).powi(2);
        (d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b())).sqrt()
    }
}
