//! The window. Native-feeling on macOS: system font and colors, sidebar under the traffic lights,
//! Messages-style conversation, and a floating voice capsule.

use crate::heartbeat;
use crate::icons::{self, Icon};
use crate::model::{self, Agent, AgentKind, Author, Channel, Data, PALETTE};
use crate::orchestrator::{self, Shared};
use crate::probe::{self, Health};
use crate::theme::Theme;
use crate::voice::{self, Dictation, Mode};
use eframe::egui::{
    self, text::LayoutJob, vec2, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Key, Layout, Margin, Rect, RichText,
    ScrollArea, Sense, Shadow, Stroke, TextFormat, UiBuilder, Vec2,
};
use global_hotkey::hotkey::{Code, HotKey, Modifiers as HkMods};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Hold at least this long = push-to-talk (release sends). Shorter = tap to start, tap again to send.
const HOLD: Duration = Duration::from_millis(350);
/// Room for the traffic-light buttons (macOS draws them over our content).
const TITLEBAR: f32 = if cfg!(target_os = "macos") { 38.0 } else { 10.0 };
const COMPOSER: &str = "composer";

pub struct App {
    shared: Arc<Shared>,
    input: String,
    dictation: Option<Dictation>,
    /// When the current push-to-talk press started.
    pressed_at: Option<Instant>,
    mic_was_down: bool,
    voice_error: Option<String>,
    /// Images dropped onto the window, waiting to go with your next message.
    attachments: Vec<std::path::PathBuf>,
    /// Which speech engine the running dictation uses.
    voice_engine: String,
    /// Channel whose full history is shown (others show the latest messages only).
    show_all: Option<String>,
    /// Text-box mode: the part of the composer that live dictation owns.
    live: Option<LiveText>,
    /// Hands-free: the utterance in flight was already put in the text box and sent from there.
    skip_next_utterance: bool,
    show_team: bool,
    show_settings: bool,
    /// The theme setting we last applied ("auto" / "light" / "dark").
    applied_theme: String,
    team_sel: Option<String>,
    new_channel: Option<NewChannel>,
    renaming: Option<String>,
    /// Slide-out on the right: a file (TASKS.md, AGENTS.md) or the docs folder.
    panel: Option<Panel>,
    last_save: Instant,
    focus_input: bool,
    _hotkeys: Option<GlobalHotKeyManager>,
    hotkey_rx: mpsc::Receiver<GlobalHotKeyEvent>,
}

/// Live dictation into the text box. `base` is what was there before (or what you've edited it to),
/// `consumed` is the part of the transcript already in `base`, `written` is what we last put in the box.
#[derive(Default, Clone, Debug, PartialEq)]
struct LiveText {
    base: String,
    consumed: String,
    written: String,
    /// The transcript `written` was made from.
    shown: String,
}

impl LiveText {
    fn start(input: &str) -> Self {
        LiveText { base: input.to_string(), consumed: String::new(), written: input.to_string(), shown: String::new() }
    }

    /// The new text-box contents for `transcript`, given the box currently holds `input`.
    /// If you edited the box since we last wrote it, your edit wins and only newer words get added.
    fn update(&mut self, input: &str, transcript: &str) -> String {
        if input != self.written {
            // Everything we had shown is now part of your text; keep only what came after it.
            self.base = input.to_string();
            self.consumed = if self.shown.is_empty() { transcript.to_string() } else { self.shown.clone() };
        }
        let tail = if self.consumed.is_empty() {
            transcript
        } else {
            transcript.strip_prefix(self.consumed.as_str()).unwrap_or("")
        };
        let out = join_words(&self.base, tail.trim());
        self.written = out.clone();
        self.shown = transcript.to_string();
        out
    }

    /// The user edited the dictated words: from now on they're theirs.
    fn edited(&self) -> bool {
        !self.consumed.is_empty()
    }
}

fn join_words(base: &str, add: &str) -> String {
    match (base.trim_end().is_empty(), add.is_empty()) {
        (true, _) => add.to_string(),
        (_, true) => base.to_string(),
        _ => format!("{} {add}", base.trim_end()),
    }
}

struct NewChannel {
    name: String,
    folder: String,
    members: Vec<String>,
}

enum Action {
    /// Slide-out: show this file (or hide it if it's already showing).
    ShowFile(std::path::PathBuf),
    /// Slide-out: list this docs folder.
    ShowDocs(std::path::PathBuf),
    /// Ask the team in a channel to do something (from a header chip).
    AskTeam(String, String),
    Send(String, String),
    Stop(String),
    VoicePress,
    VoiceRelease,
    ToggleHandsFree,
    PickFolder(String),
    RefreshAgents,
    BeatNow,
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        let ctx = cc.egui_ctx.clone();
        crate::theme::install(&ctx);
        egui_extras::install_image_loaders(&ctx);
        let data = model::load();
        let repaint_ctx = ctx.clone();
        let shared = Shared::new(
            data,
            Box::new(move || repaint_ctx.request_repaint()),
            Box::new(|(t, v): (String, String)| voice::speak(&t, &v)),
            Box::new(crate::env::notify),
        );
        heartbeat::start(shared.clone());
        crate::inbox::start(shared.clone());
        voice::prepare();
        if shared.data.lock().unwrap().settings.stt_engine == "parakeet" {
            let c = ctx.clone();
            voice::setup_parakeet(move || c.request_repaint());
        }

