//! Look & feel: macOS system fonts with warm colors (cream and coral by day, plum and coral at night),
//! following the system appearance.

use eframe::egui::{
    self, style::ScrollStyle, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke,
    TextStyle, Visuals,
};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    /// Main content background.
    pub bg: Color32,
    pub sidebar: Color32,
    /// Raised surfaces: capsule, form groups, cards.
    pub card: Color32,
    pub text: Color32,
    /// Emphasis (we only have one font weight, so emphasis is contrast).
    pub strong: Color32,
    pub secondary: Color32,
    pub tertiary: Color32,
    pub separator: Color32,
    pub accent: Color32,
    /// Fills for controls, hover and the selected sidebar row.
    pub control: Color32,
    pub hover: Color32,
    pub selected_row: Color32,
    pub code_bg: Color32,
    pub red: Color32,
    pub amber: Color32,
    pub teal: Color32,
}

fn a(c: Color32, alpha: f32) -> Color32 {
    c.gamma_multiply(alpha)
}

impl Theme {
    pub fn of(ctx: &egui::Context) -> Theme {
        if ctx.style().visuals.dark_mode {
            Theme::dark()
        } else {
            Theme::light()
        }
    }

    /// Warm evening: plum-brown surfaces, cream text, coral accent.
    pub fn dark() -> Theme {
        let cream = Color32::from_rgb(255, 240, 228);
        Theme {
            dark: true,
            bg: Color32::from_rgb(34, 26, 30),
            sidebar: Color32::from_rgb(43, 33, 38),
            card: Color32::from_rgb(52, 40, 45),
            text: Color32::from_rgb(239, 227, 218),
            strong: Color32::from_rgb(255, 246, 239),
            secondary: Color32::from_rgb(181, 158, 148),
            tertiary: Color32::from_rgb(128, 108, 101),
            separator: a(cream, 0.09),
            accent: Color32::from_rgb(244, 132, 98),
            control: a(cream, 0.08),
            hover: a(cream, 0.06),
            selected_row: Color32::from_rgba_unmultiplied(244, 132, 98, 46),
            code_bg: Color32::from_rgb(46, 36, 41),
            red: Color32::from_rgb(255, 105, 92),
            amber: Color32::from_rgb(240, 178, 90),
            teal: Color32::from_rgb(127, 193, 169),
        }
    }

    /// Warm daylight: cream and sand surfaces, brown text, coral accent.
    pub fn light() -> Theme {
        let brown = Color32::from_rgb(70, 40, 28);
        Theme {
            dark: false,
            bg: Color32::from_rgb(255, 250, 245),
            sidebar: Color32::from_rgb(247, 237, 228),
            card: Color32::from_rgb(255, 242, 232),
            text: Color32::from_rgb(58, 42, 36),
            strong: Color32::from_rgb(36, 22, 15),
            secondary: Color32::from_rgb(140, 111, 99),
            tertiary: Color32::from_rgb(191, 166, 153),
            separator: a(brown, 0.10),
            accent: Color32::from_rgb(226, 98, 68),
            control: a(brown, 0.07),
            hover: a(brown, 0.05),
            selected_row: Color32::from_rgba_unmultiplied(226, 98, 68, 34),
            code_bg: Color32::from_rgb(248, 238, 230),
            red: Color32::from_rgb(214, 66, 52),
            amber: Color32::from_rgb(196, 124, 20),
            teal: Color32::from_rgb(76, 146, 122),
        }
    }
}

pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&Theme::dark()));
    ctx.set_visuals_of(egui::Theme::Light, visuals(&Theme::light()));
    if let Ok(t) = std::env::var("YH_THEME") {
        ctx.set_theme(if t == "light" { egui::ThemePreference::Light } else { egui::ThemePreference::Dark });
    }
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::proportional(17.0)),
            (TextStyle::Body, FontId::proportional(13.5)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 4.0);
        s.spacing.interact_size.y = 24.0;
        s.spacing.menu_margin = Margin::same(6);
        s.spacing.window_margin = Margin::same(0);
        s.spacing.scroll = ScrollStyle::floating();
        s.spacing.combo_width = 220.0;
        s.interaction.selectable_labels = true;
    });
}

