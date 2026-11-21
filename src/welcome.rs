//! The first-run welcome: Alex asks a few questions out loud and you answer by voice (or type).
//! The answers become the "About you" note every agent reads (memory::you_note).

use eframe::egui::{self, pos2, vec2, Align2, Color32, CornerRadius, FontFamily, FontId, Id, Mesh, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use std::time::Instant;

pub const LANGUAGES: [(&str, &str, &[&str]); 10] = [
    ("en", "English", &["english", "englisch"]),
    ("de", "German", &["german", "deutsch"]),
    ("fr", "French", &["french", "französisch", "francais", "français"]),
    ("es", "Spanish", &["spanish", "spanisch", "español"]),
    ("it", "Italian", &["italian", "italienisch"]),
    ("nl", "Dutch", &["dutch", "niederländisch", "nederlands"]),
    ("pt", "Portuguese", &["portuguese", "portugiesisch"]),
    ("pl", "Polish", &["polish", "polnisch"]),
    ("sv", "Swedish", &["swedish", "schwedisch"]),
    ("tr", "Turkish", &["turkish", "türkisch"]),
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Step {
    Name,
    Work,
    Projects,
    Style,
    Languages,
    Done,
}

const STEPS: [Step; 6] = [Step::Name, Step::Work, Step::Projects, Step::Style, Step::Languages, Step::Done];

pub enum Outcome {
    Continue,
    /// (the About you note, your language codes)
    Finished(String, Vec<String>),
    Skipped,
}

pub struct Welcome {
    step: usize,
    pub name: String,
    name_guess: String,
    work: String,
    /// Folders you picked (names in the projects folder).
    picked: Vec<String>,
    /// Projects you named that aren't folders here.
    mentioned: Vec<String>,
    folders: Vec<String>,
    all_folders: Vec<String>,
    short: bool,
    pub langs: Vec<String>,
    /// The question for this step was handed to the voice.
    spoken: bool,
    /// What you said for this step (shown as a little quote).
    heard: String,
    typed: String,
    typing: bool,
    started: Instant,
    /// Answers arrive here; we move on a moment later so you can see what was understood.
    advance_at: Option<Instant>,
    projects_dir: String,
}

impl Welcome {
    pub fn new(name_guess: String, langs: Vec<String>, projects_dir: &str) -> Self {
        let dir = std::path::Path::new(projects_dir);
        let folders = crate::memory::recent_projects(dir, 8);
        let mut all_folders: Vec<String> = std::fs::read_dir(dir)
            .map(|r| r.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect())
            .unwrap_or_default();
        all_folders.sort();
        Welcome {
            step: 0,
            name: String::new(),
            name_guess,
            work: String::new(),
            picked: Vec::new(),
            mentioned: Vec::new(),
            folders,
            all_folders,
            short: true,
            langs,
            spoken: false,
            heard: String::new(),
            typed: String::new(),
            typing: false,
            started: Instant::now(),
            advance_at: None,
            projects_dir: projects_dir.to_string(),
        }
    }

    fn step(&self) -> Step {
        STEPS[self.step.min(STEPS.len() - 1)]
    }

    fn first_name(&self) -> String {
        let n = if self.name.is_empty() { &self.name_guess } else { &self.name };
        n.split_whitespace().next().unwrap_or("").to_string()
    }

    /// What Alex says for this step (also shown, large).
    pub fn question(&self) -> String {
        let n = self.first_name();
        match self.step() {
            Step::Name => "Hi, I'm Alex. I'll be your main point of contact here, with a small team behind me. \
                           Before we start: what should I call you?"
                .into(),
            Step::Work => format!("Nice to meet you{}! So, what do you do? Tell me a little about your work.", comma(&n)),
            Step::Projects => "What are you working on these days? Just say the names, and I'll find them.".into(),
            Step::Style => "When I answer you, do you like it short and to the point, or with more detail?".into(),
            Step::Languages => "And which languages do you speak? Then I'll understand you better.".into(),
            Step::Done => format!("Lovely{}. I've got what I need. Let's make something great together.", comma(&n)),
        }
    }

    /// The question still has to be read aloud (once per step).
    pub fn take_question_to_speak(&mut self) -> Option<String> {
        if self.spoken {
            return None;
        }
        self.spoken = true;
        Some(self.question())
    }

    /// Something you said (or typed) for the current step.
    pub fn hear(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || self.advance_at.is_some() {
            return;
        }
        let lower = text.to_lowercase();
        match self.step() {
            Step::Name => self.name = parse_name(text, &self.name_guess),
            Step::Work => self.work = text.to_string(),
            Step::Projects => {
                let (found, other) = match_projects(text, &self.all_folders);
                for f in found {
                    if !self.picked.contains(&f) {
                        self.picked.push(f);
                    }
                }
                for o in other {
                    if !self.mentioned.contains(&o) {
                        self.mentioned.push(o);
                    }
                }
            }
            Step::Style => {
                if ["detail", "long", "explain", "thorough", "ausführlich"].iter().any(|w| lower.contains(w)) {
                    self.short = false;
                } else if ["short", "brief", "concise", "quick", "point", "kurz"].iter().any(|w| lower.contains(w)) {
                    self.short = true;
                }
            }
            Step::Languages => {
                let said: Vec<String> = LANGUAGES.iter().filter(|(_, _, words)| words.iter().any(|w| lower.contains(w))).map(|(c, _, _)| c.to_string()).collect();
                if !said.is_empty() {
                    self.langs = said;
                }
            }
            Step::Done => return,
        }
        self.heard = text.to_string();
        self.advance_at = Some(Instant::now());
    }

    fn next(&mut self) {
        if self.step() == Step::Name && self.name.is_empty() {
            self.name = self.name_guess.clone();
        }
        self.step = (self.step + 1).min(STEPS.len() - 1);
        self.spoken = false;
        self.heard.clear();
        self.typed.clear();
        self.advance_at = None;
    }

    fn back(&mut self) {
        self.step = self.step.saturating_sub(1);
        self.spoken = false;
        self.heard.clear();
        self.advance_at = None;
    }

    /// The About you note from the answers.
    pub fn note(&self) -> String {
        let dir = self.projects_dir.replacen(&crate::model::home_dir(), "~", 1);
        let mut projects: Vec<String> = self.picked.iter().map(|p| format!("- {p} — {dir}/{p}")).collect();
        projects.extend(self.mentioned.iter().map(|m| format!("- {m} (no folder found yet; ask me where it lives)")));
        let names: Vec<&str> = LANGUAGES.iter().filter(|(c, _, _)| self.langs.iter().any(|l| l == c)).map(|(_, n, _)| *n).collect();
        let answers = if self.short { "Short and direct: one or two sentences unless I ask for more" } else { "Detailed, with explanations" };
        let name = if self.name.is_empty() { &self.name_guess } else { &self.name };
        crate::memory::you_note(name, &self.work, &projects.join("\n"), &names, answers)
    }

    /// Draws the whole window. `live`: what you're saying right now; `level`: mic level 0..1.
    pub fn show(&mut self, ctx: &egui::Context, live: &str, level: f32, speaking: bool, alex: Color32) -> Outcome {
        let dark = ctx.style().visuals.dark_mode;
        let pal = Palette::new(dark);
        let mut outcome = Outcome::Continue;
        if self.advance_at.is_some_and(|t| t.elapsed().as_millis() > 1100) {
            self.next();
        }
        let time = self.started.elapsed().as_secs_f32();
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
            let screen = ui.max_rect();
            backdrop(ui.painter(), screen, &pal, time);

            let col = Rect::from_center_size(screen.center(), vec2(screen.width().min(640.0) - 48.0, screen.height() - 40.0));
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(col).layout(egui::Layout::top_down(egui::Align::Center)));
            let ui = &mut ui;
            ui.add_space((col.height() * 0.12).max(20.0));

            // Alex, breathing; ripples while it talks, glows when it hears you.
            let (orb, _) = ui.allocate_exact_size(vec2(160.0, 150.0), Sense::hover());
            draw_orb(ui.painter(), orb.center(), alex, &pal, time, if speaking { 1.0 } else { 0.0 }, if live.is_empty() { 0.0 } else { level });
            ui.add_space(18.0);

            // Progress
            let (dots, _) = ui.allocate_exact_size(vec2(120.0, 10.0), Sense::hover());
            for i in 0..STEPS.len() - 1 {
                let c = dots.center() + vec2((i as f32 - 2.0) * 18.0, 0.0);
                let on = i < self.step || (i == self.step && self.step() != Step::Done);
                ui.painter().circle_filled(c, if i == self.step { 4.5 } else { 3.5 }, if on { pal.accent } else { pal.faint });
            }
            ui.add_space(26.0);

            ui.add(egui::Label::new(RichText::new(self.question()).font(FontId::new(30.0, FontFamily::Name("serif".into()))).color(pal.ink)).wrap());
            ui.add_space(22.0);

            // What we heard / what you're saying now
            let shown = if !live.is_empty() { live.to_string() } else { self.heard.clone() };
            if !shown.is_empty() {
                ui.label(RichText::new(format!("“{shown}”")).font(FontId::new(19.0, FontFamily::Name("serif".into()))).italics().color(pal.soft));
                ui.add_space(14.0);
            } else if self.step() != Step::Done {
                ui.label(RichText::new(if speaking { "Alex is talking… just answer whenever you like." } else { "I'm listening. Just talk to me." }).size(14.0).color(pal.soft));
                ui.add_space(14.0);
            }

            // Tap-friendly answers for some steps
            match self.step() {
                Step::Name if self.name.is_empty() && !self.name_guess.is_empty() => {
                    let guess = self.name_guess.clone();
                    if pills(ui, &pal, &[(&format!("Call me {}", first(&guess)), false)]) == Some(0) {
                        self.hear(first(&guess));
                    }
                }
                Step::Projects => {
                    let mut items: Vec<(String, bool)> = self.folders.iter().map(|f| (f.clone(), self.picked.contains(f))).collect();
                    items.extend(self.mentioned.iter().map(|m| (m.clone(), true)));
                    let refs: Vec<(&str, bool)> = items.iter().map(|(s, b)| (s.as_str(), *b)).collect();
                    if let Some(i) = pills(ui, &pal, &refs) {
                        let name = items[i].0.clone();
                        if let Some(p) = self.picked.iter().position(|x| *x == name) {
                            self.picked.remove(p);
                        } else if let Some(p) = self.mentioned.iter().position(|x| *x == name) {
                            self.mentioned.remove(p);
                        } else {
                            self.picked.push(name);
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("Tap the ones that matter, or just say their names.").size(12.5).color(pal.soft));
                }
                Step::Style => {
                    if let Some(i) = pills(ui, &pal, &[("Short and to the point", self.short), ("More detail", !self.short)]) {
                        self.short = i == 0;
                    }
                }
                Step::Languages => {
                    let items: Vec<(&str, bool)> = LANGUAGES.iter().map(|(c, n, _)| (*n, self.langs.iter().any(|l| l == c))).collect();
                    if let Some(i) = pills(ui, &pal, &items) {
                        let code = LANGUAGES[i].0.to_string();
                        if let Some(p) = self.langs.iter().position(|l| *l == code) {
                            self.langs.remove(p);
                        } else {
                            self.langs.push(code);
                        }
                    }
                }
                _ => {}
            }
            ui.add_space(18.0);

            // Typing instead of talking
            if self.typing && self.step() != Step::Done {
                let r = ui.add(egui::TextEdit::singleline(&mut self.typed).hint_text("Type your answer and press Enter").desired_width(380.0).margin(vec2(12.0, 8.0)));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let t = std::mem::take(&mut self.typed);
                    self.hear(&t);
                } else if !r.has_focus() && self.typed.is_empty() {
                    r.request_focus();
                }
                ui.add_space(10.0);
            }

            // Buttons
            ui.horizontal(|ui| {
                let w = 360.0;
                ui.add_space((ui.available_width() - w).max(0.0) / 2.0);
                if self.step() == Step::Done {
                    ui.add_space(110.0);
                    if big_button(ui, &pal, "Let’s go") {
                        outcome = Outcome::Finished(self.note(), self.langs.clone());
                    }
                    return;
                }
                if self.step > 0 && soft_button(ui, &pal, "Back") {
                    self.back();
                }
                if !self.typing && soft_button(ui, &pal, "Type instead") {
                    self.typing = true;
                }
                let ready = match self.step() {
                    Step::Projects | Step::Style | Step::Languages => true,
                    Step::Name => !self.name.is_empty() || !self.name_guess.is_empty(),
                    _ => !self.heard.is_empty(),
                };
                if big_button(ui, &pal, if ready { "Next" } else { "Skip" }) {
                    if !self.typed.trim().is_empty() {
                        let t = std::mem::take(&mut self.typed);
                        self.hear(&t);
                    }
                    self.next();
                }
            });
            ui.add_space(20.0);
            if self.step() != Step::Done && ui.add(egui::Label::new(RichText::new("Skip all of this for now").size(12.0).color(pal.soft)).sense(Sense::click())).clicked() {
                outcome = Outcome::Skipped;
            }
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
        outcome
    }
}

fn comma(name: &str) -> String {
    if name.is_empty() { String::new() } else { format!(", {name}") }
}

fn first(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// "My name is Sam" / "call me Sam" / "Sam" / "yes" (keep the guess).
pub fn parse_name(text: &str, guess: &str) -> String {
    let t = text.trim().trim_end_matches(['.', '!', '?']).trim();
    let lower = t.to_lowercase();
    let yes = ["yes", "yeah", "yep", "sure", "correct", "that's right", "that's me", "ja", "genau", "right"];
    if !guess.is_empty() && (yes.iter().any(|y| lower == *y || lower.starts_with(&format!("{y},")) || lower.starts_with(&format!("{y} "))) || lower.contains(&first(guess).to_lowercase())) {
        return first(guess).to_string();
    }
    let mut rest = t;
    for p in ["my name is", "my name's", "i'm", "i am", "call me", "it's", "it is", "this is", "ich bin", "ich heiße", "nenn mich"] {
        if let Some(i) = lower.find(p) {
            rest = t[i + p.len()..].trim();
            break;
        }
    }
    let words: Vec<&str> = rest.split_whitespace().take(3).map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-')).filter(|w| !w.is_empty()).collect();
    let name = words.iter().take(2).map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    });
    name.collect::<Vec<_>>().join(" ")
}

fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Folders you named (spoken loosely: "nader io" for nader-io), and other names you said.
pub fn match_projects(text: &str, folders: &[String]) -> (Vec<String>, Vec<String>) {
    let spoken = norm(text);
    let mut found: Vec<String> = folders.iter().filter(|f| norm(f).len() >= 3 && spoken.contains(&norm(f))).cloned().collect();
    // Prefer the longest names ("privacy-guard" over "guard").
    found.sort_by_key(|f| std::cmp::Reverse(f.len()));
    let mut keep: Vec<String> = Vec::new();
    for f in found {
        if !keep.iter().any(|k| norm(k).contains(&norm(&f))) {
            keep.push(f);
        }
    }
    // The other names: pieces between "and" / commas that aren't one of the folders.
    let mut other = Vec::new();
    let lower = text.to_lowercase().replace(" and ", ",").replace(" und ", ",").replace(" also ", ",");
    for piece in lower.split([',', ';', '.']) {
        let piece = piece.trim();
        let piece = ["i'm working on", "i am working on", "working on", "mostly", "there's", "there is", "it's", "my"]
            .iter()
            .fold(piece.to_string(), |p, w| p.replace(w, ""));
        let piece = piece.trim();
        if piece.is_empty() || piece.split_whitespace().count() > 4 || keep.iter().any(|k| norm(piece).contains(&norm(k)) || norm(k).contains(&norm(piece))) {
            continue;
        }
        let title: String = piece.split_whitespace().map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }).collect::<Vec<_>>().join(" ");
        if norm(&title).len() >= 3 {
            other.push(title);
        }
    }
    (keep, other)
}