        // Ctrl+Option+Space from any app: hold to talk to the current channel.
        let (tx, hotkey_rx) = mpsc::channel();
        let c = ctx.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            let _ = tx.send(e);
            c.request_repaint();
        }));
        let hotkeys = GlobalHotKeyManager::new().ok().and_then(|m| {
            m.register(HotKey::new(Some(HkMods::CONTROL | HkMods::ALT), Code::Space)).ok()?;
            Some(m)
        });

        App {
            shared,
            input: String::new(),
            dictation: None,
            pressed_at: None,
            mic_was_down: false,
            voice_error: None,
            attachments: Vec::new(),
            voice_engine: String::new(),
            show_all: None,
            live: None,
            skip_next_utterance: false,
            show_team: std::env::var_os("YH_SHOW_AGENTS").is_some(),
            show_settings: std::env::var_os("YH_SHOW_SETTINGS").is_some(),
            applied_theme: String::new(),
            team_sel: None,
            new_channel: None,
            renaming: None,
            panel: None,
            last_save: Instant::now(),
            focus_input: false,
            _hotkeys: hotkeys,
            hotkey_rx,
        }
    }

    fn save_if_needed(&mut self, force: bool) {
        if !self.shared.dirty.load(Ordering::Relaxed) || (!force && self.last_save.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.shared.dirty.store(false, Ordering::Relaxed);
        let snapshot = self.shared.data.lock().unwrap().clone();
        if let Err(e) = model::save(&snapshot) {
            eprintln!("save failed: {e}");
        }
        self.last_save = Instant::now();
    }

    /// Names the recognizer should expect.
    fn vocabulary(&self) -> Vec<String> {
        let data = self.shared.data.lock().unwrap();
        let mut w: Vec<String> = vec!["Claude".into(), "Codex".into(), "AGENTS.md".into(), "TASKS.md".into(), "docs".into(), "README".into()];
        w.extend(data.agents.iter().map(|a| a.handle.clone()));
        w.extend(data.agents.iter().filter(|a| !a.display_name.trim().is_empty()).map(|a| a.display_name.trim().to_string()));
        w.extend(data.channels.iter().map(|c| c.name.replace('-', " ")));
        w
    }

    fn start_voice(&mut self, mode: Mode, ctx: &egui::Context) {
        if std::env::var_os("YH_SCREENSHOT").is_some() {
            return; // dev screenshots: don't open the microphone
        }
        self.voice_error = None;
        let c = ctx.clone();
        let engine = self.engine();
        match Dictation::start(mode, &self.vocabulary(), &engine, move || c.request_repaint()) {
            Ok(d) => {
                self.dictation = Some(d);
                self.voice_engine = engine;
            }
            Err(e) => self.voice_error = Some(e),
        }
    }

    /// The engine to use right now: Parakeet once it's set up, otherwise Apple's.
    fn engine(&self) -> String {
        let wanted = self.shared.data.lock().unwrap().settings.stt_engine.clone();
        if wanted == "parakeet" && voice::parakeet_ready() {
            "parakeet".into()
        } else {
            "apple".into()
        }
    }

    fn hands_free(&self) -> bool {
        self.shared.data.lock().unwrap().settings.hands_free
    }

    /// Mic / Space / Ctrl+Option+Space went down.
    fn voice_press(&mut self, ctx: &egui::Context) {
        voice::stop_speaking(); // talking over the agent interrupts it
        if self.hands_free() {
            return; // already listening
        }
        match &mut self.dictation {
            Some(d) if !d.stopping() => d.stop(), // second tap: done talking
            Some(_) => {}
            None => {
                self.pressed_at = Some(Instant::now());
                self.start_voice(Mode::PushToTalk, ctx);
            }
        }
    }

    /// ...and came back up. Held = push-to-talk, so we're done; a quick tap keeps listening.
    fn voice_release(&mut self) {
        let held = self.pressed_at.take().is_some_and(|t| t.elapsed() >= HOLD);
        if let Some(d) = &mut self.dictation {
            if held && d.mode == Mode::PushToTalk && !d.stopping() {
                d.stop();
            }
        }
    }

    fn toggle_hands_free(&mut self, ctx: &egui::Context) {
        let on = {
            let mut data = self.shared.data.lock().unwrap();
            data.settings.hands_free = !data.settings.hands_free;
            data.settings.hands_free
        };
        self.shared.dirty.store(true, Ordering::Relaxed);
        self.dictation = None;
        self.voice_error = None;
        self.live = None;
        if on {
            voice::stop_speaking();
            self.start_voice(Mode::HandsFree, ctx);
        }
    }

    /// You sent the text box: whatever dictation already put there must not be sent again.
    fn composer_sent(&mut self) {
        if let Some(d) = &self.dictation {
            if d.mode == Mode::PushToTalk {
                self.dictation = None; // its words are in what you just sent
            } else if !d.state.lock().unwrap().text.is_empty() {
                self.skip_next_utterance = true;
            }
        }
        self.live = None;
    }

    /// Returns texts to send.
    fn poll_voice(&mut self, ctx: &egui::Context) -> Vec<String> {
        let (hands_free, keyboard) = {
            let d = self.shared.data.lock().unwrap();
            (d.settings.hands_free, d.settings.show_keyboard)
        };
        // Engine changed (you switched it, or Parakeet just finished setting up): restart listening.
        if let Some(d) = &self.dictation {
            if d.mode == Mode::HandsFree && self.voice_engine != self.engine() {
                self.dictation = None;
            }
        }
        // Always-listening is a setting: keep it running (unless it just failed).
        if hands_free && self.dictation.is_none() && self.voice_error.is_none() {
            self.start_voice(Mode::HandsFree, ctx);
        }
        let Some(d) = &self.dictation else {
            self.live = None;
            return vec![];
        };
        let mode = d.mode;
        let mut st = d.state.lock().unwrap();
        let mut out = Vec::new();

        // Hands-free: each pause delivers an utterance.
        for u in st.utterances.drain(..) {
            let u = u.trim().to_string();
            if std::mem::take(&mut self.skip_next_utterance) || u.is_empty() || voice::is_echo(&u) {
                // Already sent from the text box, or our own read-aloud voice.
                if keyboard {
                    self.live = None;
                }
                continue;
            }
            match self.live.take() {
                // You edited what you were saying in the text box: it waits there for Enter.
                Some(live) if keyboard && live.edited() => {
                    self.input = LiveText { written: self.input.clone(), ..live }.update(&self.input, &u);
                }
                // Otherwise it's sent, and the text box goes back to what you had typed.
                Some(live) if keyboard => {
                    self.input = live.base;
                    out.push(u);
                }
                _ => out.push(u),
            }
        }

        // Text-box mode: show what you're saying in the box as you say it.
        if keyboard && !st.text.is_empty() {
            let live = self.live.get_or_insert_with(|| LiveText::start(&self.input));
            self.input = live.update(&self.input, st.text.trim());
        }
        if !st.done {
            return out;
        }

        // Finished (push-to-talk released, or the helper stopped).
        let (text, error) = (st.text.trim().to_string(), st.error.clone());
        drop(st);
        self.dictation = None;
        if let Some(e) = error {
            // Show it even if some words came through, so failures aren't hidden.
            self.voice_error = Some(e);
            if mode == Mode::HandsFree {
                self.shared.data.lock().unwrap().settings.hands_free = false;
            }
        }
        if mode == Mode::PushToTalk && !text.is_empty() {
            if keyboard {
                // Final transcript replaces the live one (no duplicates); you edit and press Enter.
                if let Some(mut live) = self.live.take() {
                    self.input = live.update(&self.input, &text);
                }
                self.focus_input = true;
            } else if self.voice_error.is_some() {
                // Don't lose a half-recognized message: put it in the text box instead of sending it.
                self.input = join_words(&self.input, &text);
                self.shared.data.lock().unwrap().settings.show_keyboard = true;
                self.focus_input = true;
            } else {
                out.push(text);
            }
        }
        self.live = None;
        out
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        debug_screenshot(ctx);
        let t = Theme::of(ctx);
        let mut actions: Vec<Action> = Vec::new();
        self.shared.app_focused.store(ctx.input(|i| i.focused), Ordering::Relaxed);

        // Global hotkey (works when the app is in the background too).
        while let Ok(e) = self.hotkey_rx.try_recv() {
            actions.push(if e.state == HotKeyState::Pressed { Action::VoicePress } else { Action::VoiceRelease });
        }
        // Space = talk, when you're not typing in a field.
        if ctx.memory(|m| m.focused().is_none()) && !self.show_team && !self.show_settings && self.new_channel.is_none() {
            ctx.input(|i| {
                for e in &i.events {
                    if let egui::Event::Key { key: Key::Space, pressed, repeat: false, modifiers, .. } = e {
                        if modifiers.is_none() {
                            actions.push(if *pressed { Action::VoicePress } else { Action::VoiceRelease });
                        }
                    }
                }
            });
        }

        if self.panel.is_some() && ctx.memory(|m| m.focused().is_none()) && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.panel = None;
        }
        // Images dragged onto the window: attach to the next message.
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for p in dropped {
            if is_image(&p) {
                if !self.attachments.contains(&p) {
                    self.attachments.push(p);
                }
            } else {
                self.voice_error = Some(format!("Only images can be attached for now ({}).", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
            }
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let t = Theme::of(ctx);
            let screen = ctx.screen_rect();
            let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("drop-overlay")));
            p.rect_filled(screen, 0.0, Color32::from_black_alpha(if t.dark { 120 } else { 60 }));
            let card = Rect::from_center_size(screen.center(), vec2(280.0, 110.0));
            p.rect(card, 16.0, t.bg, Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
            p.text(card.center() - vec2(0.0, 12.0), Align2::CENTER_CENTER, "Drop to attach", FontId::proportional(17.0), t.strong);
            p.text(card.center() + vec2(0.0, 16.0), Align2::CENTER_CENTER, "Goes with your next message", FontId::proportional(12.5), t.secondary);
        }
        let to_send = self.poll_voice(ctx);
        let voice_state = self.dictation.as_ref().map(|d| (d.mode, d.stopping(), d.state.lock().unwrap().clone()));
        let shared = self.shared.clone();
        let mut data = shared.data.lock().unwrap();

        if data.selected.as_ref().and_then(|id| data.channel(id)).is_none() {
            data.selected = data.channels.first().map(|c| c.id.clone());
        }
        if let Some(cid) = data.selected.clone() {
            for text in to_send {
                match voice_command(&text, &data) {
                    Some(VoiceCmd::Ignore) => {}
                    Some(VoiceCmd::Stop) => {
                        voice::stop_speaking();
                        actions.push(Action::Stop(cid.clone()));
                    }
                    Some(VoiceCmd::Quiet) => voice::stop_speaking(),
                    Some(VoiceCmd::StopListening) => actions.push(Action::ToggleHandsFree),
                    Some(VoiceCmd::Switch(id)) => {
                        let name = data.channel(&id).map(|c| c.name.clone()).unwrap_or_default();
                        data.selected = Some(id);
                        shared.dirty.store(true, Ordering::Relaxed);
                        voice::speak(&format!("Switched to {}.", name.replace('-', " ")), "");
                    }
                    None => actions.push(Action::Send(cid.clone(), text)),
                }
            }
        }
        let selected = data.selected.clone();
        if let Some(cid) = &selected {
            if let Some(ch) = data.channel_mut(cid) {
                ch.unread = false;
            }
        }
        voice::set_speed(data.settings.speech_speed);
        if self.applied_theme != data.settings.theme && std::env::var_os("YH_THEME").is_none() {
            self.applied_theme = data.settings.theme.clone();
            ctx.set_theme(match self.applied_theme.as_str() {
                "light" => egui::ThemePreference::Light,
                "dark" => egui::ThemePreference::Dark,
                _ => egui::ThemePreference::System,
            });
        }
        // What's in this channel's folder (computed once here; kept fresh by the heartbeat).
        if let Some(cid) = &selected {
            let missing = !shared.project.lock().unwrap().contains_key(cid);
            if missing {
                if let Some(ch) = data.channel(cid) {
                    let info = crate::project::inspect(std::path::Path::new(&ch.folder));
                    shared.project.lock().unwrap().insert(cid.clone(), info);
                }
            }
        }
        let project = selected.as_ref().and_then(|c| shared.project.lock().unwrap().get(c).cloned()).unwrap_or_default();
        // Dev aid for screenshots: YH_SHOW_PANEL=tasks|docs opens the slide-out once.
        if let Ok(which) = std::env::var("YH_SHOW_PANEL") {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                self.panel = match which.as_str() {
                    "docs" => project.docs_path.clone().map(Panel::docs),
                    _ => project.tasks_path.clone().map(FileView::open).map(Panel::File),
                };
            });
        }
        let health: HashMap<String, Health> = shared.health.lock().unwrap().clone();
        let last_beat = *shared.last_beat.lock().unwrap();
        let runs: HashMap<String, (Vec<(String, String)>, u64, usize)> = shared
            .runs
            .lock()
            .unwrap()
            .iter()
            .map(|(k, r)| (k.clone(), (r.working.clone(), r.started.elapsed().as_secs(), r.queue.len())))
            .collect();

        // ------------------------------------------------ sidebar
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(236.0)
            .width_range(200.0..=320.0)
            .frame(Frame::new().fill(t.sidebar).inner_margin(Margin { left: 10, right: 10, top: 0, bottom: 10 }))
            .show(ctx, |ui| {
                let top = Rect::from_min_size(ui.max_rect().min, vec2(ui.available_width(), TITLEBAR));
                window_drag(ui, top, "sidebar-drag");
                ui.add_space(TITLEBAR);

                ui.horizontal(|ui| {
                    section_label(ui, &t, "Channels");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, &t, Icon::Plus, 22.0, false, "New channel").clicked() {
                            self.new_channel = Some(NewChannel {
                                name: String::new(),
                                folder: model::home_dir(),
                                members: data.agents.iter().map(|a| a.id.clone()).collect(),
                            });
                        }
                    });
                });
                let team_h = 70.0 + data.agents.len() as f32 * 40.0;
                ScrollArea::vertical()
                    .id_salt("channels")
                    .auto_shrink([false, true])
                    .max_height((ui.available_height() - team_h).max(80.0))
                    .show(ui, |ui| {
                        let mut pick = None;
                        for ch in &data.channels {
                            let sel = selected.as_deref() == Some(ch.id.as_str());
                            let busy = runs.contains_key(&ch.id);
                            let badge = if busy {
                                Some(t.accent)
                            } else if ch.unread && !sel {
                                Some(t.secondary)
                            } else {
                                None
                            };
                            if channel_row(ui, &t, sel, &ch.name, badge, busy).clicked() {
                                pick = Some(ch.id.clone());
                            }
                        }
                        if let Some(id) = pick {
                            data.selected = Some(id);
                            shared.dirty.store(true, Ordering::Relaxed);
                            self.renaming = None;
                        }
                    });

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.horizontal(|ui| {
                        if icon_button(ui, &t, Icon::Gear, 26.0, false, "Settings").clicked() {
                            self.show_settings = true;
                            std::thread::spawn(voice::refresh_voices);
                        }
                        if icon_button(ui, &t, Icon::People, 26.0, false, "Team").clicked() {
                            self.show_team = true;
                        }
                        if icon_button(ui, &t, Icon::Refresh, 26.0, false, "Check logins, models and limits").clicked() {
                            actions.push(Action::RefreshAgents);
                        }
                    });
                    ui.add_space(2.0);
                    for a in data.agents.iter().rev() {
                        let (line, col, tip) = role_status(a, &health, &t);
                        if agent_row(ui, &t, a, &line, col).on_hover_text(tip).clicked() {
                            self.team_sel = Some(a.id.clone());
                            self.show_team = true;
                        }
                    }
                    ui.add_space(4.0);
                    section_label(ui, &t, "Team");
                });
            });

        let Some(cid) = selected.clone() else {
            egui::CentralPanel::default().frame(Frame::new().fill(t.bg)).show(ctx, |ui| {
                ui.centered_and_justified(|ui| ui.label(RichText::new("Create a channel with + to get started.").color(t.secondary)));
            });
            drop(data);
            self.sheets(ctx, &t);
            return;
        };
        let run = runs.get(&cid).cloned();
        let hands_free = data.settings.hands_free;

        // ------------------------------------------------ slide-out (TASKS.md, AGENTS.md, docs)
        let mut close_panel = false;
        let shown = self.panel.is_some();
        egui::SidePanel::right("viewer")
            .resizable(true)
            .default_width(440.0)
            .width_range(320.0..=760.0)
            .frame(Frame::new().fill(t.sidebar).inner_margin(Margin { left: 18, right: 14, top: 0, bottom: 12 }))
            .show_animated(ctx, shown, |ui| {
                let top = Rect::from_min_size(ui.max_rect().min, vec2(ui.available_width(), TITLEBAR));
                window_drag(ui, top, "viewer-drag");
                ui.add_space(TITLEBAR - 8.0);
                if let Some(panel) = &mut self.panel {
                    if panel_ui(ui, &t, panel) {
                        close_panel = true;
                    }
                }
            });
        if close_panel {
            self.panel = None;
        }

        // ------------------------------------------------ voice capsule
        egui::TopBottomPanel::bottom("dock")
            .show_separator_line(false)
            .frame(Frame::new().fill(t.bg).inner_margin(Margin { left: 16, right: 16, top: 4, bottom: 10 }))
            .show(ctx, |ui| {
                if !self.attachments.is_empty() {
                    let mut remove = None;
                    ui.horizontal(|ui| {
                        let w = self.attachments.len() as f32 * 64.0 + 120.0;
                        ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                        for (i, p) in self.attachments.iter().enumerate() {
                            let img = egui::Image::new(format!("file://{}", p.display()))
                                .fit_to_exact_size(vec2(52.0, 52.0))
                                .maintain_aspect_ratio(true)
                                .corner_radius(8);
                            let r = ui.add(img.sense(Sense::click())).on_hover_text(format!("{} (click to remove)", p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
                            if r.clicked() {
                                remove = Some(i);
                            }
                        }
                        let n = self.attachments.len();
                        ui.label(RichText::new(if n == 1 { "1 image attached".to_string() } else { format!("{n} images attached") }).size(11.5).color(t.secondary));
                    });
                    if let Some(i) = remove {
                        self.attachments.remove(i);
                    }
                    ui.add_space(6.0);
                }
                if let Some(e) = self.voice_error.clone() {
                    ui.vertical_centered(|ui| {
                        if ui.add(egui::Label::new(RichText::new(format!("{e}   ✕")).size(12.0).color(t.red)).sense(Sense::click())).clicked() {
                            self.voice_error = None;
                        }
                    });
                    ui.add_space(4.0);
                }
                let listening = voice_state.is_some();
                let width = ui.available_width().min(640.0);
                let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::hover());
                let capsule = Rect::from_center_size(row.center(), vec2(width, 56.0));
                let p = ui.painter();
                p.add(Shadow { offset: [0, 4], blur: 18, spread: 0, color: Color32::from_black_alpha(if t.dark { 70 } else { 22 }) }.as_shape(capsule, 28));
                p.rect(capsule, 28.0, t.card, Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);

                let mut c = ui.new_child(UiBuilder::new().max_rect(capsule.shrink2(vec2(8.0, 8.0))).layout(Layout::left_to_right(Align::Center)));
                // Mic orb
                let level = voice_state.as_ref().map(|(_, _, s)| s.level).unwrap_or(0.0);
                let orb = mic_orb(&mut c, &t, listening, level, hands_free);
                let down = orb.is_pointer_button_down_on();
                if hands_free {
                    if orb.clicked() {
                        actions.push(Action::ToggleHandsFree);
                    }
                } else if down && !self.mic_was_down {
                    actions.push(Action::VoicePress);
                } else if !down && self.mic_was_down {
                    actions.push(Action::VoiceRelease);
                }
                self.mic_was_down = down && !hands_free;
                c.add_space(10.0);

                let keyboard = data.settings.show_keyboard;
                let n_icons = 2 + run.is_some() as usize + (keyboard && (!self.input.trim().is_empty() || !self.attachments.is_empty())) as usize;
                let middle_w = (c.available_width() - n_icons as f32 * 34.0 - 6.0).max(60.0);
                let channel_name = data.channel(&cid).map(|c| c.name.clone()).unwrap_or_default();
                c.allocate_ui_with_layout(vec2(middle_w, 40.0), Layout::left_to_right(Align::Center), |ui| {
                    if keyboard {
                        let id = Id::new(COMPOSER);
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut self.input)
                                .id(id)
                                .frame(false)
                                .font(FontId::proportional(14.0))
                                .hint_text(RichText::new(format!("Message #{channel_name}")).color(t.tertiary))
                                .desired_width(middle_w),
                        );
                        if self.focus_input {
                            edit.request_focus();
                            self.focus_input = false;
                        }
                        if edit.has_focus() && ctx.input(|i| i.key_pressed(Key::Escape)) {
                            edit.surrender_focus();
                        }
                        if edit.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) && (!self.input.trim().is_empty() || !self.attachments.is_empty()) {
                            actions.push(Action::Send(cid.clone(), self.input.trim().to_string()));
                            self.input.clear();
                            self.focus_input = true;
                            self.composer_sent();
                        }
                    } else {
                        let (text, color, size) = capsule_text(&t, &voice_state, hands_free, &run);
                        ui.add(egui::Label::new(RichText::new(text).size(size).color(color)).truncate());
                    }
                });
                if keyboard && (!self.input.trim().is_empty() || !self.attachments.is_empty()) && icon_button(&mut c, &t, Icon::Send, 30.0, true, "Send (Enter)").clicked() {
                    actions.push(Action::Send(cid.clone(), self.input.trim().to_string()));
                    self.input.clear();
                    self.focus_input = true;
                    self.composer_sent();
                }
                if run.is_some() && icon_button(&mut c, &t, Icon::Stop, 30.0, false, "Stop the agents (or say \"stop\")").clicked() {
                    actions.push(Action::Stop(cid.clone()));
                }
                let ra = data.settings.read_aloud;
                let speaking = voice::speaking();
                let tip = if speaking { "Speaking. Click to be quiet." } else if ra { "Replies are read aloud" } else { "Replies are silent" };
                if icon_button(&mut c, &t, if ra { Icon::Speaker } else { Icon::SpeakerOff }, 30.0, ra, tip).clicked() {
                    if speaking {
                        voice::stop_speaking();
                    } else {
                        data.settings.read_aloud = !ra;
                        shared.dirty.store(true, Ordering::Relaxed);
                    }
                }
                if icon_button(&mut c, &t, Icon::Keyboard, 30.0, keyboard, "Type instead").clicked() {
                    data.settings.show_keyboard = !keyboard;
                    self.focus_input = !keyboard;
                    self.live = None;
                    shared.dirty.store(true, Ordering::Relaxed);
                }

                // Mode switch, quietly under the capsule.
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let read_aloud = data.settings.read_aloud;
                    ui.add_space((ui.available_width() - if read_aloud { 316.0 } else { 236.0 }).max(0.0) / 2.0);
                    for (on, label, tip) in [
                        (hands_free, "Always listening", "Like an office: just talk, every pause sends"),
                        (!hands_free, "Push to talk", "Hold Space, Ctrl+Option+Space or the mic"),
                    ] {
                        let (r, _) = ui.allocate_exact_size(vec2(8.0, 12.0), Sense::hover());
                        if on {
                            ui.painter().circle_filled(r.center(), 3.0, t.accent);
                        } else {
                            ui.painter().circle_stroke(r.center(), 2.8, Stroke::new(1.0, t.tertiary));
                        }
                        let rt = RichText::new(label).size(11.5).color(if on { t.secondary } else { t.tertiary });
                        if ui.add(egui::Label::new(rt).sense(Sense::click())).on_hover_text(tip).clicked() && !on {
                            actions.push(Action::ToggleHandsFree);
                        }
                        ui.add_space(10.0);
                    }
                    // How fast replies are read aloud: click to step through 1× … 2×.
                    if read_aloud {
                        let speed = data.settings.speech_speed;
                        let label = format!("Speed {}×", format!("{speed:.2}").trim_end_matches('0').trim_end_matches('.'));
                        let r = ui.add(egui::Label::new(RichText::new(label).size(11.5).color(t.secondary)).sense(Sense::click()));
                        if r.on_hover_text("How fast replies are read aloud. Click to change.").clicked() {
                            let steps = model::SPEECH_SPEEDS;
                            let i = steps.iter().position(|s| (s - speed).abs() < 0.01).map(|i| (i + 1) % steps.len()).unwrap_or(0);
                            data.settings.speech_speed = steps[i];
                            shared.dirty.store(true, Ordering::Relaxed);
                            voice::set_speed(steps[i]);
                            voice::stop_speaking();
                            voice::speak("This is how fast I'll talk.", "");
                        }
                    }
                });
            });

        // ------------------------------------------------ header
        egui::TopBottomPanel::top("header")
            .exact_height(62.0)
            .frame(Frame::new().fill(t.bg).inner_margin(Margin::symmetric(20, 0)))
            .show(ctx, |ui| {
                window_drag(ui, ui.max_rect(), "header-drag");
                let Data { agents, channels, selected, .. } = &mut *data;
                let Some(idx) = channels.iter().position(|c| c.id == cid) else { return };
                let mut delete = false;
                ui.horizontal_centered(|ui| {
                    let ch = &mut channels[idx];
                    ui.vertical(|ui| {
                    ui.add_space(9.0);
                    ui.horizontal(|ui| {
                    if let Some(name) = &mut self.renaming {
                        let r = ui.add(egui::TextEdit::singleline(name).font(FontId::proportional(15.0)).desired_width(200.0));
                        r.request_focus();
                        if r.lost_focus() {
                            let n = slug(name);
                            if !n.is_empty() {
                                ch.name = n;
                                shared.dirty.store(true, Ordering::Relaxed);
                            }
                            self.renaming = None;
                        }
                    } else {
                        ui.label(RichText::new(&ch.name).size(15.5).color(t.strong));
                    }
                    let folder = ui
                        .add(egui::Label::new(RichText::new(short_path(&ch.folder)).size(12.0).color(t.tertiary)).sense(Sense::click()))
                        .on_hover_text("Project folder the agents work in. Click to change.");
                    if folder.clicked() {
                        actions.push(Action::PickFolder(cid.clone()));
                    }
                    });
                    ui.add_space(1.0);
                    project_chips(ui, &t, &project, &ch.folder, &cid, &mut actions);
                    });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // ••• menu
                        let more = icon_button(ui, &t, Icon::Ellipsis, 28.0, false, "More");
                        let more_id = Id::new("more-menu");
                        if more.clicked() {
                            ui.memory_mut(|m| m.toggle_popup(more_id));
                        }
                        egui::popup::popup_below_widget(ui, more_id, &more, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                            ui.set_min_width(190.0);
                            if menu_item(ui, "Rename Channel") {
                                self.renaming = Some(ch.name.clone());
                            }
                            if menu_item(ui, "Change Folder…") {
                                actions.push(Action::PickFolder(cid.clone()));
                            }
                            if menu_item(ui, "Check Files Now") {
                                actions.push(Action::BeatNow);
                            }
                            ui.separator();
                            if menu_item(ui, "Reset Agent Memory") {
                                ch.sessions.clear();
                                ch.seen.clear();
                                ch.messages.push(sys("Agent memory reset. Agents will start fresh sessions."));
                                shared.dirty.store(true, Ordering::Relaxed);
                            }
                            if menu_item(ui, "Clear Conversation") {
                                ch.messages.clear();
                                ch.sessions.clear();
                                ch.seen.clear();
                                shared.dirty.store(true, Ordering::Relaxed);
                            }
                            ui.separator();
                            if ui.add(egui::Button::new(RichText::new("Delete Channel").color(t.red)).frame(false)).clicked() {
                                delete = true;
                            }
                        });
                        ui.add_space(4.0);

                        // Team avatars (click: who's in this channel)
                        let team = team_stack(ui, &t, agents, &ch.members, &health);
                        let team_id = Id::new("team-menu");
                        if team.clicked() {
                            ui.memory_mut(|m| m.toggle_popup(team_id));
                        }
                        egui::popup::popup_below_widget(ui, team_id, &team, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                            ui.set_min_width(270.0);
                            ui.label(RichText::new("You talk to the main agent; it brings in the others.").size(11.5).color(t.secondary));
                            ui.add_space(4.0);
                            for a in agents.iter() {
                                ui.horizontal(|ui| {
                                    let mut on = ch.members.contains(&a.id);
                                    if toggle(ui, &t, &mut on).changed() {
                                        if on {
                                            ch.members.push(a.id.clone());
                                        } else {
                                            ch.members.retain(|m| m != &a.id);
                                        }
                                        shared.dirty.store(true, Ordering::Relaxed);
                                    }
                                    ui.label(RichText::new(a.name()).color(color(a.color)));
                                    let is_main = ch.members.first() == Some(&a.id);
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if is_main {
                                            ui.label(RichText::new("Main").size(11.0).color(t.accent));
                                        } else if on && ui.add(egui::Button::new(RichText::new("Make main").size(11.0)).frame(false)).clicked() {
                                            ch.members.retain(|m| m != &a.id);
                                            ch.members.insert(0, a.id.clone());
                                            shared.dirty.store(true, Ordering::Relaxed);
                                        }
                                    });
                                });
                            }
                        });
                        ui.add_space(6.0);

                        // Heartbeat
                        let secs = last_beat.map(|b| b.elapsed().as_secs());
                        let pulse = (ui.input(|i| i.time) as f32 * 2.0).sin() * 0.5 + 0.5;
                        let (r, resp) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::click());
                        ui.painter().circle_filled(r.center(), 3.0 + pulse * 1.2, t.teal.gamma_multiply(0.5 + 0.5 * pulse));
                        let tip = match secs {
                            Some(s) => format!("Heartbeat: checked AGENTS.md, TASKS.md and docs/ {s}s ago. Click to check now."),
                            None => "Heartbeat starting…".into(),
                        };
                        if resp.on_hover_text(tip).clicked() {
                            actions.push(Action::BeatNow);
                        }
                    });
                });
                if delete {
                    channels.remove(idx);
                    *selected = channels.first().map(|c| c.id.clone());
                    shared.dirty.store(true, Ordering::Relaxed);
                    actions.push(Action::Stop(cid.clone()));
                }
            });

        // ------------------------------------------------ conversation
        egui::CentralPanel::default().frame(Frame::new().fill(t.bg)).show(ctx, |ui| {
            let Some(ch) = data.channel(&cid) else { return };
            ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                let full = ui.available_width();
                let w = (full - 48.0).min(740.0);
                ui.horizontal(|ui| {
                    ui.add_space((full - w) / 2.0);
                    ui.vertical(|ui| {
                        ui.set_width(w);
                        ui.add_space(12.0);
                        if ch.messages.is_empty() {
                            ui.add_space(80.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new("Just start talking.").size(20.0).color(t.strong));
                                ui.label(RichText::new("The main agent listens and brings in the team.").color(t.secondary));
                            });
                        }
                        // Long conversations stay fast: only the latest messages are laid out.
                        const RECENT: usize = 150;
                        let start = if self.show_all.as_deref() == Some(cid.as_str()) { 0 } else { ch.messages.len().saturating_sub(RECENT) };
                        if start > 0 {
                            ui.vertical_centered(|ui| {
                                let r = ui.add(egui::Label::new(RichText::new(format!("Show {start} earlier messages")).size(12.0).color(t.accent)).sense(Sense::click()));
                                if r.clicked() {
                                    self.show_all = Some(cid.clone());
                                }
                            });
                        }
                        let mut prev: Option<&Author> = None;
                        for m in &ch.messages[start..] {
                            message(ui, &t, &data, m, prev);
                            prev = Some(&m.author);
                        }
                        if let Some((list, secs, queued)) = &run {
                            working(ui, &t, &data, list, *secs, *queued);
                        }
                        ui.add_space(16.0);
                    });
                });
            });
        });

        drop(data);
        self.sheets(ctx, &t);

        for a in actions {
            match a {
                Action::Send(cid, text) => {
                    // Dropped images travel with this message (typed or spoken).
                    let files = std::mem::take(&mut self.attachments);
                    let folder = self.shared.data.lock().unwrap().channel(&cid).map(|c| c.folder.clone()).unwrap_or_default();
                    let stored = store_attachments(&folder, &files);
                    let text = if text.trim().is_empty() { "(see the attached image)".to_string() } else { text };
                    orchestrator::send_with(&self.shared, &cid, text, stored);
                }
                Action::AskTeam(cid, text) => orchestrator::send(&self.shared, &cid, text),
                Action::ShowFile(path) => {
                    let same = matches!(&self.panel, Some(Panel::File(f)) if f.path == path);
                    self.panel = if same { None } else { Some(Panel::File(FileView::open(path))) };
                }
                Action::ShowDocs(dir) => {
                    let same = matches!(&self.panel, Some(Panel::Docs { dir: d, .. }) if *d == dir);
                    self.panel = if same { None } else { Some(Panel::docs(dir)) };
                }
                Action::Stop(cid) => orchestrator::stop(&self.shared, &cid),
                Action::VoicePress => self.voice_press(ctx),
                Action::VoiceRelease => self.voice_release(),
                Action::ToggleHandsFree => self.toggle_hands_free(ctx),
                Action::RefreshAgents => {
                    let s = self.shared.clone();
                    std::thread::spawn(move || heartbeat::refresh_health(&s));
                }
                Action::BeatNow => {
                    let _ = self.shared.wake.0.lock().unwrap().send(());
                }
                Action::PickFolder(cid) => {
                    let start = self.shared.data.lock().unwrap().channel(&cid).map(|c| c.folder.clone()).unwrap_or_default();
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&start).pick_folder() {
                        let mut data = self.shared.data.lock().unwrap();
                        if let Some(ch) = data.channel_mut(&cid) {
                            ch.folder = dir.to_string_lossy().into_owned();
                            // CLI sessions are tied to a folder; start fresh ones.
                            ch.sessions.clear();
                            ch.seen.clear();
                            ch.watch.clear();
                            self.shared.project.lock().unwrap().remove(&cid);
                            let msg = format!("Project folder set to {}.", pretty_path(&ch.folder));
                            ch.messages.push(sys(&msg));
                        }
                        self.shared.dirty.store(true, Ordering::Relaxed);
                    }
                }
            }
        }

        // Keep the clock, the mic animation and the dots moving.
        let anim = if self.dictation.is_some() || !runs.is_empty() { 50 } else if voice::speaking() { 200 } else { 1000 };
        let anim = if self.panel.is_some() { anim.min(1000) } else { anim };
        ctx.request_repaint_after(Duration::from_millis(anim));
        if ctx.input(|i| i.viewport().close_requested()) {
            self.shutdown();
        } else {
            self.save_if_needed(false);
        }
    }
}

