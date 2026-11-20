//! Talking to agents. On macOS this drives the bundled `yh-dictate` helper (Apple's on-device
//! SpeechAnalyzer: fast, private, nothing to install) and `say` for reading replies aloud.
//! Other platforms: not available yet (the text box still works).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// Listen until stop() (hold-to-talk / click-to-toggle), then deliver the whole thing.
    PushToTalk,
    /// Keep listening; every pause delivers an utterance.
    HandsFree,
}

#[derive(Default, Clone)]
pub struct State {
    /// What you're saying right now (partial).
    pub text: String,
    /// Mic level 0..1, for the animation.
    pub level: f32,
    pub live: bool,
    pub done: bool,
    pub error: Option<String>,
    pub info: Option<String>,
    /// Hands-free: finished utterances waiting to be sent.
    pub utterances: Vec<String>,
    /// Not listening right now because the app is talking (so it can't hear itself).
    pub muted: bool,
}

pub struct Dictation {
    child: Child,
    /// Shared with the mute thread; dropping it (stop) ends the helper.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pub mode: Mode,
    pub state: Arc<Mutex<State>>,
}

pub fn helper_path() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    // Inside the .app bundle it sits next to the main binary; during `cargo run` it's in target/release.
    [dir.join("yh-dictate"), dir.join("../release/yh-dictate")].into_iter().find(|p| p.is_file())
}

/// Make sure Apple's speech model is installed, so the first press is instant.
pub fn prepare() {
    if let Some(p) = helper_path() {
        std::thread::spawn(move || {
            let _ = Command::new(p).arg("--prepare").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        });
    }
}

// ------------------------------------------------------------------ Parakeet Ultra (optional engine)

/// Setting up Parakeet (download + first load), reported to the Settings sheet.
#[derive(Clone, Default)]
pub struct Setup {
    pub running: bool,
    pub status: String,
    pub error: Option<String>,
}

static SETUP: Mutex<Setup> = Mutex::new(Setup { running: false, status: String::new(), error: None });

fn parakeet_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("FluidAudio/Models/parakeet-ultra"))
}

/// Downloaded and loaded successfully at least once.
pub fn parakeet_ready() -> bool {
    !SETUP.lock().unwrap().running && parakeet_dir().is_some_and(|d| d.join(".yesharness-ready").is_file())
}

pub fn parakeet_setup() -> Setup {
    SETUP.lock().unwrap().clone()
}

/// Download and load Parakeet Ultra in the background (once). Safe to call repeatedly.
pub fn setup_parakeet(repaint: impl Fn() + Send + 'static) {
    {
        let mut s = SETUP.lock().unwrap();
        if s.running || parakeet_dir().is_some_and(|d| d.join(".yesharness-ready").is_file()) {
            return;
        }
        *s = Setup { running: true, status: "Setting up Parakeet Ultra (one time, ~0.7 GB)…".into(), error: None };
    }
    let Some(helper) = helper_path() else {
        *SETUP.lock().unwrap() = Setup { running: false, status: String::new(), error: Some("Voice helper not found.".into()) };
        return;
    };
    std::thread::spawn(move || {
        let child = Command::new(helper)
            .arg("--prepare")
            .env("YH_ENGINE", "parakeet")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut ok = false;
        let mut error = None;
        if let Ok(mut child) = child {
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if let Some(i) = line.strip_prefix("I:") {
                        if i == "ready" {
                            ok = true;
                        } else {
                            SETUP.lock().unwrap().status = i.replace("Downloading", "Setting up");
                        }
                    } else if let Some(e) = line.strip_prefix("E:") {
                        error = Some(e.to_string());
                    }
                    repaint();
                }
            }
            let _ = child.wait();
        } else {
            error = Some("Couldn't start the voice helper.".into());
        }
        if ok {
            if let Some(d) = parakeet_dir() {
                let _ = std::fs::write(d.join(".yesharness-ready"), "ok");
            }
        }
        *SETUP.lock().unwrap() = Setup {
            running: false,
            status: if ok { "Ready".into() } else { String::new() },
            error: if ok { None } else { Some(error.unwrap_or_else(|| "Setup didn't finish.".into())) },
        };
        repaint();
    });
}

