//! Look & feel: macOS system fonts and colors, light + dark, following the system appearance.

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

    pub fn dark() -> Theme {
        let white = Color32::WHITE;
        Theme {
            dark: true,
            bg: Color32::from_rgb(30, 30, 32),
            sidebar: Color32::from_rgb(40, 39, 43),
            card: Color32::from_rgb(44, 44, 47),
            text: Color32::from_rgb(223, 223, 227),
            strong: Color32::from_rgb(250, 250, 252),
            secondary: Color32::from_rgb(152, 152, 158),
            tertiary: Color32::from_rgb(105, 105, 110),
            separator: a(white, 0.09),
            accent: Color32::from_rgb(10, 132, 255),
            control: a(white, 0.08),
            hover: a(white, 0.06),
            selected_row: a(white, 0.10),
            code_bg: Color32::from_rgb(38, 38, 41),
            red: Color32::from_rgb(255, 69, 58),
            amber: Color32::from_rgb(255, 179, 64),
            teal: Color32::from_rgb(64, 200, 190),
        }
    }

    pub fn light() -> Theme {
        let black = Color32::BLACK;
        Theme {
            dark: false,
            bg: Color32::from_rgb(255, 255, 255),
            sidebar: Color32::from_rgb(242, 241, 244),
            card: Color32::from_rgb(246, 246, 248),
            text: Color32::from_rgb(40, 40, 44),
            strong: Color32::from_rgb(10, 10, 12),
            secondary: Color32::from_rgb(128, 128, 134),
            tertiary: Color32::from_rgb(172, 172, 178),
            separator: a(black, 0.09),
            accent: Color32::from_rgb(0, 122, 255),
            control: a(black, 0.06),
            hover: a(black, 0.045),
            selected_row: a(black, 0.075),
            code_bg: Color32::from_rgb(243, 243, 246),
            red: Color32::from_rgb(255, 59, 48),
            amber: Color32::from_rgb(214, 136, 0),
            teal: Color32::from_rgb(0, 160, 150),
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