impl App {
    /// Stop all agents and speech. Called on window close and on app exit.
    fn shutdown(&mut self) {
        let ids: Vec<String> = self.shared.runs.lock().unwrap().keys().cloned().collect();
        for id in ids {
            orchestrator::stop(&self.shared, &id);
        }
        voice::stop_speaking();
        self.dictation = None;
        self.save_if_needed(true);
    }
}

fn capsule_text(t: &Theme, voice_state: &Option<(Mode, bool, voice::State)>, hands_free: bool, run: &Option<(Vec<(String, String)>, u64, usize)>) -> (String, Color32, f32) {
    match voice_state {
        Some((_, _, st)) if !st.text.is_empty() => (st.text.clone(), t.strong, 15.0),
        Some((_, true, _)) => ("Sending…".into(), t.secondary, 14.0),
        Some((_, _, st)) if st.info.is_some() => (st.info.clone().unwrap_or_default(), t.secondary, 14.0),
        _ if voice::speaking() => ("Speaking… (not listening meanwhile) · press Space to interrupt".into(), t.secondary, 14.0),
        Some((Mode::HandsFree, _, _)) if run.is_some() => ("Listening. The team is working on it.".into(), t.secondary, 14.0),
        Some((Mode::HandsFree, _, _)) => ("Listening. Just talk.".into(), t.secondary, 14.0),
        Some(_) => ("Listening…".into(), t.secondary, 14.0),
        None if hands_free => ("Starting to listen…".into(), t.tertiary, 14.0),
        None => ("Hold Space to talk".into(), t.tertiary, 14.0),
    }
}