impl Dictation {
    /// `words`: names to recognise better (agents, channels, files).
    /// `engine`: "apple" or "parakeet".
    pub fn start(mode: Mode, words: &[String], engine: &str, repaint: impl Fn() + Send + 'static) -> Result<Self, String> {
        let path = helper_path().ok_or("Voice helper not found. Build the app with scripts/bundle.sh.")?;
        let mut cmd = Command::new(path);
        cmd.env("YH_ENGINE", engine);
        if mode == Mode::HandsFree {
            cmd.arg("--hands-free");
        }
        let mut child = cmd
            .env("YH_WORDS", words.join(","))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Couldn't start voice input: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take().ok_or("no stdout")?;
        if mode == Mode::HandsFree {
            // While the app is talking, the helper ignores the mic: no echo loop.
            let pipe = stdin.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                let mut muted = false;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(40));
                    let want = echo_window();
                    let mut g = pipe.lock().unwrap();
                    let Some(w) = g.as_mut() else { break };
                    if want != muted {
                        if writeln!(w, "{}", if want { "MUTE" } else { "UNMUTE" }).and_then(|_| w.flush()).is_err() {
                            break;
                        }
                        muted = want;
                    }
                }
            });
        }
        let state = Arc::new(Mutex::new(State::default()));
        let st = state.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let (tag, rest) = line.split_at(line.find(':').map(|i| i + 1).unwrap_or(0));
                let mut s = st.lock().unwrap();
                match tag {
                    "L:" => s.level = rest.parse().unwrap_or(0.0),
                    "P:" => s.text = rest.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string(),
                    "U:" => {
                        // Drop recognizer noise like "..." around the words.
                        let u = rest.trim_matches(|c: char| !c.is_alphanumeric() && c != '?' && c != '!' && c != '.').trim_start_matches('.').trim();
                        if u.chars().any(|c| c.is_alphanumeric()) {
                            s.utterances.push(u.to_string());
                        }
                        s.text.clear();
                    }
                    "M:" => {
                        s.muted = rest == "1";
                        s.text.clear();
                    }
                    "F:" => {
                        s.text = rest.to_string();
                        s.done = true;
                    }
                    "R:" => {
                        s.live = true;
                        s.info = None;
                    }
                    "I:" => s.info = Some(rest.to_string()),
                    "E:" => {
                        s.error = Some(rest.to_string());
                        s.done = true;
                    }
                    _ => {}
                }
                drop(s);
                repaint();
            }
            st.lock().unwrap().done = true;
            repaint();
        });
        Ok(Dictation { child, stdin, mode, state })
    }

    /// Stop listening; the final text arrives shortly after (state.done).
    pub fn stop(&mut self) {
        self.stdin.lock().unwrap().take();
    }

    pub fn stopping(&self) -> bool {
        self.stdin.lock().unwrap().is_none()
    }
}

