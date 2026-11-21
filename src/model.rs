//! Data model + persistence. Everything lives in one JSON file in the OS data dir.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Debug)]
pub enum AgentKind {
    Claude,
    Codex,
    /// A model on this Mac via Ollama, driven by Codex's local (--oss) mode, so it can still use tools.
    Local,
    /// Any shell command. The prompt is sent on stdin, stdout is the reply.
    Custom,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude Code",
            AgentKind::Codex => "Codex",
            AgentKind::Local => "Local",
            AgentKind::Custom => "Custom command",
        }
    }
    pub fn program(self) -> Option<&'static str> {
        match self {
            AgentKind::Claude => Some("claude"),
            AgentKind::Codex => Some("codex"),
            AgentKind::Local => Some("ollama"),
            AgentKind::Custom => None,
        }
    }
    pub fn supports_sessions(self) -> bool {
        !matches!(self, AgentKind::Custom)
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Agent {
    pub id: String,
    /// What you type after @ to talk to it.
    pub handle: String,
    /// Human-readable name, separate from the stable @mention handle.
    #[serde(default)]
    pub display_name: String,
    pub kind: AgentKind,
    /// Empty = the CLI's own default.
    #[serde(default)]
    pub model: String,
    /// Let the agent run commands / edit without sandbox restrictions.
    #[serde(default)]
    pub full_access: bool,
    /// Only for Custom agents.
    #[serde(default)]
    pub command: String,
    /// One line: what this agent is for. The main agent uses it to pick who does what.
    #[serde(default)]
    pub description: String,
    /// The agent's standing instructions (its job description).
    #[serde(default)]
    pub briefing: String,
    /// macOS voice for reading its replies ("" = automatic, a different one per agent).
    #[serde(default)]
    pub voice: String,
    /// How hard it thinks: "low" (quick), "" (the engine's default) or "high" (deep).
    #[serde(default)]
    pub effort: String,
    /// Load your global Claude/Codex plugins and MCP servers. Off = lean and faster.
    #[serde(default)]
    pub use_my_plugins: bool,
    /// Internet inside the sandbox (curl, whois, package installs, APIs). File edits stay limited to the project.
    #[serde(default = "yes")]
    pub internet: bool,
    pub color: [u8; 3],
}

impl Agent {
    /// Most tool steps one turn may take. Every step re-sends the whole conversation, so long loops get
    /// expensive fast. The main agent should route, not investigate.
    pub fn step_cap(&self) -> usize {
        match self.handle.as_str() {
            "main" => 8,
            "general" => 15,
            _ => 30,
        }
    }

    pub fn name(&self) -> &str {
        if self.display_name.trim().is_empty() { &self.handle } else { &self.display_name }
    }

    /// Does `word` refer to this agent? Its handle ("main") or its name ("Alex", "alex").
    pub fn answers_to(&self, word: &str) -> bool {
        let w = word.trim().trim_start_matches('@');
        if w.is_empty() {
            return false;
        }
        let name = self.display_name.trim();
        self.handle.eq_ignore_ascii_case(w)
            || (!name.is_empty()
                && (name.eq_ignore_ascii_case(w)
                    || name.split_whitespace().next().is_some_and(|first| first.eq_ignore_ascii_case(w))
                    || name.replace(' ', "").eq_ignore_ascii_case(w)))
    }

    /// "Alex (@main)" when the name differs from the handle, else "@developer".
    pub fn label(&self) -> String {
        if self.display_name.trim().is_empty() || self.display_name.trim().eq_ignore_ascii_case(&self.handle) {
            format!("@{}", self.handle)
        } else {
            format!("{} (@{})", self.display_name.trim(), self.handle)
        }
    }
}

/// Upgrade existing state without changing ids, handles, memberships, or sessions.
fn migrate_agent_names(data: &mut Data) -> bool {
    let mut changed = false;
    let mut rebriefed: Vec<String> = Vec::new();
    for agent in &mut data.agents {
        // Main agents still on the default briefing get the newer, more concise one.
        if agent.handle == "main" && agent.briefing.starts_with("You are the user's main agent, like a chief of staff") && !agent.briefing.contains("as concise as possible") {
            agent.briefing = TEMPLATES[0].agent(agent.color).briefing;
            rebriefed.push(agent.id.clone());
            changed = true;
        }
        if agent.handle == "main" && agent.display_name.is_empty() {
            agent.display_name = "Alex".into();
            changed = true;
        }
    }
    // A new briefing only reaches the agent in a fresh session.
    for ch in data.channels.iter_mut() {
        ch.sessions.retain(|k, _| !rebriefed.iter().any(|id| k.starts_with(&format!("{id}:"))));
        for id in &rebriefed {
            ch.seen.remove(id);
        }
    }
    changed
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub enum Author {
    User,
    Agent(String),
    System,
    /// The minute-by-minute project file watcher.
    Heartbeat,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Message {
    pub author: Author,
    pub text: String,
    pub ts: u64,
    #[serde(default)]
    pub error: bool,
    /// An agent answering you (not agent-to-agent chatter): gets read aloud.
    #[serde(default)]
    pub to_user: bool,
    /// Which engine/model actually produced it, e.g. "Codex · gpt-6-astra".
    #[serde(default)]
    pub via: String,
    /// Images you attached (absolute paths, copied into the project's .yesharness/attachments).
    #[serde(default)]
    pub attachments: Vec<String>,
    /// What the agent's turn actually did ("ran 2 commands · edited ui.rs · handed off to developer").
    #[serde(default)]
    pub receipt: String,
    /// It said work was happening, but nothing was run, edited or handed off.
    #[serde(default)]
    pub idle_claim: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub folder: String,
    /// Agent ids. The first one is the lead: it gets every message that doesn't @mention someone.
    pub members: Vec<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    /// agent id -> CLI session/thread id, so each agent keeps its memory per channel.
    #[serde(default)]
    pub sessions: HashMap<String, String>,
    /// agent id -> number of channel messages that agent has already seen.
    #[serde(default)]
    pub seen: HashMap<String, usize>,
    /// Heartbeat: hashes of AGENTS.md / TASKS.md / docs files at the last check.
    #[serde(default)]
    pub watch: HashMap<String, u64>,
    /// Something new happened while you were in another channel.
    #[serde(default)]
    pub unread: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Settings {
    /// Push-to-talk sends as soon as you stop talking.
    #[serde(default = "yes")]
    pub voice_auto_send: bool,
    /// Speak agent replies with the system voice.
    #[serde(default = "yes")]
    pub read_aloud: bool,
    /// Always listening; every pause sends what you said. The default: like talking to people in an office.
    #[serde(default = "yes")]
    pub hands_free: bool,
    /// Show the text box (voice is the default).
    #[serde(default)]
    pub show_keyboard: bool,
    /// How fast replies are read aloud: 1.0 = normal (200 words per minute).
    #[serde(default = "default_speech_speed")]
    pub speech_speed: f32,
    /// "auto" (follow macOS), "light" or "dark".
    #[serde(default = "auto_theme")]
    pub theme: String,
    /// Speech recognition: "apple" (built in) or "parakeet" (Parakeet Ultra, downloaded once).
    #[serde(default = "default_engine")]
    pub stt_engine: String,
}

/// Parakeet Ultra by default: clearly more accurate. Apple's recognizer is used until it's set up.
fn default_engine() -> String {
    "parakeet".into()
}

fn auto_theme() -> String {
    "auto".into()
}

pub fn default_speech_speed() -> f32 {
    1.25
}

/// The steps of the speed control.
pub const SPEECH_SPEEDS: [f32; 5] = [1.0, 1.25, 1.5, 1.75, 2.0];

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings { voice_auto_send: true, read_aloud: cfg!(target_os = "macos"), hands_free: cfg!(target_os = "macos"), show_keyboard: !cfg!(target_os = "macos"), speech_speed: default_speech_speed(), theme: auto_theme(), stt_engine: default_engine() }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Data {
    pub agents: Vec<Agent>,
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub selected: Option<String>,
    #[serde(default)]
    pub settings: Settings,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Message { author: Author::System, text: text.into(), ts: now(), error: false, to_user: false, via: String::new(), attachments: Vec::new(), receipt: String::new(), idle_claim: false }
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn new_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{:x}{:x}", nanos, N.fetch_add(1, Ordering::Relaxed))
}

pub const PALETTE: [[u8; 3]; 6] = [
    [217, 119, 87],  // claude orange
    [16, 163, 127],  // codex green
    [99, 132, 255],  // blue
    [190, 100, 220], // purple
    [230, 180, 40],  // yellow
    [80, 190, 210],  // teal
];

pub fn home_dir() -> String {
    dirs::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| ".".into())
}

/// ~/YesHarness: the folder for #general (never the whole home folder: agents would search everything,
/// which is slow and burns quota). Created on demand.
pub fn general_dir() -> String {
    let dir = format!("{}/YesHarness", home_dir());
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Where the New Channel sheet starts: ~/Development if you have one, else ~/YesHarness.
pub fn projects_dir() -> String {
    let dev = format!("{}/Development", home_dir());
    if std::path::Path::new(&dev).is_dir() { dev } else { general_dir() }
}

/// Starter roles. Each is a briefing on top of an engine; you can switch the engine or model any time.
pub struct Template {
    pub title: &'static str,
    pub handle: &'static str,
    pub kind: AgentKind,
    pub role: &'static str,
    pub briefing: &'static str,
}

pub const TEMPLATES: &[Template] = &[
    Template {
        title: "Main agent (coordinator)",
        handle: "main",
        kind: AgentKind::Claude,
        role: "Main agent: talks with the user, plans the work and hands it to the right specialist.",
        briefing: "You are the user's main agent, like a chief of staff in a small office. The user talks to you, \
                   mostly by voice, and you get things done with the team.\n\
                   - Work out what they actually want. Make sensible assumptions; ask only when you really can't.\n\
                   - Do quick things yourself: answers, small edits, looking things up in the project.\n\
                   - Hand real work to the specialist whose role fits, with a self-contained brief: the goal, where \
                   files live, and what done looks like. Split a request into its parts and give each part to the \
                   specialist whose role matches it (e.g. the blog post to the writer, the posts for X to the social \
                   media agent). Brief all of them in the same message so they can work in parallel.\n\
                   - When they report back, check the result, then tell the user in one or two spoken sentences what \
                   happened and where to find it.\n\
                   - Keep TASKS.md current.\n\
                   - Be as concise as possible. Usually one or two short sentences. No preamble, no repeating what the \
                   user said, no explaining things they didn't ask about. Only go into detail when asked.",
    },
    Template {
        title: "Developer",
        handle: "developer",
        kind: AgentKind::Codex,
        role: "Software developer: writes, runs and fixes code, runs tests and builds.",
        briefing: "You are the team's software developer. You write, run and fix code in the project folder, following \
                   AGENTS.md. Make focused changes, run the relevant tests or build, and report which files you changed \
                   and the result. If something is ambiguous, pick the simplest reasonable option and say so.",
    },
    Template {
        title: "Blog post writer",
        handle: "writer",
        kind: AgentKind::Claude,
        role: "Blog post writer: researches, outlines and writes blog posts and articles in Markdown.",
        briefing: "You are the team's blog post writer. You write clear, engaging, well-structured posts in Markdown. \
                   Unless told otherwise, save drafts in docs/blog/ as YYYY-MM-DD-slug.md. Each draft has a title, a \
                   one-line summary, headings, and a short call to action. Match the voice described in AGENTS.md or \
                   used in earlier posts. Report the file path and a two-sentence summary.",
    },
    Template {
        title: "Social media manager",
        handle: "social",
        kind: AgentKind::Codex,
        role: "Social media manager: turns ideas, releases and blog posts into posts for X, LinkedIn and others.",
        briefing: "You are the team's social media manager. You turn ideas, releases and blog posts into short posts \
                   for X, LinkedIn and other platforms: a punchy hook, the right length for each platform, a few relevant \
                   hashtags, no fluff. Save drafts in docs/social/ (one file per campaign, one section per platform). \
                   Only draft; never publish anything yourself. Report what you drafted and where.",
    },
    Template {
        title: "Researcher",
        handle: "researcher",
        kind: AgentKind::Claude,
        role: "Researcher: finds and checks facts, competitors and sources; writes short research notes.",
        briefing: "You are the team's researcher. You find and verify information (the web when available, plus the \
                   project's files) and write concise findings with sources to docs/research/. Keep facts separate from \
                   opinions, and say how confident you are.",
    },
    Template {
        title: "Reviewer / editor",
        handle: "reviewer",
        kind: AgentKind::Codex,
        role: "Reviewer and editor: checks code or writing for mistakes and clarity, and suggests concrete fixes.",
        briefing: "You are the team's reviewer and editor. You review code or writing for correctness, clarity and \
                   consistency with AGENTS.md, and give concrete, prioritized fixes. Make small fixes yourself when \
                   they're obviously right; list the rest.",
    },
    Template {
        title: "General helper",
        handle: "general",
        kind: AgentKind::Codex,
        role: "General helper: steps in for anything (questions, research, quick fixes, small tasks) when the others are busy.",
        briefing: "You are the team's general helper. You step in when the main agent or a specialist is busy, so the \
                   user never has to wait. Handle whatever comes: answer questions, look things up, check things, make \
                   small changes, or take over a task that was meant for a busy teammate. Do the work yourself with your \
                   tools and report briefly: one or two sentences, plus where to find results.",
    },
];

impl Template {
    pub fn agent(&self, color: [u8; 3]) -> Agent {
        Agent {
            id: new_id(),
            handle: self.handle.into(),
            display_name: if self.handle == "main" { "Alex".into() } else { String::new() },
            kind: self.kind,
            model: String::new(),
            full_access: false,
            command: String::new(),
            description: self.role.into(),
            briefing: self.briefing.split_whitespace().collect::<Vec<_>>().join(" ").replace(" - ", "\n- "),
            voice: String::new(),
            // The main agent mostly routes and summarizes: quick thinking keeps the conversation snappy.
            effort: if self.handle == "main" { "low".into() } else { String::new() },
            use_my_plugins: false,
            internet: true,
            color,
        }
    }
}

impl Default for Data {
    fn default() -> Self {
        // A small office: the main agent you talk to, plus specialists it can hand work to.
        let agents: Vec<Agent> = TEMPLATES[..4].iter().enumerate().map(|(i, t)| t.agent(PALETTE[i % PALETTE.len()])).collect();
        let general = Channel {
            id: new_id(),
            name: "general".into(),
            folder: general_dir(),
            members: agents.iter().map(|a| a.id.clone()).collect(),
            messages: vec![Message::system(
                "Welcome to your workspace. Send a message to your main agent; it brings in the right teammates. \
                 Create a channel for each project and choose its folder. Use Manage team to set up your agents.",
            )],
            sessions: HashMap::new(),
            seen: HashMap::new(),
            watch: HashMap::new(),
            unread: false,
        };
        Data { selected: Some(general.id.clone()), agents, channels: vec![general], settings: Settings::default() }
    }
}

impl Data {
    pub fn agent(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }
    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }
    pub fn channel_mut(&mut self, id: &str) -> Option<&mut Channel> {
        self.channels.iter_mut().find(|c| c.id == id)
    }
    /// Members of a channel that still exist, lead first.
    pub fn members(&self, ch: &Channel) -> Vec<Agent> {
        ch.members.iter().filter_map(|id| self.agent(id).cloned()).collect()
    }
}

pub fn data_file() -> PathBuf {
    if let Some(p) = std::env::var_os("YH_DATA_FILE") {
        return PathBuf::from(p);
    }
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("YesHarness").join("state.json")
}

/// Engines in fallback order: when a role's own engine is down, the first available one of these runs it.
pub const ENGINES: [AgentKind; 3] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Local];

/// Codex's home for local mode: clean (no MCP servers, plugins or skills), so prompts fit a local model's context.
pub fn local_codex_home() -> PathBuf {
    let dir = data_file().parent().map(|p| p.join("codex-local")).unwrap_or_else(|| PathBuf::from("codex-local"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn load() -> Data {
    let path = data_file();
    match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<Data>(&s) {
            Ok(mut d) => {
                if migrate_agent_names(&mut d) {
                    if let Err(e) = save(&d) {
                        eprintln!("could not persist agent display names: {e}");
                    }
                }
                d
            },
            Err(e) => {
                // Keep the broken file around instead of silently overwriting it.
                let _ = std::fs::copy(&path, path.with_extension("broken.json"));
                eprintln!("could not parse {}: {e}", path.display());
                Data::default()
            }
        },
        Err(_) => Data::default(),
    }
}

pub fn save(data: &Data) -> std::io::Result<()> {
    let path = data_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(data)?)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_state_gets_alex_without_changing_routing_or_sessions() {
        let mut legacy = serde_json::to_value(Data::default()).unwrap();
        for agent in legacy["agents"].as_array_mut().unwrap() {
            agent.as_object_mut().unwrap().remove("display_name");
        }
        let mut data: Data = serde_json::from_value(legacy).unwrap();
        let id = data.agents[0].id.clone();
        data.channels[0].sessions.insert(id.clone(), "existing-session".into());
        let before = serde_json::to_value(&data).unwrap();
        assert!(migrate_agent_names(&mut data));
        assert_eq!(data.agents[0].name(), "Alex");
        assert_eq!(data.agents[0].handle, "main");
        assert_eq!(data.agents[0].id, id);
        assert_eq!(crate::orchestrator::user_mentions("@main hello", &data.agents), vec![id]);
        let persisted = serde_json::to_string(&data).unwrap();
        let restored: Data = serde_json::from_str(&persisted).unwrap();
        assert_eq!(restored.agents[0].display_name, "Alex");
        assert_eq!(serde_json::to_value(&restored).unwrap()["channels"], before["channels"]);
        assert!(!migrate_agent_names(&mut data));
        data.agents[0].display_name = "Custom name".into();
        assert!(!migrate_agent_names(&mut data));
        assert_eq!(data.agents[0].name(), "Custom name");
        assert_eq!(data.agents[1].name(), "developer");
    }
}