// ------------------------------------------------------------------ widgets

/// Lets the window be dragged (and zoomed with a double-click) by an empty area, like a title bar.
fn window_drag(ui: &mut egui::Ui, rect: Rect, id: &str) {
    let r = ui.interact(rect, Id::new(id), Sense::click_and_drag());
    if r.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if r.double_clicked() {
        let max = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
    }
}

fn section_label(ui: &mut egui::Ui, t: &Theme, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).size(11.0).color(t.tertiary));
}

fn icon_button(ui: &mut egui::Ui, t: &Theme, icon: Icon, size: f32, active: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let p = ui.painter();
    if resp.hovered() && icon != Icon::Send {
        p.circle_filled(rect.center(), size / 2.0, t.hover.gamma_multiply(1.6));
    }
    let col = match icon {
        Icon::Send => t.accent,
        Icon::Stop => t.red,
        _ if active => t.accent,
        _ if resp.hovered() => t.text,
        _ => t.secondary,
    };
    let inner = if icon == Icon::Send { size * 0.8 } else { (size * 0.56).min(17.0) };
    icons::paint(p, icon, Rect::from_center_size(rect.center(), Vec2::splat(inner)), col);
    resp.on_hover_text(tip)
}

fn channel_row(ui: &mut egui::Ui, t: &Theme, selected: bool, name: &str, badge: Option<Color32>, busy: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 6.0, t.selected_row);
    } else if resp.hovered() {
        p.rect_filled(rect, 6.0, t.hover);
    }
    p.text(rect.left_center() + vec2(10.0, 0.0), Align2::LEFT_CENTER, "#", FontId::proportional(14.0), t.tertiary);
    p.text(rect.left_center() + vec2(24.0, 0.0), Align2::LEFT_CENTER, name, FontId::proportional(13.5), if selected { t.strong } else { t.text });
    if let Some(c) = badge {
        let r = if busy { 3.0 + (ui.input(|i| i.time) as f32 * 3.0).sin().abs() } else { 3.5 };
        p.circle_filled(rect.right_center() - vec2(12.0, 0.0), r, c);
    }
    resp
}

fn agent_row(ui: &mut egui::Ui, t: &Theme, a: &Agent, status: &str, status_color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 6.0, t.hover);
    }
    icons::avatar(p, rect.left_center() + vec2(18.0, 0.0), 10.0, color(a.color), a.name().chars().next().unwrap_or('?'));
    p.text(rect.left_top() + vec2(36.0, 5.0), Align2::LEFT_TOP, a.name(), FontId::proportional(13.0), t.text);
    let galley = p.layout_job(one_line(status, 11.0, status_color, rect.width() - 42.0));
    p.galley(rect.left_top() + vec2(36.0, 22.0), galley, status_color);
    resp
}

fn one_line(text: &str, size: f32, color: Color32, max_width: f32) -> LayoutJob {
    let mut job = LayoutJob::simple_singleline(text.to_string(), FontId::proportional(size), color);
    job.wrap.max_width = max_width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job
}

/// Overlapping avatars of the channel's team, main agent first.
fn team_stack(ui: &mut egui::Ui, t: &Theme, agents: &[Agent], members: &[String], health: &HashMap<String, Health>) -> egui::Response {
    let list: Vec<&Agent> = members.iter().filter_map(|id| agents.iter().find(|a| &a.id == id)).collect();
    let n = list.len().max(1) as f32;
    let (rect, resp) = ui.allocate_exact_size(vec2(14.0 * n + 12.0, 28.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect.expand(2.0), 14.0, t.hover);
    }
    for (i, a) in list.iter().enumerate().rev() {
        let c = egui::pos2(rect.left() + 12.0 + i as f32 * 14.0, rect.center().y);
        p.circle_filled(c, 11.5, t.bg);
        let down = role_status(a, health, t).1 == t.red;
        let col = if down { color(a.color).gamma_multiply(0.35) } else { color(a.color) };
        icons::avatar(p, c, 10.0, col, a.name().chars().next().unwrap_or('?'));
    }
    let names: Vec<String> = list.iter().enumerate().map(|(i, a)| format!("{}{}", a.name(), if i == 0 { " (main)" } else { "" })).collect();
    resp.on_hover_text(format!("Team: {}", names.join(", ")))
}

// ------------------------------------------------------------------ slide-out viewer

/// A file shown in the slide-out, re-read when it changes on disk (e.g. agents ticking off tasks).
struct FileView {
    path: std::path::PathBuf,
    text: String,
    modified: Option<std::time::SystemTime>,
    checked: Instant,
}

impl FileView {
    fn open(path: std::path::PathBuf) -> Self {
        let mut f = FileView { path, text: String::new(), modified: None, checked: Instant::now() };
        f.reload();
        f
    }

    fn reload(&mut self) {
        self.text = std::fs::read_to_string(&self.path).unwrap_or_else(|_| "_This file can't be read as text._".into());
        self.modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
    }

    /// Picks up edits made by agents (or you, in another editor), about once a second.
    fn refresh(&mut self) {
        if self.checked.elapsed() < Duration::from_millis(900) {
            return;
        }
        self.checked = Instant::now();
        let m = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if m != self.modified {
            self.reload();
        }
    }

    /// Tick / untick the checkbox on line `i` and save the file.
    fn toggle_task(&mut self, i: usize) {
        let mut lines: Vec<String> = self.text.lines().map(String::from).collect();
        let Some(l) = lines.get_mut(i) else { return };
        *l = if let Some(p) = l.find("[ ]") {
            format!("{}[x]{}", &l[..p], &l[p + 3..])
        } else if let Some(p) = l.find("[x]").or_else(|| l.find("[X]")) {
            format!("{}[ ]{}", &l[..p], &l[p + 3..])
        } else {
            return;
        };
        let mut text = lines.join("\n");
        if self.text.ends_with('\n') {
            text.push('\n');
        }
        if std::fs::write(&self.path, &text).is_ok() {
            self.text = text;
            self.modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        }
    }

    fn is_markdown(&self) -> bool {
        let ext = self.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        matches!(ext.as_str(), "md" | "mdx" | "markdown" | "txt" | "rst" | "adoc" | "")
    }
}

enum Panel {
    File(FileView),
    Docs { dir: std::path::PathBuf, files: Vec<std::path::PathBuf>, open: Option<FileView>, listed: Instant },
}

impl Panel {
    fn docs(dir: std::path::PathBuf) -> Self {
        let files = list_docs(&dir);
        Panel::Docs { dir, files, open: None, listed: Instant::now() }
    }
}

fn list_docs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
        if depth > 5 || out.len() > 400 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(d) else { return };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                continue;
            }
            if p.is_dir() {
                walk(&p, out, depth + 1);
            } else {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out, 0);
    out
}

fn is_text_doc(p: &std::path::Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(
        ext.as_str(),
        "md" | "mdx" | "markdown" | "txt" | "rst" | "adoc" | "json" | "yaml" | "yml" | "toml" | "csv" | "html" | "css" | "js" | "ts" | "sh" | "py" | "rs" | ""
    )
}

/// Draws the slide-out. Returns true when it should close.
fn panel_ui(ui: &mut egui::Ui, t: &Theme, panel: &mut Panel) -> bool {
    let mut close = false;
    let mut back = false;
    // Header
    let (title, subtitle, reveal): (String, String, std::path::PathBuf) = match panel {
        Panel::File(f) => {
            f.refresh();
            let name = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let sub = task_summary(&f.text).unwrap_or_else(|| pretty_path(&f.path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()));
            (name, sub, f.path.clone())
        }
        Panel::Docs { dir, files, open, listed } => {
            if listed.elapsed() > Duration::from_secs(3) {
                *files = list_docs(dir);
                *listed = Instant::now();
            }
            match open {
                Some(f) => {
                    f.refresh();
                    let rel = f.path.strip_prefix(dir.as_path()).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
                    (rel, task_summary(&f.text).unwrap_or_else(|| "docs".into()), f.path.clone())
                }
                None => ("Docs".into(), format!("{} files · {}", files.len(), pretty_path(&dir.to_string_lossy())), dir.clone()),
            }
        }
    };
    ui.horizontal(|ui| {
        if matches!(panel, Panel::Docs { open: Some(_), .. }) && icon_button(ui, t, Icon::Back, 24.0, false, "All docs").clicked() {
            back = true;
        }
        ui.vertical(|ui| {
            ui.add(egui::Label::new(RichText::new(&title).size(15.0).color(t.strong)).truncate());
            ui.add(egui::Label::new(RichText::new(&subtitle).size(11.5).color(t.secondary)).truncate());
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            if icon_button(ui, t, Icon::Close, 24.0, false, "Close (Esc)").clicked() {
                close = true;
            }
            if ui.add(egui::Button::new(RichText::new("Open").size(12.0)).frame(false)).on_hover_text("Open in your editor / Finder").clicked() {
                open_path(&reveal);
            }
        });
    });
    ui.add_space(6.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.left()..=r.right(), r.top(), Stroke::new(1.0, t.separator));
    ui.add_space(8.0);

    // Body
    ScrollArea::vertical().id_salt("viewer-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.set_width(ui.available_width() - 6.0);
        match panel {
            Panel::File(f) => file_body(ui, t, f),
            Panel::Docs { dir, files, open, .. } => match open {
                Some(f) => file_body(ui, t, f),
                None => {
                    if files.is_empty() {
                        ui.label(RichText::new("No files in docs/ yet.").color(t.secondary));
                    }
                    let mut pick = None;
                    for p in files.iter() {
                        let rel = p.strip_prefix(dir.as_path()).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
                        if resp.hovered() {
                            ui.painter().rect_filled(rect, 6.0, t.hover);
                        }
                        let (folder, name) = match rel.rsplit_once('/') {
                            Some((f, n)) => (format!("{f}/"), n.to_string()),
                            None => (String::new(), rel.clone()),
                        };
                        let x = rect.left() + 8.0;
                        let g1 = ui.painter().layout_no_wrap(folder, FontId::proportional(12.5), t.tertiary);
                        let w1 = g1.size().x;
                        ui.painter().galley(egui::pos2(x, rect.center().y - g1.size().y / 2.0), g1, t.tertiary);
                        let g2 = ui.painter().layout_job(one_line(&name, 13.0, t.text, rect.width() - w1 - 16.0));
                        ui.painter().galley(egui::pos2(x + w1, rect.center().y - g2.size().y / 2.0), g2, t.text);
                        if resp.on_hover_text(pretty_path(&p.to_string_lossy())).clicked() {
                            pick = Some(p.clone());
                        }
                    }
                    if let Some(p) = pick {
                        if is_text_doc(&p) {
                            *open = Some(FileView::open(p));
                        } else {
                            open_path(&p); // images, PDFs…: their own app
                        }
                    }
                }
            },
        }
        ui.add_space(20.0);
    });
    if back {
        if let Panel::Docs { open, .. } = panel {
            *open = None;
        }
    }
    close
}

fn file_body(ui: &mut egui::Ui, t: &Theme, f: &mut FileView) {
    if f.text.trim().is_empty() {
        ui.label(RichText::new("This file is empty.").color(t.secondary));
        return;
    }
    if !f.is_markdown() {
        Frame::new().fill(t.code_bg).corner_radius(8).inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(&f.text).font(FontId::monospace(12.0)).color(t.text)).selectable(true).wrap());
        });
        return;
    }
    if let Some(i) = render_file(ui, t, &f.text) {
        f.toggle_task(i);
    }
}

/// "3 open · 5 done" for files with checkboxes.
fn task_summary(text: &str) -> Option<String> {
    let (mut open, mut done) = (0, 0);
    let mut in_code = false;
    for l in text.lines() {
        if l.trim_start().starts_with("```") {
            in_code = !in_code;
        } else if !in_code {
            match task_line(l.trim_start()) {
                Some((true, _)) => done += 1,
                Some((false, _)) => open += 1,
                None => {}
            }
        }
    }
    (open + done > 0).then(|| format!("{open} open · {done} done"))
}

/// "- [ ] text" / "* [x] text" -> (done, text)
fn task_line(l: &str) -> Option<(bool, &str)> {
    let l = l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")).or_else(|| l.strip_prefix("+ "))?;
    if let Some(r) = l.strip_prefix("[ ] ") {
        Some((false, r))
    } else {
        l.strip_prefix("[x] ").or_else(|| l.strip_prefix("[X] ")).map(|r| (true, r))
    }
}

/// Markdown with real checkboxes. Returns the line index of a checkbox you clicked.
fn render_file(ui: &mut egui::Ui, t: &Theme, text: &str) -> Option<usize> {
    let mut clicked = None;
    let mut buf = String::new();
    let mut in_code = false;
    let flush = |ui: &mut egui::Ui, buf: &mut String| {
        if !buf.trim().is_empty() {
            render_markdown(ui, t, buf, 13.5);
        }
        buf.clear();
    };
    for (i, line) in text.lines().enumerate() {
        let tr = line.trim_start();
        if tr.starts_with("```") {
            in_code = !in_code;
        } else if !in_code {
            if let Some((done, rest)) = task_line(tr) {
                flush(ui, &mut buf);
                let indent = (line.len() - tr.len()) as f32 * 7.0;
                if task_row(ui, t, done, rest, indent) {
                    clicked = Some(i);
                }
                continue;
            }
        }
        buf.push_str(line);
        buf.push('\n');
    }
    flush(ui, &mut buf);
    clicked
}