impl Drop for Dictation {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ------------------------------------------------------------------ reading replies aloud

use std::collections::VecDeque;

static QUEUE: Mutex<VecDeque<(String, String)>> = Mutex::new(VecDeque::new());
static SAY: Mutex<Option<Child>> = Mutex::new(None);
static LAST_SPOKE: Mutex<Option<Instant>> = Mutex::new(None);
static WORKER: std::sync::Once = std::sync::Once::new();
/// Words per minute for `say` (set from the speed setting).
static RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(300);

/// 1.0 = normal speed (200 words per minute).
pub fn set_speed(speed: f32) {
    let wpm = (200.0 * speed.clamp(0.5, 3.0)).round() as u32;
    RATE.store(wpm, std::sync::atomic::Ordering::Relaxed);
}

/// Queue `text` to be spoken in `voice` ("" = system voice). Agents take turns, like people.
pub fn speak(text: &str, voice: &str) {
    let t = speakable(text);
    if t.is_empty() || !cfg!(target_os = "macos") {
        return;
    }
    {
        let mut r = RECENT.lock().unwrap();
        r.push_back((Instant::now(), t.clone()));
        while r.len() > 20 {
            r.pop_front();
        }
    }
    QUEUE.lock().unwrap().push_back((t, voice.to_string()));
    WORKER.call_once(|| {
        std::thread::spawn(|| loop {
            let next = QUEUE.lock().unwrap().pop_front();
            let Some((text, voice)) = next else {
                std::thread::sleep(std::time::Duration::from_millis(60));
                continue;
            };
            let mut cmd = Command::new("say");
            if !voice.is_empty() {
                cmd.args(["-v", &voice]);
            }
            let rate = RATE.load(std::sync::atomic::Ordering::Relaxed).to_string();
            if let Ok(child) = cmd.args(["-r", &rate]).arg(&text).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                *SAY.lock().unwrap() = Some(child);
                loop {
                    *LAST_SPOKE.lock().unwrap() = Some(Instant::now());
                    let mut g = SAY.lock().unwrap();
                    match g.as_mut().map(|c| c.try_wait()) {
                        Some(Ok(None)) => {}
                        _ => {
                            *g = None;
                            break;
                        }
                    }
                    drop(g);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                *LAST_SPOKE.lock().unwrap() = Some(Instant::now());
            }
        });
    });
}

pub fn stop_speaking() {
    QUEUE.lock().unwrap().clear();
    if let Some(mut c) = SAY.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

pub fn speaking() -> bool {
    SAY.lock().unwrap().is_some() || !QUEUE.lock().unwrap().is_empty()
}

/// True while speaking and shortly after (room echo), so hands-free mode doesn't transcribe our own voice.
pub fn echo_window() -> bool {
    speaking() || LAST_SPOKE.lock().unwrap().is_some_and(|t| t.elapsed().as_millis() < 700)
}

/// What we said out loud recently, to catch any echo the mute missed (speakers, delays).
static RECENT: Mutex<VecDeque<(Instant, String)>> = Mutex::new(VecDeque::new());

fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(String::from).collect()
}

/// Is this "utterance" really our own voice coming back? True during speech, or when most of its
/// words are what an agent said in the last minute.
pub fn is_echo(utterance: &str) -> bool {
    if echo_window() {
        return true;
    }
    let heard = words(utterance);
    if heard.len() < 3 {
        return false;
    }
    let mut recent = RECENT.lock().unwrap();
    while recent.front().is_some_and(|(t, _)| t.elapsed().as_secs() > 60) {
        recent.pop_front();
    }
    recent.iter().any(|(_, said)| {
        let said = words(said);
        let hits = heard.iter().filter(|w| said.contains(w)).count();
        hits * 10 >= heard.len() * 7 // 70%+ of the words were ours
    })
}

#[cfg(test)]
fn remember_spoken(text: &str) {
    RECENT.lock().unwrap().push_back((Instant::now(), text.to_string()));
}

/// Installed system voices for your language, nicest first (no novelty voices). Cached; see refresh_voices.
pub fn voices() -> Vec<String> {
    static V: Mutex<Option<Vec<String>>> = Mutex::new(None);
    if REFRESH.swap(false, std::sync::atomic::Ordering::Relaxed) {
        *V.lock().unwrap() = None;
    }
    V.lock().unwrap().get_or_insert_with(list_voices).clone()
}

static REFRESH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Re-read the installed voices (e.g. after downloading new ones in System Settings).
pub fn refresh_voices() {
    REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The natural-sounding (Premium / Enhanced) voices you have installed.
pub fn natural_voices() -> Vec<String> {
    voices().into_iter().filter(|v| v.contains("Premium") || v.contains("Enhanced")).collect()
}

/// Opens System Settings where Apple's natural voices are downloaded (free).
pub fn open_voice_settings() {
    #[cfg(target_os = "macos")]
    {
        let ok = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.Accessibility-Settings.extension?SpokenContent")
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            let _ = Command::new("open").arg("x-apple.systempreferences:com.apple.Accessibility-Settings.extension").status();
        }
    }
}

fn list_voices() -> Vec<String> {
    {
        if !cfg!(target_os = "macos") {
            return vec![];
        }
        let lang = Command::new("defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("LANG").ok())
            .unwrap_or_else(|| "en".into());
        let lang = lang.split(['_', '-', '.']).next().unwrap_or("en").to_lowercase();
        let Ok(out) = Command::new("say").args(["-v", "?"]).output() else { return vec![] };
        const NOVELTY: [&str; 22] = [
            "Albert", "Bad News", "Bahh", "Bells", "Boing", "Bubbles", "Cellos", "Good News", "Jester", "Organ", "Superstar",
            "Trinoids", "Whisper", "Wobble", "Zarvox", "Junior", "Ralph", "Fred", "Kathy", "Grandma", "Grandpa", "Deranged",
        ];
        const NICE: [&str; 8] = ["Samantha", "Daniel", "Karen", "Moira", "Tessa", "Rishi", "Anna", "Thomas"];
        let mut v: Vec<(u8, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let left = l.split('#').next()?.trim_end();
                let (name, locale) = left.rsplit_once(char::is_whitespace)?;
                let name = name.trim().to_string();
                if !locale.to_lowercase().starts_with(&lang) || NOVELTY.iter().any(|n| name.starts_with(n)) {
                    return None;
                }
                let rank = if name.contains("Premium") || name.contains("Enhanced") {
                    0
                } else if NICE.iter().any(|n| name == *n) {
                    1
                } else {
                    2
                };
                Some((rank, name))
            })
            .collect();
        v.sort_by_key(|(r, _)| *r);
        v.into_iter().map(|(_, n)| n).collect()
    }
}

/// The voice for the n-th agent: the first (main) agent uses your system voice, the others each get their own.
pub fn auto_voice(n: usize) -> String {
    let v = voices();
    if n == 0 {
        // The main agent: the most natural voice you have, else your system voice (which can be a Siri voice).
        return natural_voices().into_iter().next().unwrap_or_default();
    }
    if v.is_empty() {
        String::new()
    } else {
        v[(n - 1) % v.len()].clone()
    }
}

/// The first couple of sentences, without code, handoff lines or Markdown symbols.
pub fn speakable(text: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || l.is_empty() || l.starts_with('@') || l.starts_with('|') {
            continue;
        }
        let l = l.trim_start_matches(['#', '-', '*', '>', ' ']);
        let mut clean = String::new();
        let mut chars = l.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' | '_' | '`' => {}
                // [label](url) -> label
                ']' if chars.peek() == Some(&'(') => {
                    for c in chars.by_ref() {
                        if c == ')' {
                            break;
                        }
                    }
                }
                '[' => {}
                c => clean.push(c),
            }
        }
        out.push_str(clean.trim());
        out.push(' ');
        if out.len() > 320 {
            break;
        }
    }
    let out = out.trim();
    if out.len() <= 320 {
        return out.to_string();
    }
    let mut end = 320;
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    // Cut at a sentence end.
    let head = &out[..end];
    let cut = head.rfind(['.', '!', '?']).map(|i| i + 1).unwrap_or_else(|| head.rfind(' ').unwrap_or(end));
    out[..cut].trim().to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn speakable_skips_code_and_handoffs() {
        let t = "Done! Tests **pass**.\n@codex: review it\n```\nlet x = 1;\n```\nSee [the docs](http://x).";
        assert_eq!(super::speakable(t), "Done! Tests pass. See the docs.");
    }

    #[test]
    fn catches_echo_of_our_own_speech() {
        super::remember_spoken("I pulled the current tasks. The only actionable item is adding live dictation to YesHarness.");
        // Speech recognition of our own voice, with typical mishearings.
        assert!(super::is_echo(". I pulled the current tasks. The only actionable item is adding lid dictation to yes harness"));
        // Things you'd actually say are not echoes.
        assert!(!super::is_echo("Write a blog post about voice agents"));
        assert!(!super::is_echo("Yes"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn finds_voices() {
        let v = super::voices();
        assert!(!v.is_empty());
        assert!(!v.iter().any(|n| n == "Bubbles"));
        assert_eq!(super::auto_voice(0), "");
        assert_ne!(super::auto_voice(1), super::auto_voice(2));
    }
}