// ------------------------------------------------------------------ looks

struct Palette {
    top: Color32,
    bottom: Color32,
    blob: [Color32; 3],
    ink: Color32,
    soft: Color32,
    accent: Color32,
    faint: Color32,
    pill: Color32,
}

impl Palette {
    fn new(dark: bool) -> Self {
        if dark {
            Palette {
                top: Color32::from_rgb(38, 26, 32),
                bottom: Color32::from_rgb(58, 34, 40),
                blob: [Color32::from_rgba_unmultiplied(240, 122, 90, 40), Color32::from_rgba_unmultiplied(196, 112, 168, 36), Color32::from_rgba_unmultiplied(250, 190, 110, 30)],
                ink: Color32::from_rgb(250, 236, 224),
                soft: Color32::from_rgb(214, 186, 176),
                accent: Color32::from_rgb(244, 132, 98),
                faint: Color32::from_rgba_unmultiplied(250, 236, 224, 50),
                pill: Color32::from_rgba_unmultiplied(255, 240, 230, 22),
            }
        } else {
            Palette {
                top: Color32::from_rgb(255, 246, 236),
                bottom: Color32::from_rgb(255, 222, 206),
                blob: [Color32::from_rgba_unmultiplied(255, 170, 140, 70), Color32::from_rgba_unmultiplied(246, 160, 196, 55), Color32::from_rgba_unmultiplied(255, 208, 130, 60)],
                ink: Color32::from_rgb(62, 40, 32),
                soft: Color32::from_rgb(140, 104, 92),
                accent: Color32::from_rgb(232, 104, 74),
                faint: Color32::from_rgba_unmultiplied(62, 40, 32, 40),
                pill: Color32::from_rgba_unmultiplied(255, 255, 255, 150),
            }
        }
    }
}