/// One task: a real checkbox (click to tick it off) and its text.
fn task_row(ui: &mut egui::Ui, t: &Theme, done: bool, text: &str, indent: f32) -> bool {
    let mut clicked = false;
    ui.horizontal_top(|ui| {
        ui.add_space(indent);
        let (rect, resp) = ui.allocate_exact_size(vec2(18.0, 20.0), Sense::click());
        let b = Rect::from_center_size(rect.center(), vec2(14.0, 14.0));
        let p = ui.painter();
        if done {
            p.rect_filled(b, 4.0, t.accent);
            let w = Stroke::new(1.8, Color32::WHITE);
            p.line_segment([b.left_center() + vec2(3.0, 0.5), b.center() + vec2(-1.0, 3.5)], w);
            p.line_segment([b.center() + vec2(-1.0, 3.5), b.right_top() + vec2(-3.0, 3.5)], w);
        } else {
            p.rect_stroke(b, 4.0, Stroke::new(1.3, if resp.hovered() { t.accent } else { t.tertiary }), egui::StrokeKind::Inside);
        }
        if resp.on_hover_text(if done { "Mark as not done" } else { "Mark as done" }).clicked() {
            clicked = true;
        }
        let fmt = TextFormat {
            font_id: FontId::proportional(13.5),
            color: if done { t.tertiary } else { t.text },
            strikethrough: if done { Stroke::new(1.0, t.tertiary) } else { Stroke::NONE },
            line_height: Some(19.0),
            ..Default::default()
        };
        let mut job = LayoutJob::default();
        inline(&mut job, text, &fmt, t);
        job.wrap.max_width = ui.available_width() - 4.0;
        ui.add(egui::Label::new(job).selectable(true).wrap());
    });
    ui.add_space(2.0);
    clicked
}

fn is_image(p: &std::path::Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
}

/// Copy attachments into <project>/.yesharness/attachments so sandboxed agents can read them.
/// That folder ignores itself in Git. Returns absolute paths (falls back to the original file).
fn store_attachments(folder: &str, files: &[std::path::PathBuf]) -> Vec<String> {
    let base = std::path::Path::new(folder).join(".yesharness");
    let dir = base.join("attachments");
    let ok = std::fs::create_dir_all(&dir).is_ok();
    if ok && !base.join(".gitignore").exists() {
        let _ = std::fs::write(base.join(".gitignore"), "# YesHarness attachments: not part of your project\n*\n");
    }
    files
        .iter()
        .map(|f| {
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "image.png".into());
            let dest = dir.join(format!("{}-{name}", model::new_id()));
            if ok && std::fs::copy(f, &dest).is_ok() {
                dest.to_string_lossy().into_owned()
            } else {
                f.to_string_lossy().into_owned()
            }
        })
        .collect()
}

/// A small rounded label in the header.
fn chip(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32, tip: &str) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(11.0), fg);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 14.0, 18.0), Sense::click());
    let fill = if resp.hovered() { bg.gamma_multiply(1.6) } else { bg };
    ui.painter().rect_filled(rect, 9.0, fill);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
    resp.on_hover_text(tip)
}

fn open_path(p: &std::path::Path) {
    let _ = std::process::Command::new("open").arg(p).spawn();
}

/// Git/GitHub, AGENTS.md, TASKS.md and docs at a glance. Missing pieces are one click away.
fn project_chips(ui: &mut egui::Ui, t: &Theme, p: &crate::project::ProjectInfo, folder: &str, cid: &str, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let good = (t.text, t.control);
        let dim = (t.tertiary, t.control.gamma_multiply(0.6));
        let warn = (t.amber, t.amber.gamma_multiply(0.14));
        if !p.exists {
            chip(ui, "Folder not found", t.red, t.red.gamma_multiply(0.14), "Pick another folder in the ••• menu.");
            return;
        }
        // Git / GitHub
        match (&p.github, p.git) {
            (Some(repo), _) => {
                let label = match &p.branch {
                    Some(b) => format!("GitHub · {repo} · {b}"),
                    None => format!("GitHub · {repo}"),
                };
                if chip(ui, &label, good.0, good.1, "Connected to GitHub. Click to open the repository.").clicked() {
                    let _ = std::process::Command::new("open").arg(format!("https://github.com/{repo}")).spawn();
                }
            }
            (None, true) => {
                let label = p.branch.as_ref().map(|b| format!("Git · {b}")).unwrap_or_else(|| "Git".into());
                chip(ui, &label, good.0, good.1, "A Git repository without a GitHub remote.");
            }
            (None, false) => {
                chip(ui, "No Git", dim.0, dim.1, "This folder isn't a Git repository.");
            }
        }
        // AGENTS.md
        match p.agents_md {
            Some(n) if n > 0 => {
                if chip(ui, "AGENTS.md", good.0, good.1, &format!("{n} lines of instructions for the agents. Click to read.")).clicked() {
                    if let Some(f) = &p.agents_path {
                        actions.push(Action::ShowFile(f.clone()));
                    }
                }
            }
            Some(_) => {
                if chip(ui, "AGENTS.md is empty", warn.0, warn.1, "Click to ask the team to fill it in.").clicked() {
                    actions.push(Action::AskTeam(cid.into(), ASK_AGENTS_MD.into()));
                }
            }
            None => {
                if chip(ui, "No AGENTS.md", dim.0, dim.1, "Click to ask the team to write one.").clicked() {
                    actions.push(Action::AskTeam(cid.into(), ASK_AGENTS_MD.into()));
                }
            }
        }
        // TASKS.md
        match p.tasks {
            Some((open, done)) => {
                let label = match (open, done) {
                    (0, 0) => "TASKS.md".to_string(),
                    (0, _) => "All tasks done".to_string(),
                    (1, _) => "1 open task".to_string(),
                    (n, _) => format!("{n} open tasks"),
                };
                if chip(ui, &label, good.0, good.1, &format!("TASKS.md: {open} open, {done} done. Click to see the list.")).clicked() {
                    if let Some(f) = &p.tasks_path {
                        actions.push(Action::ShowFile(f.clone()));
                    }
                }
            }
            None => {
                if chip(ui, "No TASKS.md", dim.0, dim.1, "Click to ask the team to start a task list.").clicked() {
                    actions.push(Action::AskTeam(cid.into(), ASK_TASKS_MD.into()));
                }
            }
        }
        // docs/
        if p.docs > 0 {
            let label = if p.docs == 1 { "1 doc".to_string() } else { format!("{} docs", p.docs) };
            if chip(ui, &label, good.0, good.1, "Files in docs/. Click to browse them.").clicked() {
                actions.push(Action::ShowDocs(p.docs_path.clone().unwrap_or_else(|| std::path::PathBuf::from(folder))));
            }
        } else {
            chip(ui, "No docs", dim.0, dim.1, "No docs/ folder yet.");
        }
    });
}

const ASK_AGENTS_MD: &str = "Please write an AGENTS.md for this project: what it is, how it's organized, how to run, build and \
test it, conventions to follow, and anything an agent should know before changing things. Keep it short and concrete.";
const ASK_TASKS_MD: &str = "Please start a TASKS.md for this project with the current open tasks as a checklist (- [ ] …), \
based on what you can see in the folder.";

fn mic_orb(ui: &mut egui::Ui, t: &Theme, listening: bool, level: f32, hands_free: bool) -> egui::Response {
    let size = 40.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
    let c = rect.center();
    let p = ui.painter();
    let base = if listening && !hands_free { t.red } else if listening { t.accent } else { t.tertiary };
    if listening {
        p.circle_filled(c, size / 2.0 + 2.0 + level * 10.0, base.gamma_multiply(0.18));
    }
    let fill = if resp.hovered() { base.gamma_multiply(0.9) } else { base };
    p.circle_filled(c, size / 2.0, fill);
    icons::paint(p, Icon::Mic, Rect::from_center_size(c, Vec2::splat(20.0)), Color32::WHITE);
    let tip = match (hands_free, listening) {
        (true, true) => "Always listening. Click to pause.",
        (true, false) => "Paused. Click to listen.",
        (false, true) => "Listening… release (or tap again) to send",
        (false, false) => "Hold to talk · tap to start/stop",
    };
    resp.on_hover_text(tip)
}

/// macOS-style switch.
fn toggle(ui: &mut egui::Ui, t: &Theme, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(32.0, 18.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let k = ui.ctx().animate_bool_responsive(resp.id, *on);
    let off_col = if t.dark { Color32::from_gray(70) } else { Color32::from_gray(214) };
    let bg = egui::lerp(egui::Rgba::from(off_col)..=egui::Rgba::from(t.accent), k);
    let p = ui.painter();
    p.rect_filled(rect, 9.0, Color32::from(bg));
    let x = egui::lerp(rect.left() + 9.0..=rect.right() - 9.0, k);
    p.circle_filled(egui::pos2(x, rect.center().y + 0.5), 7.5, Color32::from_black_alpha(40));
    p.circle_filled(egui::pos2(x, rect.center().y), 7.5, Color32::WHITE);
    resp
}

/// macOS segmented control. Returns the clicked index.
fn segmented(ui: &mut egui::Ui, t: &Theme, current: usize, labels: &[&str]) -> Option<usize> {
    let mut clicked = None;
    Frame::new().fill(t.control).corner_radius(7).inner_margin(Margin::same(2)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.horizontal(|ui| {
            for (i, l) in labels.iter().enumerate() {
                let galley = ui.painter().layout_no_wrap(l.to_string(), FontId::proportional(12.5), t.text);
                let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 22.0, 22.0), Sense::click());
                let p = ui.painter();
                if i == current {
                    let fill = if t.dark { Color32::from_gray(96) } else { Color32::WHITE };
                    p.add(Shadow { offset: [0, 1], blur: 2, spread: 0, color: Color32::from_black_alpha(30) }.as_shape(rect, 5));
                    p.rect_filled(rect, 5.0, fill);
                } else if resp.hovered() {
                    p.rect_filled(rect, 5.0, t.hover);
                }
                p.galley(rect.center() - galley.size() / 2.0, galley, t.text);
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
        });
    });
    clicked
}

fn primary_button(ui: &mut egui::Ui, t: &Theme, text: &str, enabled: bool) -> bool {
    let b = egui::Button::new(RichText::new(text).color(Color32::WHITE))
        .fill(if enabled { t.accent } else { t.accent.gamma_multiply(0.4) })
        .corner_radius(6)
        .min_size(vec2(72.0, 26.0));
    ui.add_enabled(enabled, b).clicked()
}

fn menu_item(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(egui::Button::new(text).frame(false).min_size(vec2(ui.available_width(), 22.0))).clicked()
}

/// A rounded settings group (like System Settings), with an optional caption above.
fn group<R>(ui: &mut egui::Ui, t: &Theme, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    if !title.is_empty() {
        ui.add_space(2.0);
        ui.label(RichText::new(title).size(12.0).color(t.secondary));
    }
    let r = Frame::new()
        .fill(t.card)
        .stroke(Stroke::new(1.0, t.separator))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(14, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner;
    ui.add_space(14.0);
    r
}

fn form_row(ui: &mut egui::Ui, t: &Theme, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        let (r, _) = ui.allocate_exact_size(vec2(96.0, 22.0), Sense::hover());
        ui.painter().text(r.left_center(), Align2::LEFT_CENTER, label, FontId::proportional(13.0), t.text);
        add(ui);
    });
}

fn form_sep(ui: &mut egui::Ui, t: &Theme) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.left()..=r.right(), r.top() + 1.0, Stroke::new(1.0, t.separator));
    ui.add_space(3.0);
}

// ------------------------------------------------------------------ sheets

impl App {
    fn sheets(&mut self, ctx: &egui::Context, t: &Theme) {
        let shared = self.shared.clone();
        let sheet_frame = Frame::new()
            .fill(t.bg)
            .corner_radius(12)
            .stroke(Stroke::new(1.0, t.separator))
            .shadow(Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(if t.dark { 140 } else { 60 }) });
        let backdrop = Color32::from_black_alpha(if t.dark { 110 } else { 50 });