/// SF Pro + SF Mono from the system (macOS), egui's bundled fonts as fallback (and for emoji/symbols).
fn fonts() -> FontDefinitions {
    let mut f = FontDefinitions::default();
    let load = |path: &str| std::fs::read(path).ok();
    if let Some(sf) = load("/System/Library/Fonts/SFNS.ttf") {
        f.font_data.insert("sf".into(), Arc::new(FontData::from_owned(sf)));
        f.families.entry(FontFamily::Proportional).or_default().insert(0, "sf".into());
    }
    // A warm serif for the welcome screen (New York, then Georgia); falls back to the UI font.
    let serif = load("/System/Library/Fonts/NewYork.ttf").or_else(|| load("/System/Library/Fonts/Supplemental/Georgia.ttf"));
    let mut serif_family = Vec::new();
    if let Some(ny) = serif {
        f.font_data.insert("serif".into(), Arc::new(FontData::from_owned(ny)));
        serif_family.push("serif".to_string());
    }
    serif_family.extend(f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    f.families.insert(FontFamily::Name("serif".into()), serif_family);
    if let Some(mono) = load("/System/Library/Fonts/SFNSMono.ttf") {
        f.font_data.insert("sfmono".into(), Arc::new(FontData::from_owned(mono)));
        f.families.entry(FontFamily::Monospace).or_default().insert(0, "sfmono".into());
    }
    f
}

fn visuals(t: &Theme) -> Visuals {
    let mut v = if t.dark { Visuals::dark() } else { Visuals::light() };
    let r6 = CornerRadius::same(6);
    v.panel_fill = t.bg;
    v.window_fill = t.bg;
    v.faint_bg_color = t.card;
    v.extreme_bg_color = if t.dark { Color32::from_rgb(36, 36, 39) } else { Color32::WHITE };
    v.code_bg_color = t.code_bg;
    v.window_stroke = Stroke::new(1.0_f32, t.separator);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    let shadow = Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if t.dark { 110 } else { 45 }) };
    v.window_shadow = shadow;
    v.popup_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(if t.dark { 90 } else { 35 }) };
    v.hyperlink_color = t.accent;
    v.selection.bg_fill = t.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, t.accent);
    v.text_cursor.stroke = Stroke::new(2.0_f32, t.accent);
    v.slider_trailing_fill = true;

    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, t.separator);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, t.text);
    w.noninteractive.bg_fill = t.bg;
    w.noninteractive.weak_bg_fill = t.card;
    w.noninteractive.corner_radius = r6;
    for (wv, fill) in [(&mut w.inactive, t.control), (&mut w.hovered, t.control), (&mut w.active, t.control), (&mut w.open, t.control)] {
        wv.corner_radius = r6;
        wv.expansion = 0.0;
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.bg_stroke = Stroke::NONE;
        wv.fg_stroke = Stroke::new(1.5_f32, t.text);
    }
    w.hovered.weak_bg_fill = t.control.gamma_multiply(1.7);
    w.hovered.bg_fill = t.control.gamma_multiply(1.7);
    w.active.weak_bg_fill = t.control.gamma_multiply(2.2);
    w.active.bg_fill = t.control.gamma_multiply(2.2);
    w.hovered.fg_stroke = Stroke::new(1.5_f32, t.strong);
    w.active.fg_stroke = Stroke::new(1.5_f32, t.strong);
    // Text fields: subtle outline, accent ring when focused.
    w.inactive.bg_stroke = Stroke::new(1.0_f32, t.separator);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, t.separator.gamma_multiply(1.6));
    w.active.bg_stroke = Stroke::new(1.0_f32, t.accent);
    v
}