/// Warm gradient with slowly drifting soft blobs.
fn backdrop(p: &egui::Painter, r: Rect, pal: &Palette, t: f32) {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), pal.top);
    m.colored_vertex(r.right_top(), pal.top);
    m.colored_vertex(r.right_bottom(), pal.bottom);
    m.colored_vertex(r.left_bottom(), pal.bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(m);
    let spots = [(0.18, 0.25, 0.32, 0.0), (0.85, 0.35, 0.28, 2.0), (0.55, 0.9, 0.36, 4.0)];
    for (i, (x, y, size, phase)) in spots.iter().enumerate() {
        let c = pos2(r.left() + r.width() * (x + 0.03 * (t * 0.21 + phase).sin()), r.top() + r.height() * (y + 0.03 * (t * 0.17 + phase).cos()));
        soft_circle(p, c, r.width().max(r.height()) * size, pal.blob[i]);
    }
}

/// A circle that fades to transparent at its edge.
fn soft_circle(p: &egui::Painter, c: Pos2, radius: f32, color: Color32) {
    let mut m = Mesh::default();
    let n = 48;
    m.colored_vertex(c, color);
    for i in 0..=n {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        m.colored_vertex(c + radius * Vec2::angled(a), Color32::TRANSPARENT);
    }
    for i in 1..=n as u32 {
        m.add_triangle(0, i, i + 1);
    }
    p.add(m);
}