        // ------------------------------------------------ settings
        if self.show_settings {
            let mut done = false;
            let mut toggle_listening = false;
            let resp = egui::Modal::new(Id::new("settings")).frame(sheet_frame).backdrop_color(backdrop).show(ctx, |ui| {
                let mut data = shared.data.lock().unwrap();
                let before = format!("{:?}", data.settings);
                ui.set_width(520.0);
                Frame::new().inner_margin(Margin::same(20)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Settings").size(16.0).color(t.strong));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if primary_button(ui, t, "Done", true) {
                                done = true;
                            }
                        });
                    });
                    ui.add_space(12.0);
                    group(ui, t, "Appearance", |ui| {
                        form_row(ui, t, "Theme", |ui| {
                            let opts = ["auto", "light", "dark"];
                            let cur = opts.iter().position(|o| *o == data.settings.theme).unwrap_or(0);
                            if let Some(i) = segmented(ui, t, cur, &["Automatic", "Light", "Dark"]) {
                                data.settings.theme = opts[i].into();
                            }
                        });
                        ui.label(RichText::new("Automatic follows your Mac's light or dark appearance.").size(11.5).color(t.secondary));
                    });
                    group(ui, t, "Voice", |ui| {
                        form_row(ui, t, "Listening", |ui| {
                            let cur = if data.settings.hands_free { 0 } else { 1 };
                            if let Some(i) = segmented(ui, t, cur, &["Always listening", "Push to talk"]) {
                                if i != cur {
                                    toggle_listening = true;
                                }
                            }
                        });
                        form_sep(ui, t);
                        form_row(ui, t, "Read replies", |ui| {
                            let mut on = data.settings.read_aloud;
                            if toggle(ui, t, &mut on).changed() {
                                data.settings.read_aloud = on;
                                if !on {
                                    voice::stop_speaking();
                                }
                            }
                        });
                        form_sep(ui, t);
                        form_row(ui, t, "Speed", |ui| {
                            let steps = model::SPEECH_SPEEDS;
                            let cur = steps.iter().position(|s| (s - data.settings.speech_speed).abs() < 0.01).unwrap_or(2);
                            if let Some(i) = segmented(ui, t, cur, &["1×", "1.25×", "1.5×", "1.75×", "2×"]) {
                                data.settings.speech_speed = steps[i];
                                voice::set_speed(steps[i]);
                                voice::stop_speaking();
                                voice::speak("This is how fast I'll talk.", &voice::auto_voice(0));
                            }
                        });
                        form_sep(ui, t);
                        form_row(ui, t, "Recognition", |ui| {
                            let cur = if data.settings.stt_engine == "parakeet" { 1 } else { 0 };
                            if let Some(i) = segmented(ui, t, cur, &["Apple", "Parakeet Ultra"]) {
                                data.settings.stt_engine = if i == 1 { "parakeet".into() } else { "apple".into() };
                                if i == 1 {
                                    let c = ctx.clone();
                                    voice::setup_parakeet(move || c.request_repaint());
                                }
                            }
                        });
                        let setup = voice::parakeet_setup();
                        let note = if data.settings.stt_engine != "parakeet" {
                            "Built into macOS: live text as you speak, nothing to download.".to_string()
                        } else if let Some(e) = &setup.error {
                            format!("Parakeet couldn't be set up: {e} Using Apple meanwhile.")
                        } else if setup.running {
                            format!("{} Using Apple until it's ready.", setup.status)
                        } else if voice::parakeet_ready() {
                            "NVIDIA Parakeet Ultra on the Neural Engine: fewer mistakes, especially with names and terms.".to_string()
                        } else {
                            "Setting up…".to_string()
                        };
                        ui.add(egui::Label::new(RichText::new(note).size(11.5).color(if setup.error.is_some() { t.red } else { t.secondary })).wrap());
                        if setup.error.is_some() && data.settings.stt_engine == "parakeet" && ui.button("Try Again").clicked() {
                            let c = ctx.clone();
                            voice::setup_parakeet(move || c.request_repaint());
                        }
                        form_sep(ui, t);
                        let natural = voice::natural_voices();
                        form_row(ui, t, "Natural voices", |ui| {
                            let label = match natural.len() {
                                0 => "None installed yet".to_string(),
                                1 => format!("1 installed ({})", natural[0]),
                                n => format!("{n} installed"),
                            };
                            ui.label(RichText::new(label).color(if natural.is_empty() { t.secondary } else { t.text }));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("Get Natural Voices…").clicked() {
                                    voice::open_voice_settings();
                                }
                                if !natural.is_empty() && ui.button("Hear").clicked() {
                                    voice::stop_speaking();
                                    voice::speak("Hi, this is how I sound now.", &natural[0]);
                                }
                            });
                        });
                        ui.add_space(2.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    "Apple's Premium and Enhanced voices sound much more human, and they're free. In System Settings, go to \
                                     Accessibility → Spoken Content → System voice → Manage Voices…, then download an English voice \
                                     marked Premium (for example Zoe, Ava or Evan). YesHarness uses them automatically, and the main \
                                     agent gets the best one.",
                                )
                                .size(11.5)
                                .color(t.secondary),
                            )
                            .wrap(),
                        );
                    });
                });
                if format!("{:?}", data.settings) != before {
                    shared.dirty.store(true, Ordering::Relaxed);
                }
            });
            if toggle_listening {
                self.toggle_hands_free(ctx);
            }
            if done || resp.should_close() {
                self.show_settings = false;
                voice::refresh_voices(); // maybe you downloaded some meanwhile
            }
        }

        // ------------------------------------------------ new channel
        if self.new_channel.is_some() {
            let mut create = false;
            let mut cancel = false;
            let resp = egui::Modal::new(Id::new("new-channel")).frame(sheet_frame).backdrop_color(backdrop).show(ctx, |ui| {
                let Some(nc) = self.new_channel.as_mut() else { return };
                let data = shared.data.lock().unwrap();
                ui.set_width(440.0);
                Frame::new().inner_margin(Margin::same(20)).show(ui, |ui| {
                    ui.label(RichText::new("New Channel").size(16.0).color(t.strong));
                    ui.label(RichText::new("One channel per project. The team works in its folder.").size(12.0).color(t.secondary));
                    ui.add_space(12.0);
                    group(ui, t, "", |ui| {
                        form_row(ui, t, "Name", |ui| {
                            ui.add(egui::TextEdit::singleline(&mut nc.name).hint_text("my-project").desired_width(f32::INFINITY));
                        });
                        form_sep(ui, t);
                        form_row(ui, t, "Folder", |ui| {
                            ui.label(RichText::new(short_path(&nc.folder)).color(t.secondary));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("Choose…").clicked() {
                                    if let Some(dir) = rfd::FileDialog::new().set_directory(&nc.folder).pick_folder() {
                                        nc.folder = dir.to_string_lossy().into_owned();
                                        if nc.name.trim().is_empty() {
                                            nc.name = dir.file_name().map(|f| slug(&f.to_string_lossy())).unwrap_or_default();
                                        }
                                    }
                                }
                            });
                        });
                    });
                    group(ui, t, "Team", |ui| {
                        for (i, a) in data.agents.iter().enumerate() {
                            if i > 0 {
                                form_sep(ui, t);
                            }
                            ui.horizontal(|ui| {
                                ui.set_min_height(28.0);
                                let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                                icons::avatar(ui.painter(), r.center(), 10.0, color(a.color), a.name().chars().next().unwrap_or('?'));
                                ui.label(RichText::new(a.name()).color(t.text));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let mut on = nc.members.contains(&a.id);
                                    if toggle(ui, t, &mut on).changed() {
                                        if on {
                                            nc.members.push(a.id.clone());
                                        } else {
                                            nc.members.retain(|m| m != &a.id);
                                        }
                                    }
                                });
                            });
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if primary_button(ui, t, "Create", !slug(&nc.name).is_empty()) {
                                create = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                });
            });
            if create {
                if let Some(nc) = self.new_channel.take() {
                    let mut data = shared.data.lock().unwrap();
                    // Keep the team order from settings, so the main agent is predictable.
                    let members: Vec<String> = data.agents.iter().filter(|a| nc.members.contains(&a.id)).map(|a| a.id.clone()).collect();
                    let ch = Channel {
                        id: model::new_id(),
                        name: slug(&nc.name),
                        folder: nc.folder.clone(),
                        members,
                        messages: vec![sys(&format!("Channel created. The team works in {}.", pretty_path(&nc.folder)))],
                        sessions: HashMap::new(),
                        seen: HashMap::new(),
                        watch: HashMap::new(),
                        unread: false,
                    };
                    data.selected = Some(ch.id.clone());
                    data.channels.push(ch);
                    shared.dirty.store(true, Ordering::Relaxed);
                    let _ = shared.wake.0.lock().unwrap().send(()); // baseline its files now
                }
            } else if cancel || resp.should_close() {
                self.new_channel = None;
            }
        }

        // ------------------------------------------------ team
        if !self.show_team {
            return;
        }
        let health: HashMap<String, Health> = shared.health.lock().unwrap().clone();
        let mut done = false;
        let mut refresh = false;
        let resp = egui::Modal::new(Id::new("team")).frame(sheet_frame).backdrop_color(backdrop).show(ctx, |ui| {
            let mut data = shared.data.lock().unwrap();
            let agents_before = data.agents.clone();
            let screen = ctx.screen_rect();
            let (w, h) = ((screen.width() - 80.0).min(860.0), (screen.height() - 80.0).min(600.0));
            ui.set_width(w);
            ui.set_height(h);
            if self.team_sel.as_ref().is_none_or(|id| data.agent(id).is_none()) {
                self.team_sel = data.agents.first().map(|a| a.id.clone());
            }
            let mut remove = None;
            let mut add: Option<Agent> = None;
            let mut reset: Vec<String> = Vec::new();

            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.horizontal_top(|ui| {
                // Left: the team list
                Frame::new()
                    .fill(t.sidebar)
                    .corner_radius(CornerRadius { nw: 12, sw: 12, ne: 0, se: 0 })
                    .inner_margin(Margin { left: 10, right: 10, top: 14, bottom: 10 })
                    .show(ui, |ui| ui.vertical(|ui| {
                        ui.set_width(210.0);
                        ui.set_height(h - 24.0);
                        ui.spacing_mut().item_spacing = vec2(6.0, 2.0);
                        section_label(ui, t, "Team");
                        ui.add_space(4.0);
                        for a in &data.agents {
                            let sel = self.team_sel.as_deref() == Some(a.id.as_str());
                            let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::click());
                            let p = ui.painter();
                            if sel {
                                p.rect_filled(rect, 7.0, t.selected_row);
                            } else if r.hovered() {
                                p.rect_filled(rect, 7.0, t.hover);
                            }
                            icons::avatar(p, rect.left_center() + vec2(20.0, 0.0), 12.0, color(a.color), a.name().chars().next().unwrap_or('?'));
                            p.text(rect.left_top() + vec2(40.0, 7.0), Align2::LEFT_TOP, a.name(), FontId::proportional(13.5), t.text);
                            let role = if a.description.is_empty() { a.kind.label().to_string() } else { a.description.clone() };
                            let g = p.layout_job(one_line(&role, 11.0, t.secondary, rect.width() - 46.0));
                            p.galley(rect.left_top() + vec2(40.0, 25.0), g, t.secondary);
                            if r.clicked() {
                                self.team_sel = Some(a.id.clone());
                            }
                        }
                        ui.add_space(8.0);
                        let n = data.agents.len();
                        ui.menu_button(RichText::new("+  Add Agent").color(t.accent), |ui| {
                            ui.set_min_width(240.0);
                            for tp in model::TEMPLATES {
                                if menu_item(ui, tp.title) {
                                    add = Some(tp.agent(PALETTE[n % PALETTE.len()]));
                                    ui.close_menu();
                                }
                            }
                            ui.separator();
                            if menu_item(ui, "Blank Agent") {
                                let mut a = model::TEMPLATES[1].agent(PALETTE[n % PALETTE.len()]);
                                a.handle = "agent".into();
                                a.description.clear();
                                a.briefing.clear();
                                add = Some(a);
                                ui.close_menu();
                            }
                            if menu_item(ui, "Custom Command (Ollama, Aider…)") {
                                let mut a = model::TEMPLATES[1].agent(PALETTE[n % PALETTE.len()]);
                                a.handle = "local".into();
                                a.kind = AgentKind::Custom;
                                a.description = "Local model".into();
                                a.briefing.clear();
                                add = Some(a);
                                ui.close_menu();
                            }
                        });
                    }));

                // Right: details
                Frame::new().inner_margin(Margin { left: 24, right: 20, top: 14, bottom: 14 }).show(ui, |ui| ui.vertical(|ui| {
                    ui.set_width(w - 210.0 - 20.0 - 44.0);
                    ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if primary_button(ui, t, "Done", true) {
                                done = true;
                            }
                            if ui.button("Check Limits").on_hover_text("Re-check logins, models and usage").clicked() {
                                refresh = true;
                            }
                        });
                    });
                    ScrollArea::vertical().max_height(h - 70.0).auto_shrink([false, false]).show(ui, |ui| {
                        let Some(idx) = data.agents.iter().position(|a| Some(&a.id) == self.team_sel.as_ref()) else { return };
                        let snapshot = data.agents[idx].clone();
                        let (status, status_col, tip) = role_status(&snapshot, &health, t);
                        let a = &mut data.agents[idx];
                        let before = (a.kind, a.briefing.clone());
                        let h = health.get(&probe::key(a));

                        // Header
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
                            icons::avatar(ui.painter(), r.center(), 22.0, color(a.color), a.name().chars().next().unwrap_or('?'));
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.add_space(4.0);
                                ui.label(RichText::new(a.name()).size(18.0).color(t.strong));
                                ui.label(RichText::new(&status).size(12.0).color(status_col)).on_hover_text(tip);
                            });
                        });
                        ui.add_space(14.0);

                        group(ui, t, "", |ui| {
                            form_row(ui, t, "Display name", |ui| {
                                ui.add(egui::TextEdit::singleline(&mut a.display_name)
                                    .hint_text(&a.handle).desired_width(f32::INFINITY));
                            });
                            form_sep(ui, t);
                            form_row(ui, t, "Handle", |ui| {
                                let mut hd = a.handle.clone();
                                let r = ui.add(egui::TextEdit::singleline(&mut hd).desired_width(f32::INFINITY));
                                if r.on_hover_text("Say it to talk to this agent directly: \"Writer, …\"").changed() {
                                    let hd = slug(&hd);
                                    if !hd.is_empty() {
                                        a.handle = hd;
                                    }
                                }
                            });
                            form_sep(ui, t);
                            form_row(ui, t, "Role", |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut a.description)
                                        .hint_text("One line. The main agent uses it to pick who does what.")
                                        .desired_width(f32::INFINITY),
                                );
                            });
                            form_sep(ui, t);
                            form_row(ui, t, "Voice", |ui| {
                                let auto = voice::auto_voice(idx);
                                let auto_label = if auto.is_empty() { "System voice".to_string() } else { auto.clone() };
                                let current = if a.voice.is_empty() { format!("Automatic ({auto_label})") } else { a.voice.clone() };
                                egui::ComboBox::from_id_salt(format!("voice-{}", a.id)).selected_text(current).width(230.0).show_ui(ui, |ui| {
                                    ui.selectable_value(&mut a.voice, String::new(), format!("Automatic ({auto_label})"));
                                    for v in voice::voices() {
                                        ui.selectable_value(&mut a.voice, v.clone(), v);
                                    }
                                });
                                if icon_button(ui, t, Icon::Play, 24.0, false, "Hear it").clicked() {
                                    let v = if a.voice.is_empty() { auto } else { a.voice.clone() };
                                    voice::stop_speaking();
                                    voice::speak(&format!("Hi, I'm {}.", a.name()), &v);
                                }
                            });
                            form_sep(ui, t);
                            form_row(ui, t, "Color", |ui| {
                                for c in PALETTE {
                                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                                    if a.color == c {
                                        ui.painter().circle_stroke(r.center(), 9.0, Stroke::new(2.0, color(c)));
                                    }
                                    ui.painter().circle_filled(r.center(), 6.5, color(c));
                                    if resp.clicked() {
                                        a.color = c;
                                    }
                                }
                            });
                        });

                        group(ui, t, "Briefing", |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut a.briefing)
                                    .frame(false)
                                    .hint_text("Standing instructions: what this agent does, where it saves things, its style…")
                                    .desired_rows(5)
                                    .desired_width(f32::INFINITY),
                            );
                        });

                        group(ui, t, "Engine", |ui| {
                            form_row(ui, t, "Runs on", |ui| {
                                let kinds = [AgentKind::Claude, AgentKind::Codex, AgentKind::Local, AgentKind::Custom];
                                let cur = kinds.iter().position(|k| *k == a.kind).unwrap_or(0);
                                if let Some(i) = segmented(ui, t, cur, &["Claude Code", "Codex", "Local", "Command"]) {
                                    if kinds[i] != a.kind {
                                        a.kind = kinds[i];
                                        a.model.clear();
                                    }
                                }
                            });
                            form_sep(ui, t);
                            if a.kind == AgentKind::Custom {
                                form_row(ui, t, "Command", |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut a.command)
                                            .hint_text("ollama run llama3.2   (prompt on stdin)")
                                            .font(egui::TextStyle::Monospace)
                                            .desired_width(f32::INFINITY),
                                    );
                                });
                            } else {
                                form_row(ui, t, "Model", |ui| {
                                    let auto_pick = {
                                        let mut auto = a.clone();
                                        auto.model.clear();
                                        probe::pick_model(&auto, h).unwrap_or_else(|| "CLI default".into())
                                    };
                                    let current = if a.model.trim().is_empty() { format!("Automatic ({auto_pick})") } else { a.model.clone() };
                                    egui::ComboBox::from_id_salt(format!("model-{}", a.id)).selected_text(current).width(260.0).show_ui(ui, |ui| {
                                        ui.selectable_value(&mut a.model, String::new(), format!("Automatic: best available ({auto_pick})"));
                                        for m in h.map(|h| h.models.as_slice()).unwrap_or_default() {
                                            if m.hidden {
                                                continue;
                                            }
                                            let bad = h.is_some_and(|h| h.bad_models.contains(&m.id));
                                            let label = format!("{}{}", if m.name.is_empty() { &m.id } else { &m.name }, if bad { "  (not available)" } else { "" });
                                            ui.selectable_value(&mut a.model, m.id.clone(), label);
                                        }
                                    });
                                });
                                form_sep(ui, t);
                                form_row(ui, t, "Thinking", |ui| {
                                    let opts = ["low", "", "high"];
                                    let cur = opts.iter().position(|o| *o == a.effort).unwrap_or(1);
                                    if let Some(i) = segmented(ui, t, cur, &["Quick", "Normal", "Deep"]) {
                                        a.effort = opts[i].to_string();
                                    }
                                    ui.label(RichText::new("Quick answers faster; Deep is better at hard problems").size(12.0).color(t.secondary));
                                });
                                if a.kind != AgentKind::Local {
                                    form_sep(ui, t);
                                    form_row(ui, t, "Your plugins", |ui| {
                                        toggle(ui, t, &mut a.use_my_plugins);
                                        let note = if a.use_my_plugins {
                                            "Loads your MCP servers and plugins (slower)"
                                        } else {
                                            "Lean and fast: without your global MCP servers and plugins"
                                        };
                                        ui.label(RichText::new(note).size(12.0).color(t.secondary));
                                    });
                                }
                                form_sep(ui, t);
                                form_row(ui, t, "Full access", |ui| {
                                    toggle(ui, t, &mut a.full_access);
                                    let note = match (a.kind, a.full_access) {
                                        (AgentKind::Claude, false) => "Edits files; shell commands blocked",
                                        (AgentKind::Codex | AgentKind::Local, false) => "Sandboxed to the project folder",
                                        _ => "No sandbox. Trusted projects only.",
                                    };
                                    ui.label(RichText::new(note).size(12.0).color(t.secondary));
                                });
                            }
                        });

                        if let Some(h) = h.filter(|h| !h.limits.is_empty() || h.plan.is_some()) {
                            group(ui, t, "Usage", |ui| {
                                if let Some(p) = &h.plan {
                                    form_row(ui, t, "Plan", |ui| {
                                        ui.label(RichText::new(capitalize(p)).color(t.secondary));
                                    });
                                }
                                for l in &h.limits {
                                    form_sep(ui, t);
                                    form_row(ui, t, &capitalize(&l.label), |ui| {
                                        let col = if l.used_pct >= 99.0 { t.red } else if l.used_pct >= 85.0 { t.amber } else { t.accent };
                                        ui.add(egui::ProgressBar::new(l.used_pct / 100.0).desired_width(180.0).desired_height(6.0).fill(col).corner_radius(3));
                                        ui.label(RichText::new(format!("{:.0}%", l.used_pct)).size(12.0).color(t.text));
                                        if let Some(r) = l.resets_at {
                                            ui.label(RichText::new(format!("resets {}", probe::when(r))).size(12.0).color(t.secondary));
                                        }
                                    });
                                }
                            });
                        }

                        if ui.add(egui::Button::new(RichText::new("Remove Agent").color(t.red)).frame(false)).clicked() {
                            remove = Some(a.id.clone());
                        }
                        if before != (a.kind, a.briefing.clone()) {
                            reset.push(a.id.clone());
                        }
                    });
                }));
            });

            if !reset.is_empty() {
                for ch in data.channels.iter_mut() {
                    ch.sessions.retain(|k, _| !reset.iter().any(|id| k.starts_with(&format!("{id}:"))));
                    for id in &reset {
                        ch.seen.remove(id);
                    }
                }
            }
            if let Some(id) = remove {
                data.agents.retain(|a| a.id != id);
                for ch in data.channels.iter_mut() {
                    ch.members.retain(|m| m != &id);
                }
                self.team_sel = None;
                shared.dirty.store(true, Ordering::Relaxed);
            }
            if let Some(mut a) = add {
                let base = a.handle.clone();
                let mut n = 2;
                while data.agents.iter().any(|x| x.handle == a.handle) {
                    a.handle = format!("{base}{n}");
                    n += 1;
                }
                // New teammates are available in every channel.
                for ch in data.channels.iter_mut() {
                    ch.members.push(a.id.clone());
                }
                self.team_sel = Some(a.id.clone());
                data.agents.push(a);
                refresh = true;
            }
            if data.agents != agents_before {
                shared.dirty.store(true, Ordering::Relaxed);
            }
        });
        if refresh {
            let s = shared.clone();
            std::thread::spawn(move || heartbeat::refresh_health(&s));
        }
        if done || resp.should_close() {
            self.show_team = false;
        }
    }
}

// ------------------------------------------------------------------ status lines

/// Like agent_status, but knows that a Claude/Codex role can run on the other engine.
fn role_status(a: &Agent, all: &HashMap<String, Health>, t: &Theme) -> (String, Color32, String) {
    let own = agent_status(a, all.get(&probe::key(a)), t);
    let down = all.get(&probe::key(a)).and_then(|h| h.why_unavailable());
    if let (Some(why), true) = (down, a.kind != AgentKind::Custom) {
        // Same order the orchestrator uses for stand-ins.
        let alt = model::ENGINES.into_iter().filter(|k| *k != a.kind).find(|k| all.get(k.label()).is_some_and(|h| h.available()));
        let Some(alt) = alt else { return own };
        let mut b = a.clone();
        b.kind = alt;
        b.model.clear();
        if let Some(h) = all.get(alt.label()) {
            let (line, _, _) = agent_status(&b, Some(h), t);
            return (
                format!("on {} · {line}", alt.label()),
                t.amber,
                format!("{} is {why}, so this agent runs on {} until it's back.\n{}", a.kind.label(), alt.label(), own.2),
            );
        }
    }
    own
}

/// One line about an agent's engine state, a color, and a tooltip.
fn agent_status(a: &Agent, h: Option<&Health>, t: &Theme) -> (String, Color32, String) {
    let grey = t.secondary;
    let Some(h) = h else {
        return if a.kind == AgentKind::Custom { ("Custom command".into(), grey, a.command.clone()) } else { ("Checking…".into(), grey, String::new()) };
    };
    if !h.installed {
        return ("Not installed".into(), t.red, format!("Install {} and click refresh.", a.kind.label()));
    }
    if h.logged_in == Some(false) {
        let fix = match a.kind {
            AgentKind::Claude => "Open Terminal, run `claude`, type /login, then click refresh.",
            AgentKind::Codex => "Open Terminal, run `codex login`, then click refresh.",
            AgentKind::Local | AgentKind::Custom => "",
        };
        return (format!("{} · not logged in", a.kind.label()), t.red, fix.into());
    }
    let model = probe::pick_model(a, Some(h));
    let model_name = model
        .as_ref()
        .map(|m| h.models.iter().find(|x| &x.id == m).map(|x| x.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| m.clone()))
        .or_else(|| h.models.iter().find(|m| m.is_default).map(|m| m.name.clone()))
        .unwrap_or_else(|| "default model".into());
    let mut tip = format!("Model: {model_name} (picked automatically unless set in Team settings)");
    if let Some(p) = &h.plan {
        tip.push_str(&format!("\nPlan: {p}"));
    }
    for l in &h.limits {
        tip.push_str(&format!("\n{}: {:.0}% used{}", l.label, l.used_pct, l.resets_at.map(|t| format!(", resets {}", probe::when(t))).unwrap_or_default()));
    }
    if h.limits.is_empty() && a.kind == AgentKind::Claude {
        tip.push_str("\nUsage limits show up after the first reply.");
    }
    if h.blocked() {
        let back = h.blocked_until.map(|u| format!(" · back {}", probe::when(u))).unwrap_or_default();
        return (format!("{} limit reached{back}", a.kind.label()), t.red, tip);
    }
    let mut line = model_name;
    if a.kind == AgentKind::Local {
        line.push_str(" · on this Mac");
    }
    if let Some(l) = h.busiest() {
        line.push_str(&format!(" · {} {:.0}%", l.label, l.used_pct));
    }
    let col = if h.busiest().is_some_and(|l| l.used_pct >= 85.0) { t.amber } else { grey };
    (line, col, tip)
}

// ------------------------------------------------------------------ conversation

fn message(ui: &mut egui::Ui, t: &Theme, data: &Data, m: &model::Message, prev: Option<&Author>) {
    let grouped = prev == Some(&m.author) && !matches!(m.author, Author::System | Author::Heartbeat);
    match &m.author {
        Author::System => {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                let col = if m.error { t.red } else { t.tertiary };
                ui.add(egui::Label::new(RichText::new(strip_md(&m.text)).size(12.0).color(col)).wrap());
            });
            ui.add_space(6.0);
        }
        Author::User => {
            ui.add_space(if grouped { 3.0 } else { 14.0 });
            ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                let max = ui.available_width() * 0.72;
                Frame::new().fill(t.accent).corner_radius(17).inner_margin(Margin::symmetric(13, 8)).show(ui, |ui| {
                    ui.set_max_width(max);
                    ui.add(egui::Label::new(RichText::new(&m.text).size(14.0).color(Color32::WHITE)).wrap());
                });
            });
            if !m.attachments.is_empty() {
                ui.add_space(4.0);
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                    for a in m.attachments.iter().rev() {
                        let img = egui::Image::new(format!("file://{a}")).max_size(vec2(220.0, 160.0)).maintain_aspect_ratio(true).corner_radius(12);
                        if ui.add(img.sense(Sense::click())).on_hover_text("Click to open").clicked() {
                            open_path(std::path::Path::new(a));
                        }
                    }
                });
            }
        }
        Author::Heartbeat => {
            ui.add_space(12.0);
            Frame::new()
                .fill(t.teal.gamma_multiply(if t.dark { 0.10 } else { 0.07 }))
                .corner_radius(10)
                .inner_margin(Margin { left: 14, right: 14, top: 10, bottom: 10 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.5, t.teal);
                        ui.label(RichText::new("Files changed").size(12.0).color(t.teal));
                    });
                    // First line is "**Heartbeat:** project files changed <who>"; show just the who.
                    let body = match m.text.split_once('\n') {
                        Some((head, rest)) => {
                            let who = head.split("changed").nth(1).unwrap_or("").trim();
                            if who.is_empty() { rest.to_string() } else { format!("{}\n{rest}", capitalize(who)) }
                        }
                        None => m.text.clone(),
                    };
                    render_markdown(ui, t, &body, 13.0);
                });
        }
        Author::Agent(id) => {
            let agent = data.agent(id);
            if !grouped {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let (name, col) = match agent {
                        Some(a) => (a.name().to_string(), color(a.color)),
                        None => ("removed".into(), t.tertiary),
                    };
                    let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                    icons::avatar(ui.painter(), r.center(), 9.5, col, name.chars().next().unwrap_or('?'));
                    ui.label(RichText::new(&name).size(13.0).color(col));
                    let via = if m.via.is_empty() { agent.map(|a| a.kind.label().to_string()).unwrap_or_default() } else { m.via.clone() };
                    ui.label(RichText::new(via).size(11.0).color(t.tertiary));
                    if m.to_user && icon_button(ui, t, Icon::Speaker, 20.0, false, "Read aloud").clicked() {
                        let idx = data.agents.iter().position(|a| &a.id == id).unwrap_or(0);
                        let v = agent.filter(|a| !a.voice.is_empty()).map(|a| a.voice.clone()).unwrap_or_else(|| voice::auto_voice(idx));
                        voice::stop_speaking();
                        voice::speak(&m.text, &v);
                    }
                });
            } else {
                ui.add_space(8.0);
            }
            ui.horizontal_top(|ui| {
                ui.add_space(28.0);
                ui.vertical(|ui| {
                    if m.error {
                        Frame::new()
                            .fill(t.red.gamma_multiply(if t.dark { 0.12 } else { 0.07 }))
                            .corner_radius(8)
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| render_markdown(ui, t, &m.text, 13.5));
                    } else {
                        render_markdown(ui, t, &m.text, 14.0);
                    }
                });
            });
        }
    }
}