fn draw_orb(p: &egui::Painter, c: Pos2, color: Color32, pal: &Palette, t: f32, talking: f32, hearing: f32) {
    let breathe = 1.0 + 0.03 * (t * 1.6).sin();
    let r = 46.0 * breathe;
    soft_circle(p, c, r * (2.0 + hearing * 0.8), color.gamma_multiply(0.35 + hearing * 0.3));
    if talking > 0.0 {
        for k in 0..3 {
            let phase = ((t * 0.9 + k as f32 / 3.0) % 1.0) as f32;
            p.circle_stroke(c, r + 8.0 + phase * 46.0, Stroke::new(2.0_f32, pal.accent.gamma_multiply(0.55 * (1.0 - phase))));
        }
    }
    p.circle_filled(c, r, color);
    soft_circle(p, c - vec2(r * 0.3, r * 0.34), r * 0.75, Color32::from_rgba_unmultiplied(255, 255, 255, 60));
    p.text(c + vec2(0.0, 1.0), Align2::CENTER_CENTER, "A", FontId::new(r * 0.95, FontFamily::Name("serif".into())), Color32::WHITE);
}

/// Rounded choices; returns the one tapped.
fn pills(ui: &mut egui::Ui, pal: &Palette, items: &[(&str, bool)]) -> Option<usize> {
    let mut tapped = None;
    let font = FontId::proportional(14.5);
    // Lay out in centered rows.
    let max_w = ui.available_width().min(560.0);
    let sizes: Vec<f32> = items.iter().map(|(s, _)| ui.painter().layout_no_wrap(s.to_string(), font.clone(), pal.ink).size().x + 28.0).collect();
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut w = 0.0;
    for (i, s) in sizes.iter().enumerate() {
        if w + s > max_w && !rows.last().unwrap().is_empty() {
            rows.push(Vec::new());
            w = 0.0;
        }
        rows.last_mut().unwrap().push(i);
        w += s + 8.0;
    }
    for row in rows {
        let total: f32 = row.iter().map(|i| sizes[*i] + 8.0).sum::<f32>() - 8.0;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
        let mut x = rect.center().x - total / 2.0;
        for i in row {
            let r = Rect::from_min_size(pos2(x, rect.top() + 2.0), vec2(sizes[i], 32.0));
            let resp = ui.interact(r, Id::new(("pill", items[i].0)), Sense::click());
            let on = items[i].1;
            let fill = if on { pal.accent } else if resp.hovered() { pal.pill.gamma_multiply(1.4) } else { pal.pill };
            ui.painter().rect_filled(r, CornerRadius::same(16), fill);
            ui.painter().text(r.center(), Align2::CENTER_CENTER, items[i].0, font.clone(), if on { Color32::WHITE } else { pal.ink });
            if resp.clicked() {
                tapped = Some(i);
            }
            x += sizes[i] + 8.0;
        }
        ui.add_space(4.0);
    }
    tapped
}