/// Everyone who's working right now (handoffs run in parallel), with what they're doing.
fn working(ui: &mut egui::Ui, t: &Theme, data: &Data, list: &[(String, String)], secs: u64, queued: usize) {
    ui.add_space(16.0);
    let starting = [(String::new(), "getting started".to_string())];
    let list = if list.is_empty() { &starting[..] } else { list };
    for (i, (agent, status)) in list.iter().enumerate() {
        ui.horizontal(|ui| {
            let a = data.agents.iter().find(|a| &a.handle == agent);
            let agent = &a.map(|a| a.name().to_string()).unwrap_or_else(|| agent.clone());
            let col = a.map(|a| color(a.color)).unwrap_or(t.tertiary);
            let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
            icons::avatar(ui.painter(), r.center(), 9.5, col, agent.chars().next().unwrap_or('•'));
            let (d, _) = ui.allocate_exact_size(vec2(26.0, 20.0), Sense::hover());
            icons::typing_dots(ui.painter(), d.left_center() + vec2(4.0, 0.0), t.secondary, ui.input(|i| i.time) + i as f64 * 0.3);
            let tail = if i == list.len() - 1 {
                format!(" · {secs}s{}", if queued > 0 { format!(" · {queued} more queued") } else { String::new() })
            } else {
                String::new()
            };
            let who = if agent.is_empty() { "The team".to_string() } else { agent.to_string() };
            ui.add(egui::Label::new(RichText::new(format!("{who} is {status}{tail}")).size(12.0).color(t.secondary)).truncate());
        });
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn strip_md(s: &str) -> String {
    s.replace("**", "").replace('`', "")
}

/// Small Markdown subset: code fences, headings, bullets, quotes, **bold**, `code`.
fn render_markdown(ui: &mut egui::Ui, t: &Theme, text: &str, size: f32) {
    let width = ui.available_width() - 4.0;
    let body = TextFormat { font_id: FontId::proportional(size), color: t.text, line_height: Some(size * 1.42), ..Default::default() };
    let mut job = LayoutJob::default();
    let mut code = String::new();
    let mut in_code = false;

    let flush_job = |ui: &mut egui::Ui, job: &mut LayoutJob| {
        if !job.text.trim().is_empty() {
            let mut j = std::mem::take(job);
            while j.text.ends_with('\n') {
                j.text.pop();
            }
            // Drop/clamp sections that pointed into the trimmed newlines.
            let len = j.text.len();
            j.sections.retain(|s| s.byte_range.start < len);
            for s in j.sections.iter_mut() {
                s.byte_range.end = s.byte_range.end.min(len);
            }
            j.wrap.max_width = width;
            ui.add(egui::Label::new(j).selectable(true).wrap());
        }
        *job = LayoutJob::default();
    };

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                ui.add_space(2.0);
                Frame::new().fill(t.code_bg).corner_radius(8).inner_margin(Margin::symmetric(12, 9)).show(ui, |ui| {
                    ui.set_width(width - 24.0);
                    ui.add(egui::Label::new(RichText::new(code.trim_end()).font(FontId::monospace(12.0)).color(t.text)).selectable(true).wrap());
                });
                ui.add_space(2.0);
                code.clear();
            } else {
                flush_job(ui, &mut job);
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(h) = trimmed.strip_prefix("### ").or(trimmed.strip_prefix("## ")).or(trimmed.strip_prefix("# ")) {
            let f = TextFormat { font_id: FontId::proportional(size + 2.5), color: t.strong, ..body.clone() };
            inline(&mut job, h, &f, t);
        } else if let Some(b) = trimmed.strip_prefix("- ").or(trimmed.strip_prefix("* ")) {
            let indent = "    ".repeat((line.len() - trimmed.len()) / 2);
            job.append(&format!("{indent}  •  "), 0.0, TextFormat { color: t.secondary, ..body.clone() });
            inline(&mut job, b, &body, t);
        } else if let Some(q) = trimmed.strip_prefix("> ") {
            job.append("    ", 0.0, body.clone());
            inline(&mut job, q, &TextFormat { color: t.secondary, ..body.clone() }, t);
        } else {
            inline(&mut job, line, &body, t);
        }
        job.append("\n", 0.0, body.clone());
    }
    if in_code && !code.is_empty() {
        job.append(&code, 0.0, TextFormat { font_id: FontId::monospace(12.0), ..body.clone() });
    }
    flush_job(ui, &mut job);
}

/// "[label](url)" -> "label" (links aren't clickable in this renderer, but shouldn't look like code).
fn strip_links(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(mid) = rest[open..].find("](").map(|m| open + m) else { break };
        let Some(close) = rest[mid..].find(')').map(|c| mid + c) else { break };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..mid]);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

fn inline(job: &mut LayoutJob, line: &str, base: &TextFormat, t: &Theme) {
    let line = strip_links(line);
    let line = line.as_str();
    let strong = TextFormat { color: t.strong, ..base.clone() };
    let code = TextFormat { font_id: FontId::monospace(base.font_id.size * 0.88), background: t.code_bg, color: t.strong, ..base.clone() };
    let mut rest = line;
    let mut bold = false;
    while !rest.is_empty() {
        let next_tick = rest.find('`');
        let next_bold = rest.find("**");
        match (next_tick, next_bold) {
            (Some(i), b) if b.is_none_or(|b| i < b) => {
                let fmt = if bold { &strong } else { base };
                job.append(&rest[..i], 0.0, fmt.clone());
                let after = &rest[i + 1..];
                match after.find('`') {
                    Some(end) => {
                        job.append(&after[..end], 0.0, code.clone());
                        rest = &after[end + 1..];
                    }
                    None => {
                        job.append(&rest[i..], 0.0, fmt.clone());
                        rest = "";
                    }
                }
            }
            (_, Some(b)) => {
                job.append(&rest[..b], 0.0, if bold { strong.clone() } else { base.clone() });
                bold = !bold;
                rest = &rest[b + 2..];
            }
            _ => {
                job.append(rest, 0.0, if bold { strong.clone() } else { base.clone() });
                rest = "";
            }
        }
    }
}

// ------------------------------------------------------------------ voice commands

enum VoiceCmd {
    Ignore,
    Stop,
    Quiet,
    StopListening,
    Switch(String),
}

/// Things you say to the app itself rather than to the agents.
fn voice_command(text: &str, data: &Data) -> Option<VoiceCmd> {
    let t: String = text.to_lowercase().chars().filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '-').collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    match t.as_str() {
        "" | "um" | "uh" | "hmm" | "hm" | "mm" | "mhm" | "ah" | "oh" | "er" | "erm" => return Some(VoiceCmd::Ignore),
        "stop" | "stop it" | "stop everything" | "stop all" | "cancel" | "cancel that" | "abort" => return Some(VoiceCmd::Stop),
        "quiet" | "be quiet" | "stop talking" | "shush" | "silence" | "shut up" | "enough" => return Some(VoiceCmd::Quiet),
        "stop listening" | "pause listening" | "mute" | "go to sleep" => return Some(VoiceCmd::StopListening),
        _ => {}
    }
    for prefix in ["switch to ", "go to ", "open ", "change to ", "move to "] {
        if let Some(rest) = t.strip_prefix(prefix) {
            let rest = rest.trim_start_matches("the ").trim_start_matches("channel ").trim_end_matches(" channel");
            let want = slug(rest);
            let found = data.channels.iter().find(|c| c.name == want || c.name.replace('-', "") == want.replace('-', ""));
            if let Some(c) = found {
                return Some(VoiceCmd::Switch(c.id.clone()));
            }
        }
    }
    None
}

// ------------------------------------------------------------------ helpers

fn sys(text: &str) -> model::Message {
    model::Message::system(text)
}

fn color(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

fn slug(s: &str) -> String {
    let s: String = s
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
}

/// `~/Development/a/very/long/path` -> `…/long/path` when it's too long for the header.
fn short_path(p: &str) -> String {
    let full = pretty_path(p);
    if full.chars().count() <= 40 {
        return full;
    }
    let parts: Vec<&str> = full.split('/').filter(|s| !s.is_empty()).collect();
    format!("…/{}", parts[parts.len().saturating_sub(2)..].join("/"))
}

fn pretty_path(p: &str) -> String {
    let home = model::home_dir();
    match p.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => p.to_string(),
    }
}

/// Dev aid: YH_SCREENSHOT=/path/out.tga saves one frame of the window, then quits.
fn debug_screenshot(ctx: &egui::Context) {
    let Some(path) = std::env::var_os("YH_SCREENSHOT") else { return };
    let delay: f64 = std::env::var("YH_SCREENSHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if ctx.input(|i| i.time) >= delay && !ASKED.swap(true, Ordering::Relaxed) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
    }
    ctx.request_repaint();
    let shot = ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    if let Some(img) = shot {
        let (w, h) = (img.size[0] as u16, img.size[1] as u16);
        let mut out = vec![0u8, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&[32, 0x28]);
        for p in &img.pixels {
            out.extend_from_slice(&[p.b(), p.g(), p.r(), 255]);
        }
        let _ = std::fs::write(path, out);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_dictation_into_text_box() {
        // Typed text stays; the dictated part updates in place as recognition refines it.
        let mut live = LiveText::start("Hi team,");
        let mut input = live.update("Hi team,", "please");
        assert_eq!(input, "Hi team, please");
        input = live.update(&input, "please run the test");
        assert_eq!(input, "Hi team, please run the test");
        input = live.update(&input, "Please run the tests.");
        assert_eq!(input, "Hi team, Please run the tests.");
        // Final transcript: no duplication.
        input = live.update(&input, "Please run the tests.");
        assert_eq!(input, "Hi team, Please run the tests.");
        assert!(!live.edited());

        // You fix a word mid-sentence: your edit is kept, only newer words are added.
        let mut live = LiveText::start("");
        let input = live.update("", "write the blog");
        assert_eq!(input, "write the blog");
        let edited = "write the blog post".to_string();
        let input = live.update(&edited, "write the blog about voice");
        assert_eq!(input, "write the blog post about voice");
        assert!(live.edited());
        let input = live.update(&input, "write the blog about voice agents");
        assert_eq!(input, "write the blog post about voice agents");
    }

    #[test]
    fn renders_text_with_trailing_blank_lines() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let t = Theme::dark();
                render_markdown(ui, &t, "# YesHarness launch\n\n\n", 13.5);
                render_file(ui, &t, "# Tasks\n\n- [ ] one\n\n\n## Done\n- [x] two\n\n");
            });
        });
    }

    #[test]
    fn attachments_are_copied_into_the_project() {
        let proj = std::env::temp_dir().join(format!("yh-att-{}", model::new_id()));
        std::fs::create_dir_all(&proj).unwrap();
        let src = std::env::temp_dir().join(format!("shot-{}.png", model::new_id()));
        std::fs::write(&src, b"png").unwrap();
        let stored = store_attachments(proj.to_str().unwrap(), std::slice::from_ref(&src));
        assert_eq!(stored.len(), 1);
        assert!(stored[0].starts_with(proj.join(".yesharness/attachments").to_str().unwrap()), "{stored:?}");
        assert!(std::path::Path::new(&stored[0]).is_file());
        assert!(std::fs::read_to_string(proj.join(".yesharness/.gitignore")).unwrap().contains('*'));
        assert!(is_image(&src) && !is_image(std::path::Path::new("notes.txt")));
        let _ = std::fs::remove_dir_all(proj);
        let _ = std::fs::remove_file(src);
    }

    #[test]
    fn task_lists_and_links() {
        assert_eq!(task_line("- [ ] write README"), Some((false, "write README")));
        assert_eq!(task_line("* [x] ship it"), Some((true, "ship it")));
        assert_eq!(task_line("- plain bullet"), None);
        assert_eq!(task_summary("# Tasks\n- [ ] a\n- [x] b\n```\n- [ ] not a task\n```\n- [ ] c"), Some("2 open · 1 done".into()));
        assert_eq!(strip_links("See [the docs](docs/a.md) and [x](y)."), "See the docs and x.");

        let dir = std::env::temp_dir().join(format!("yh-view-{}", model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("TASKS.md");
        std::fs::write(&p, "# Tasks\n- [ ] one\n  - [x] two\n").unwrap();
        let mut f = FileView::open(p.clone());
        f.toggle_task(1);
        f.toggle_task(2);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "# Tasks\n- [x] one\n  - [ ] two\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn voice_commands() {
        let mut data = Data::default();
        data.channels[0].name = "my-blog".into();
        assert!(matches!(voice_command("Stop.", &data), Some(VoiceCmd::Stop)));
        assert!(matches!(voice_command("Be quiet!", &data), Some(VoiceCmd::Quiet)));
        assert!(matches!(voice_command("Um.", &data), Some(VoiceCmd::Ignore)));
        assert!(matches!(voice_command("Switch to the my blog channel.", &data), Some(VoiceCmd::Switch(_))));
        assert!(voice_command("Stop the server and write a blog post", &data).is_none());
        assert!(voice_command("Go to the store", &data).is_none());
    }
}