fn big_button(ui: &mut egui::Ui, pal: &Palette, label: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(140.0, 44.0), Sense::click());
    let fill = if resp.hovered() { pal.accent.gamma_multiply(0.9) } else { pal.accent };
    ui.painter().rect_filled(r, CornerRadius::same(22), fill);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, label, FontId::proportional(16.0), Color32::WHITE);
    resp.clicked()
}

fn soft_button(ui: &mut egui::Ui, pal: &Palette, label: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(110.0, 44.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(22), pal.pill);
    }
    ui.painter().text(r.center(), Align2::CENTER_CENTER, label, FontId::proportional(15.0), pal.soft);
    resp.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn understands_names() {
        assert_eq!(parse_name("Yes, that's me.", "Sam Doe"), "Sam");
        assert_eq!(parse_name("My name is alex smith", ""), "Alex Smith");
        assert_eq!(parse_name("Just call me Kim.", "Sam Doe"), "Kim");
        assert_eq!(parse_name("Sam", "Sam Doe"), "Sam");
    }

    #[test]
    fn finds_spoken_projects() {
        let folders = vec!["sample-site".to_string(), "privacy-guard".into(), "clubkit".into(), "guard".into()];
        let (found, other) = match_projects("Mostly sample site and Privacy Guard, and there's Moonbase.", &folders);
        assert_eq!(found, vec!["privacy-guard", "sample-site"]);
        assert_eq!(other, vec!["Moonbase"]);
    }

    #[test]
    fn walks_through_the_questions() {
        let mut w = Welcome::new("Sam Doe".into(), vec!["en".into()], "/nonexistent");
        assert!(w.take_question_to_speak().unwrap().contains("what should I call you"));
        assert!(w.take_question_to_speak().is_none(), "asked once");
        w.hear("yes");
        w.next();
        assert!(w.question().starts_with("Nice to meet you, Sam!"));
        w.hear("I build apps for small shops");
        w.next();
        w.hear("Moonbase and the shop app");
        w.next();
        w.hear("Short please");
        w.next();
        w.hear("English and German");
        w.next();
        let note = w.note();
        assert!(note.contains("- Name: Sam Doe") || note.contains("- Name: Sam"), "{note}");
        assert!(note.contains("I build apps for small shops") && note.contains("Moonbase") && note.contains("English, German"), "{note}");
    }
}
