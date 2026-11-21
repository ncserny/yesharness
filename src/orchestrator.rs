//! How agents work together in a channel.
//!
//! 1. Your message goes to the lead agent (first available member), or to whoever you address:
//!    `@codex …`, or by voice "Codex, …" (`@all` / "everyone, …" = every agent).
//! 2. An agent can hand off work by writing a line `@other: <task>`. The app runs that
//!    agent, posts its reply in the channel, then gives the turn back to the delegator to
//!    check the work and answer you.
//! 3. Each agent resumes its own CLI session per channel, and is shown only the channel
//!    messages it hasn't seen yet, so everyone stays in sync without re-sending history.
//! 4. Models are picked automatically from what each CLI reports (see probe.rs). If an agent
//!    is logged out or out of quota, another agent takes over.

use crate::agents::{self, Ctl};
use crate::env;
use crate::model::{self, Agent, AgentKind, Author, Channel, Data, Message};
use crate::voice;
use crate::probe::{self, Failure, Health};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

/// Tokens one message may use across all agents before the app pauses it (you can say "continue").
const TOKEN_BUDGET: u64 = 800_000;

/// Max agent turns per user message, so agents can't ping-pong forever.
const MAX_TURNS: u32 = 10;
/// Max nesting of delegations (A asks B asks C ...).
const MAX_DEPTH: u32 = 2;
/// How many messages a fresh agent (no session yet) gets as context.
const HISTORY: usize = 30;
const MAX_MSG_CHARS: usize = 8000;
/// How many recent messages the router-mode prompt shows.
const ROUTER_HISTORY: usize = 12;

pub struct Run {
    pub cancel: Arc<AtomicBool>,
    /// One slot per agent turn in flight (handoffs run in parallel); Stop kills them all.
    pub pids: Arc<Mutex<Vec<Arc<Mutex<Option<u32>>>>>>,
    /// Who's working right now and what they're doing, in the order they started.
    pub working: Vec<(String, String)>,
    pub started: Instant,
    pub queue: VecDeque<String>,
    /// Messages being handled right now in this channel (they run side by side).
    pub active: usize,
    /// Each running turn's own stop flag (Stop sets them all; a follow-up correction sets one).
    pub turn_flags: Vec<Arc<AtomicBool>>,
}

/// The main agent answering one of your messages, so a quick follow-up ("I mean X") can restart it
/// with both messages instead of a helper answering the follow-up on its own.
pub struct LeadTurn {
    pub agent: String,
    /// Index of the user message it's answering.
    pub user_msg: usize,
    pub flag: Arc<AtomicBool>,
    pub pid: Arc<Mutex<Option<u32>>>,
}

/// Seconds within which a second message counts as a follow-up to the first.
const FOLLOW_UP_SECS: u64 = 45;

type Hook<T> = Box<dyn Fn(T) + Send + Sync>;

pub struct Shared {
    pub data: Mutex<Data>,
    pub runs: Mutex<HashMap<String, Run>>,
    pub dirty: AtomicBool,
    /// What each agent kind can do right now (login, models, limits).
    pub health: Mutex<HashMap<String, Health>>,
    /// Channel id -> agents that ran since the last heartbeat.
    pub activity: Mutex<HashMap<String, BTreeSet<String>>>,
    pub last_beat: Mutex<Option<Instant>>,
    /// Channel id -> what's in its project folder (Git/GitHub, AGENTS.md, TASKS.md, docs).
    pub project: Mutex<HashMap<String, crate::project::ProjectInfo>>,
    /// Is the app window focused? (Notifications only when it isn't.)
    pub app_focused: AtomicBool,
    /// Send () to run the heartbeat now.
    pub wake: (Mutex<mpsc::Sender<()>>, Arc<Mutex<mpsc::Receiver<()>>>),
    pub repaint: Box<dyn Fn() + Send + Sync>,
    /// (text, voice) to read aloud.
    pub speak: Hook<(String, String)>,
    pub notify: Hook<String>,
    /// Agent id -> stand-in engine we already told the user about.
    pub engine_notes: Mutex<HashMap<String, AgentKind>>,
    /// "{channel}:{agent}" -> held while that agent runs a turn there (an agent never runs two turns at once).
    pub agent_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Channel id -> the main agent's turn on your latest message (see LeadTurn).
    pub lead_turns: Mutex<HashMap<String, LeadTurn>>,
}

impl Shared {
    pub fn new(data: Data, repaint: Box<dyn Fn() + Send + Sync>, speak: Hook<(String, String)>, notify: Hook<String>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        Arc::new(Shared {
            data: Mutex::new(data),
            runs: Mutex::new(HashMap::new()),
            dirty: AtomicBool::new(false),
            health: Mutex::new(HashMap::new()),
            activity: Mutex::new(HashMap::new()),
            last_beat: Mutex::new(None),
            project: Mutex::new(HashMap::new()),
            app_focused: AtomicBool::new(true),
            wake: (Mutex::new(tx), Arc::new(Mutex::new(rx))),
            repaint,
            speak,
            notify,
            engine_notes: Mutex::new(HashMap::new()),
            agent_locks: Mutex::new(HashMap::new()),
            lead_turns: Mutex::new(HashMap::new()),
        })
    }

    pub fn quiet(data: Data) -> Arc<Self> {
        Shared::new(data, Box::new(|| {}), Box::new(|_: (String, String)| {}), Box::new(|_| {}))
    }

    pub fn post(&self, cid: &str, author: Author, text: String, error: bool) {
        if let Some(ch) = self.data.lock().unwrap().channel_mut(cid) {
            ch.messages.push(Message { author, text, ts: model::now(), error, to_user: false, via: String::new(), attachments: Vec::new(), receipt: String::new(), idle_claim: false });
        }
        self.dirty.store(true, Ordering::Relaxed);
        (self.repaint)();
    }

    fn set_status(&self, cid: &str, agent: &str, status: String) {
        if let Some(r) = self.runs.lock().unwrap().get_mut(cid) {
            match r.working.iter_mut().find(|(a, _)| a == agent) {
                Some(w) => w.1 = status,
                None => r.working.push((agent.to_string(), status)),
            }
        }
        (self.repaint)();
    }

    fn done_working(&self, cid: &str, agent: &str) {
        if let Some(r) = self.runs.lock().unwrap().get_mut(cid) {
            r.working.retain(|(a, _)| a != agent);
        }
        (self.repaint)();
    }

    fn cancelled(&self, cid: &str) -> bool {
        self.runs.lock().unwrap().get(cid).map(|r| r.cancel.load(Ordering::Relaxed)).unwrap_or(true)
    }

    /// Re-read a channel's project folder (cheap: a few small files).
    pub fn refresh_project(&self, cid: &str) {
        let folder = self.data.lock().unwrap().channel(cid).map(|c| c.folder.clone());
        if let Some(f) = folder {
            let info = crate::project::inspect(std::path::Path::new(&f));
            self.project.lock().unwrap().insert(cid.to_string(), info);
            (self.repaint)();
        }
    }

    pub fn health_of(&self, a: &Agent) -> Option<Health> {
        self.health.lock().unwrap().get(&probe::key(a)).cloned()
    }

    /// Unknown health counts as available: we'd rather try than refuse.
    pub fn available(&self, a: &Agent) -> bool {
        self.health_of(a).map(|h| h.available()).unwrap_or(true)
    }

    /// The agent as it should run now: on its own engine, or (for Claude/Codex roles) on the other
    /// engine if its own is logged out or out of quota. None = nothing can run it.
    pub fn effective(&self, a: &Agent) -> Option<Agent> {
        if self.available(a) {
            return Some(a.clone());
        }
        if a.kind == AgentKind::Custom {
            return None;
        }
        model::ENGINES.into_iter().filter(|k| *k != a.kind).find_map(|k| {
            let mut b = a.clone();
            b.kind = k;
            b.model = String::new(); // its model belongs to the other engine
            // A stand-in must be known-good, not just "unknown".
            self.health_of(&b).is_some_and(|h| h.available()).then_some(b)
        })
    }

    fn why_unavailable(&self, a: &Agent) -> String {
        self.health_of(a).and_then(|h| h.why_unavailable()).unwrap_or_else(|| "unavailable".into())
    }
}

/// Called by the UI when you hit send (or stop talking).
pub fn send(shared: &Arc<Shared>, cid: &str, text: String) {
    send_with(shared, cid, text, Vec::new());
}

/// Send with images attached (absolute paths inside the project folder).
pub fn send_with(shared: &Arc<Shared>, cid: &str, text: String, attachments: Vec<String>) {
    restart_for_follow_up(shared, cid);
    if let Some(ch) = shared.data.lock().unwrap().channel_mut(cid) {
        ch.messages.push(Message { author: Author::User, text: text.clone(), ts: model::now(), error: false, to_user: false, via: String::new(), attachments, receipt: String::new(), idle_claim: false });
    }
    shared.dirty.store(true, Ordering::Relaxed);
    (shared.repaint)();
    // Every message is handled right away, even while teammates are still working on earlier ones.
    {
        let mut runs = shared.runs.lock().unwrap();
        match runs.get_mut(cid) {
            Some(r) if r.cancel.load(Ordering::Relaxed) => {
                // You just pressed Stop: start this one once the stopped work has wound down.
                drop(runs);
                let shared = shared.clone();
                let cid = cid.to_string();
                std::thread::spawn(move || {
                    while shared.runs.lock().unwrap().contains_key(&cid) {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    start_handler(&shared, &cid, text);
                });
                return;
            }
            Some(r) => r.active += 1,
            None => {
                runs.insert(
                    cid.to_string(),
                    Run {
                        cancel: Arc::new(AtomicBool::new(false)),
                        pids: Arc::new(Mutex::new(Vec::new())),
                        working: Vec::new(),
                        started: Instant::now(),
                        queue: VecDeque::new(),
                        active: 1,
                        turn_flags: Vec::new(),
                    },
                );
            }
        }
    }
    let shared = shared.clone();
    let cid = cid.to_string();
    std::thread::spawn(move || run_handler(&shared, &cid, &text));
}

/// You sent another message right after one the main agent is still answering (a correction, a missing
/// detail): stop that answer, so the agent answers both together. Otherwise a helper would answer the
/// follow-up on its own, without the first message, and you'd get two half-answers.
fn restart_for_follow_up(shared: &Arc<Shared>, cid: &str) {
    let (prev, prev_ts, answered) = {
        let data = shared.data.lock().unwrap();
        let Some(ch) = data.channel(cid) else { return };
        let Some(i) = ch.messages.iter().rposition(|m| m.author == Author::User) else { return };
        let answered = ch.messages[i + 1..].iter().any(|m| matches!(m.author, Author::Agent(_)));
        (i, ch.messages[i].ts, answered)
    };
    if answered || model::now().saturating_sub(prev_ts) > FOLLOW_UP_SECS {
        return;
    }
    let target = {
        let turns = shared.lead_turns.lock().unwrap();
        match turns.get(cid) {
            Some(t) if t.user_msg == prev && !t.flag.load(Ordering::Relaxed) => {
                t.flag.store(true, Ordering::Relaxed);
                Some((t.agent.clone(), *t.pid.lock().unwrap()))
            }
            _ => None,
        }
    };
    let Some((agent, pid)) = target else { return };
    if let Some(pid) = pid {
        env::kill_tree(pid);
    }
    let name = shared.data.lock().unwrap().agent(&agent).map(|a| a.name().to_string()).unwrap_or_else(|| "The main agent".into());
    shared.post(cid, Author::System, format!("{name} is starting over with both of your messages."), false);
}

/// Register a handler on the channel's run and start it.
fn start_handler(shared: &Arc<Shared>, cid: &str, text: String) {
    {
        let mut runs = shared.runs.lock().unwrap();
        match runs.get_mut(cid) {
            Some(r) => r.active += 1,
            None => {
                runs.insert(
                    cid.to_string(),
                    Run {
                        cancel: Arc::new(AtomicBool::new(false)),
                        pids: Arc::new(Mutex::new(Vec::new())),
                        working: Vec::new(),
                        started: Instant::now(),
                        queue: VecDeque::new(),
                        active: 1,
                        turn_flags: Vec::new(),
                    },
                );
            }
        }
    }
    run_handler(shared, cid, &text);
}

fn run_handler(shared: &Arc<Shared>, cid: &str, text: &str) {
    handle(shared, cid, text);
    shared.refresh_project(cid); // agents may have added AGENTS.md, tasks, docs…
    let mut runs = shared.runs.lock().unwrap();
    if let Some(r) = runs.get_mut(cid) {
        r.active = r.active.saturating_sub(1);
        if r.active == 0 {
            runs.remove(cid);
        }
    }
    drop(runs);
    (shared.repaint)();
}

pub fn stop(shared: &Arc<Shared>, cid: &str) {
    let pids: Vec<u32> = {
        let mut runs = shared.runs.lock().unwrap();
        let Some(r) = runs.get_mut(cid) else { return };
        r.cancel.store(true, Ordering::Relaxed);
        r.turn_flags.iter().for_each(|f| f.store(true, Ordering::Relaxed));
        r.queue.clear();
        for w in r.working.iter_mut() {
            w.1 = "stopping…".into();
        }
        let slots = r.pids.lock().unwrap();
        slots.iter().filter_map(|p| *p.lock().unwrap()).collect()
    };
    for pid in pids {
        env::kill_tree(pid);
    }
}

#[derive(Clone)]
enum Mode {
    /// Answering the user directly.
    User,
    /// Doing a sub-task another agent handed over.
    Delegated { from: String },
    /// Delegates replied; check their work and wrap up.
    FollowUp,
    /// It promised work but did nothing: do it now (or say plainly that nothing is needed).
    Nudge,
}

struct Turns {
    budget: u32,
    /// Agents that failed (logged out / out of quota) during this message.
    failed: Vec<String>,
    /// Agents that already answered the user for this message (so a stand-in doesn't answer twice).
    answered: Vec<String>,
    /// Empty promises we've already sent back once.
    nudges: u32,
    /// Tokens used for this message so far, across all agents.
    tokens: u64,
}

fn handle(shared: &Arc<Shared>, cid: &str, text: &str) {
    let members = {
        let data = shared.data.lock().unwrap();
        match data.channel(cid) {
            Some(ch) => data.members(ch),
            None => return,
        }
    };
    if members.is_empty() {
        shared.post(cid, Author::System, "No agents in this channel yet. Add one from the channel header.".into(), true);
        return;
    }
    let mut targets = user_mentions(text, &members);
    if targets.is_empty() {
        targets.push(members[0].id.clone());
    }
    let turns = Mutex::new(Turns { budget: MAX_TURNS, failed: Vec::new(), answered: Vec::new(), nudges: 0, tokens: 0 });
    for t in targets {
        if shared.cancelled(cid) {
            break;
        }
        if turns.lock().unwrap().answered.contains(&t) {
            continue;
        }
        turn(shared, cid, &t, Mode::User, &turns, 0);
    }
    if shared.cancelled(cid) {
        let already = shared.data.lock().unwrap().channel(cid).and_then(|c| c.messages.last()).is_some_and(|m| m.text == "Stopped.");
        if !already {
            shared.post(cid, Author::System, "Stopped.".into(), false);
        }
    }
}

/// Another member that can answer the user instead of `agent_id`.
fn substitute(shared: &Shared, cid: &str, agent_id: &str, turns: &Turns) -> Option<Agent> {
    let members = {
        let data = shared.data.lock().unwrap();
        data.members(data.channel(cid)?)
    };
    members
        .into_iter()
        .find(|a| a.id != agent_id && !turns.failed.contains(&a.id) && !turns.answered.contains(&a.id) && shared.effective(a).is_some())
}

/// "Nobody can work right now" with a reason and a fix per engine, if every engine is known to be down.
fn outage_report(shared: &Shared) -> Option<String> {
    let health = shared.health.lock().unwrap();
    let mut lines = Vec::new();
    for kind in model::ENGINES {
        let h = health.get(kind.label())?;
        let why = h.why_unavailable()?;
        let fix = match kind {
            _ if !h.installed => String::new(),
            AgentKind::Claude if h.logged_in == Some(false) => " Fix: run `claude` in Terminal and type `/login`.".into(),
            AgentKind::Codex if h.logged_in == Some(false) => " Fix: run `codex login` in Terminal.".into(),
            AgentKind::Codex => " Codex has its own usage limit, separate from chatting on chatgpt.com.".into(),
            AgentKind::Local => " Open the Ollama app, or install a model with `ollama pull gpt-oss:20b`.".into(),
            _ => String::new(),
        };
        lines.push(format!("- **{}**: {why}.{fix}", kind.label()));
    }
    Some(format!(
        "Nobody can work right now:\n{}\nAs soon as one is back, just say it again; I check every few minutes (or click ⟳).",
        lines.join("\n")
    ))
}

/// Sessions are per role *and* engine (a Claude session can't be resumed by Codex).
pub fn session_key(a: &Agent) -> String {
    format!("{}:{}", a.id, a.kind.label())
}

/// Say once in the chat that a role is running on a stand-in engine.
fn note_engine(shared: &Shared, cid: &str, agent: &Agent, eff: &Agent) {
    let mut notes = shared.engine_notes.lock().unwrap();
    if eff.kind == agent.kind {
        notes.remove(&agent.id);
        return;
    }
    if notes.get(&agent.id) == Some(&eff.kind) {
        return;
    }
    notes.insert(agent.id.clone(), eff.kind);
    drop(notes);
    let why = shared.why_unavailable(agent);
    shared.post(cid, Author::System, format!("{} is working on {} for now ({} is {why}).", agent.name(), eff.kind.label(), agent.kind.label()), false);
}

fn mark_failure(shared: &Shared, eff: &Agent, failure: &Failure) {
    let mut health = shared.health.lock().unwrap();
    let h = health.entry(probe::key(eff)).or_insert_with(|| Health { installed: true, ..Default::default() });
    match failure {
        Failure::Auth => h.logged_in = Some(false),
        Failure::Limit if !h.blocked() => {
            h.blocked_until = Some(model::now() + 15 * 60);
            h.blocked_reason = Some("usage limit reached".into());
        }
        _ => {}
    }
}

fn turn(shared: &Arc<Shared>, cid: &str, agent_id: &str, mode: Mode, turns: &Mutex<Turns>, depth: u32) {
    turn_inst(shared, cid, agent_id, mode, turns, depth, 0)
}

/// The General helper: created the first time someone is busy, on the main agent's engine, in every channel.
fn ensure_general(shared: &Shared) -> Option<String> {
    let mut data = shared.data.lock().unwrap();
    if let Some(a) = data.agents.iter().find(|a| a.handle == "general") {
        let id = a.id.clone();
        for ch in data.channels.iter_mut() {
            if !ch.members.contains(&id) {
                ch.members.push(id.clone());
            }
        }
        return Some(id);
    }
    let tpl = model::TEMPLATES.iter().find(|t| t.handle == "general")?;
    let mut g = tpl.agent(model::PALETTE[5]);
    if let Some(main) = data.agents.iter().find(|a| a.handle == "main").or(data.agents.first()) {
        // Same engine as the main agent (and its command, for custom setups).
        g.kind = main.kind;
        g.command = main.command.clone();
        g.full_access = main.full_access;
    }
    g.display_name = "General".into();
    let id = g.id.clone();
    data.agents.push(g);
    for ch in data.channels.iter_mut() {
        ch.members.push(id.clone());
    }
    shared.dirty.store(true, Ordering::Relaxed);
    Some(id)
}

/// A free General helper instance in this channel (not `skip`), if any. Instances > 0 run without a saved session.
fn free_general(shared: &Shared, cid: &str, skip: (&str, usize)) -> Option<(String, usize)> {
    let gid = ensure_general(shared)?;
    for inst in 0..4 {
        if (gid.as_str(), inst) == skip {
            continue;
        }
        let key = if inst == 0 { format!("{cid}:{gid}") } else { format!("{cid}:{gid}#{inst}") };
        let lock = shared.agent_locks.lock().unwrap().entry(key).or_default().clone();
        let free = lock.try_lock().is_ok();
        if free {
            return Some((gid, inst));
        }
    }
    None
}

/// `inst`: which copy of the agent (0 = the agent itself; 1+ = extra General helpers, no saved session).
#[allow(clippy::too_many_arguments)]
fn turn_inst(shared: &Arc<Shared>, cid: &str, agent_id: &str, mode: Mode, turns: &Mutex<Turns>, depth: u32, inst: usize) {
    if shared.cancelled(cid) {
        return;
    }
    if turns.lock().unwrap().budget == 0 {
        shared.post(cid, Author::System, format!("Paused: the agents used their {MAX_TURNS} turns for this message. Reply to keep going."), false);
        return;
    }
    let Some(agent) = shared.data.lock().unwrap().agent(agent_id).cloned() else { return };

    // Which engine runs this role right now? Its own, or a stand-in if that one is logged out / out of quota.
    let mut eff = match shared.effective(&agent) {
        Some(e) => e,
        None => {
            let why = shared.why_unavailable(&agent);
            let sub = substitute(shared, cid, agent_id, &turns.lock().unwrap());
            match (&mode, sub) {
                (Mode::User | Mode::FollowUp, Some(sub)) => {
                    shared.post(cid, Author::System, format!("{} is {why}, so {} is taking this.", agent.name(), sub.name()), false);
                    turns.lock().unwrap().failed.push(agent.id.clone());
                    return turn(shared, cid, &sub.id, mode, turns, depth);
                }
                (Mode::Delegated { .. }, _) => {
                    // Tell the delegator, without spending a turn.
                    shared.post(cid, Author::Agent(agent.id.clone()), format!("I can't help right now: {why}."), true);
                    return;
                }
                _ => {
                    // Nothing can run it. If we know why, say so plainly instead of failing again.
                    if let Some(report) = outage_report(shared) {
                        shared.post(cid, Author::System, report, true);
                        let s = shared.clone();
                        std::thread::spawn(move || crate::heartbeat::refresh_health(&s)); // maybe you just logged in
                        return;
                    }
                    agent.clone() // status unknown: try anyway
                }
            }
        }
    };
    note_engine(shared, cid, &agent, &eff);
    {
        let mut t = turns.lock().unwrap();
        if t.budget == 0 {
            return;
        }
        t.budget -= 1;
    }
    shared.activity.lock().unwrap().entry(cid.to_string()).or_default().insert(agent.handle.clone());

    // One turn at a time per agent (its CLI session can't take two at once); others keep working meanwhile.
    let key = if inst == 0 { format!("{cid}:{}", agent.id) } else { format!("{cid}:{}#{inst}", agent.id) };
    let lock = shared.agent_locks.lock().unwrap().entry(key).or_default().clone();
    let guard = match lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            // It's stopping its answer to your previous message to take both together: wait for it.
            let restarting = shared.lead_turns.lock().unwrap().get(cid).is_some_and(|t| t.agent == agent.id && t.flag.load(Ordering::Relaxed));
            // Busy: rather than make you wait, a General helper takes it.
            if matches!(mode, Mode::User | Mode::Delegated { .. }) && !restarting {
                if let Some((gid, ginst)) = free_general(shared, cid, (agent.id.as_str(), inst)) {
                    let gname = shared.data.lock().unwrap().agent(&gid).map(|a| a.name().to_string()).unwrap_or_else(|| "General".into());
                    let label = if ginst == 0 { gname } else { format!("{gname} #{}", ginst + 1) };
                    shared.post(cid, Author::System, format!("{} is busy, so {label} is taking this.", agent.name()), false);
                    turns.lock().unwrap().budget += 1;
                    return turn_inst(shared, cid, &gid, mode, turns, depth, ginst);
                }
            }
            shared.set_status(cid, &agent.handle, "finishing its previous turn first…".into());
            lock.lock().unwrap_or_else(|e| e.into_inner())
        }
    };
    if shared.cancelled(cid) {
        shared.done_working(cid, &agent.handle);
        return;
    }

    // Snapshot what we need, then release the lock while the agent works.
    let (members, folder, sessions, read_aloud, voice_name) = {
        let data = shared.data.lock().unwrap();
        let Some(ch) = data.channel(cid) else { return };
        let idx = data.agents.iter().position(|a| a.id == agent.id).unwrap_or(0);
        let voice_name = voice::agent_voice(idx, &agent.voice, &agent.eleven_voice);
        (data.members(ch), PathBuf::from(&ch.folder), ch.sessions.clone(), data.settings.read_aloud, voice_name)
    };
    if !folder.is_dir() {
        shared.post(cid, Author::System, format!("The channel folder `{}` doesn't exist. Pick another one in the header.", folder.display()), true);
        return;
    }
    // Our own process slot, so parallel turns can each be stopped.
    let pid = Arc::new(Mutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut runs = shared.runs.lock().unwrap();
        let Some(r) = runs.get_mut(cid) else { return };
        r.pids.lock().unwrap().push(pid.clone());
        if r.cancel.load(Ordering::Relaxed) {
            cancel.store(true, Ordering::Relaxed);
        }
        r.turn_flags.push(cancel.clone());
    }
    // The main agent answering you: a quick follow-up message can restart this turn (restart_for_follow_up).
    let _lead = (matches!(mode, Mode::User) && inst == 0 && depth == 0 && members.first().is_some_and(|m| m.id == agent.id)).then(|| {
        let user_msg = shared.data.lock().unwrap().channel(cid).and_then(|c| c.messages.iter().rposition(|m| m.author == Author::User)).unwrap_or(0);
        shared.lead_turns.lock().unwrap().insert(cid.to_string(), LeadTurn { agent: agent.id.clone(), user_msg, flag: cancel.clone(), pid: pid.clone() });
        LeadTurnGuard { shared, cid, flag: cancel.clone() }
    });
    let status = |s: String| shared.set_status(cid, &agent.handle, s);
    let ctl = Ctl { cancel: &cancel, pid: &pid, status: &status };
    // Long-term memory: the channel's note (new conversations) and older messages related to this request.
    let (recall, router_recall) = memory_recall(shared, cid);
    let prompt_for = |fresh: bool| {
        let p = {
            let data = shared.data.lock().unwrap();
            data.channel(cid).map(|ch| build_prompt(ch, &agent, &members, &data, &mode, fresh, read_aloud, shared)).unwrap_or_default()
        };
        with_memory(p, cid, fresh, &recall, shared)
    };

    // Images on your latest message go to the agent answering you.
    let images: Vec<String> = if matches!(mode, Mode::User) {
        let data = shared.data.lock().unwrap();
        data.channel(cid)
            .and_then(|ch| ch.messages.iter().rev().find(|m| m.author == Author::User))
            .map(|m| m.attachments.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    // Router mode: the main agent's routine turns are one cheap, tool-free call. "/work" = it needs its tools.
    let mut routed: Option<(agents::Outcome, Option<String>)> = None;
    let mut router_tokens = 0u64;
    let quick = shared.data.lock().unwrap().settings.quick_routing;
    let is_main = members.first().is_some_and(|m| m.id == agent.id);
    if quick && inst == 0 && is_main && eff.kind == AgentKind::Codex && images.is_empty() && matches!(mode, Mode::User | Mode::FollowUp) {
        let prompt = {
            let data = shared.data.lock().unwrap();
            data.channel(cid).map(|ch| build_router_prompt(ch, &agent, &members, &data, &mode, read_aloud, shared)).unwrap_or_default()
        };
        let prompt = with_memory(prompt, cid, true, &router_recall, shared);
        let model = probe::pick_model(&eff, shared.health_of(&eff).as_ref());
        shared.set_status(cid, &agent.handle, "thinking…".into());
        let r = agents::run_router(&folder, &prompt, model.as_deref(), eff.internet, &ctl);
        if cancel.load(Ordering::Relaxed) {
            shared.done_working(cid, &agent.handle);
            return;
        }
        if !r.error && !agents::wants_full_agent(&r.text) {
            routed = Some((r, model));
        } else {
            router_tokens = r.receipt.tokens;
            shared.set_status(cid, &agent.handle, "looking at the project…".into());
        }
    }

    // Extra helper copies start fresh each time (two copies can't share one CLI session).
    let mut session = if eff.kind.supports_sessions() && inst == 0 { sessions.get(&session_key(&eff)).cloned() } else { None };
    let mut out;
    let mut used_model;
    let mut tries = 0;
    if let Some((r, m)) = routed {
        out = r;
        used_model = m;
    } else {
    loop {
        tries += 1;
        used_model = probe::pick_model(&eff, shared.health_of(&eff).as_ref());
        let label = used_model.clone().unwrap_or_else(|| "default model".into());
        shared.set_status(cid, &agent.handle, format!("thinking… ({} · {label})", eff.kind.label()));
        // `eff` carries the role's identity (handle, briefing) with the engine that's actually available.
        out = agents::run(&eff, &folder, &prompt_for(session.is_none()), session.as_deref(), used_model.as_deref(), &images, &ctl);
        if let Some(info) = &out.rate {
            let mut health = shared.health.lock().unwrap();
            probe::apply_claude_rate_info(info, health.entry(probe::key(&eff)).or_insert_with(|| Health { installed: true, ..Default::default() }));
        }
        if !out.error || cancel.load(Ordering::Relaxed) || tries >= 4 {
            break;
        }
        match probe::classify(&out.text) {
            Failure::Model(name) => {
                // That model isn't available on this account: remember, pick another, retry.
                let bad = name.or(used_model.clone()).unwrap_or_else(|| "default".into());
                let mut health = shared.health.lock().unwrap();
                let h = health.entry(probe::key(&eff)).or_insert_with(|| Health { installed: true, ..Default::default() });
                if h.bad_models.contains(&bad) {
                    break;
                }
                h.bad_models.push(bad.clone());
                drop(health);
                shared.set_status(cid, &agent.handle, format!("{bad} isn't available, switching model…"));
            }
            f @ (Failure::Auth | Failure::Limit) => {
                // Logged out / out of quota: same role, other engine.
                mark_failure(shared, &eff, &f);
                match shared.effective(&agent).filter(|e| e.kind != eff.kind) {
                    Some(next) => {
                        note_engine(shared, cid, &agent, &next);
                        eff = next;
                        session = sessions.get(&session_key(&eff)).cloned();
                    }
                    None => break,
                }
            }
            Failure::Other if session.is_some() => {
                // The saved session may be gone (deleted, other machine…). Start fresh once.
                session = None;
                shared.set_status(cid, &agent.handle, "retrying with a fresh session…".into());
            }
            Failure::Other => break,
        }
    }
    }
    out.receipt.tokens += router_tokens;
    shared.done_working(cid, &agent.handle);
    if let Some(r) = shared.runs.lock().unwrap().get_mut(cid) {
        r.pids.lock().unwrap().retain(|p| !Arc::ptr_eq(p, &pid));
        r.turn_flags.retain(|f| !Arc::ptr_eq(f, &cancel));
    }
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    let failure = if out.error { probe::classify(&out.text) } else { Failure::Other };
    if out.error {
        mark_failure(shared, &eff, &failure);
    }

    // App actions (/new-channel, /send, /switch) are carried out by the app, not shown as text.
    let (shown, actions) = if out.error { (out.text.clone(), Vec::new()) } else { app_actions(&out.text) };
    let shown = if shown.trim().is_empty() && !actions.is_empty() { "On it.".to_string() } else { shown };

    // What did this turn really do? (From the engine's events, not from the reply's wording.)
    let exclude = match &mode {
        Mode::Delegated { from } => Some(from.clone()),
        _ => None,
    };
    let delegates = if out.error { Vec::new() } else { delegations(&out.text, &members, &agent.id, exclude.as_deref()) };
    let mut receipt = out.receipt.summary();
    if !delegates.is_empty() {
        let names: Vec<String> = delegates.iter().filter_map(|id| members.iter().find(|m| &m.id == id)).map(|m| m.name().to_string()).collect();
        if !receipt.is_empty() {
            receipt.push_str(" · ");
        }
        receipt.push_str(&format!("handed off to {}", names.join(", ")));
    }
    if !actions.is_empty() {
        if !receipt.is_empty() {
            receipt.push_str(" · ");
        }
        receipt.push_str("app action");
    }
    // Per-message token budget: past it, no more handoffs, follow-ups or retries for this message.
    let over_budget = {
        let mut t = turns.lock().unwrap();
        t.tokens += out.receipt.tokens;
        t.tokens > TOKEN_BUDGET
    };
    // An agent that tried something and was blocked isn't making empty promises: it already says why.
    // Nor is one that looked things up and answered, or that asks you for something ("send me the link and I'll…").
    let idle_claim = !out.error
        && !out.receipt.did_work()
        && out.receipt.blocked == 0
        && out.receipt.lookups == 0
        && delegates.is_empty()
        && actions.is_empty()
        && !asks_user(&shown)
        && claims_work(&shown, &members);

    let to_user = matches!(mode, Mode::User | Mode::FollowUp | Mode::Nudge) && !out.error;
    if matches!(mode, Mode::User) && !out.error {
        turns.lock().unwrap().answered.push(agent.id.clone());
    }
    let via = match (&used_model, eff.kind) {
        (_, AgentKind::Custom) => String::new(),
        (Some(m), k) => format!("{} · {m}", k.label()),
        (None, k) => k.label().to_string(),
    };
    {
        let mut data = shared.data.lock().unwrap();
        let selected = data.selected.as_deref() == Some(cid);
        if let Some(ch) = data.channel_mut(cid) {
            if let (Some(s), 0) = (&out.session, inst) {
                ch.sessions.insert(session_key(&eff), s.clone());
            }
            ch.messages.push(Message {
                author: Author::Agent(agent.id.clone()),
                text: shown.clone(),
                ts: model::now(),
                error: out.error,
                to_user,
                via,
                attachments: Vec::new(),
                receipt,
                idle_claim,
            });
            ch.seen.insert(agent.id.clone(), ch.messages.len());
            if !selected {
                ch.unread = true;
            }
        }
        if to_user && read_aloud && selected {
            (shared.speak)((shown.clone(), voice_name));
        }
    }
    shared.dirty.store(true, Ordering::Relaxed);
    (shared.repaint)();
    drop(guard); // follow-ups and handoffs take their own turns
    run_app_actions(shared, cid, &actions);

    if out.error {
        turns.lock().unwrap().failed.push(agent.id.clone());
        if matches!(failure, Failure::Auth | Failure::Limit) && matches!(mode, Mode::User | Mode::FollowUp) {
            let sub = substitute(shared, cid, &agent.id, &turns.lock().unwrap());
            if let Some(sub) = sub {
                shared.post(cid, Author::System, format!("{} is taking over.", sub.name()), false);
                turn(shared, cid, &sub.id, Mode::User, turns, depth);
            }
        }
        return;
    }
    if over_budget {
        let used = turns.lock().unwrap().tokens;
        let first = !turns.lock().unwrap().failed.iter().any(|f| f == "__budget");
        if first {
            turns.lock().unwrap().failed.push("__budget".into());
            shared.post(
                cid,
                Author::System,
                format!("Paused this request at {} to save your quota. Say \"continue\" to keep going.", agents::tokens_label(used)),
                false,
            );
        }
        return;
    }
    // Said work was happening, but nothing was run, edited or handed off: send it back once.
    if idle_claim && !shared.cancelled(cid) {
        let again = {
            let mut t = turns.lock().unwrap();
            let ok = t.nudges < 2 && t.budget > 0;
            if ok {
                t.nudges += 1;
            }
            ok
        };
        if again {
            shared.post(
                cid,
                Author::System,
                format!("{} said work was happening, but nothing was run, edited or handed off. Sending {} back to actually do it.", agent.name(), agent.name()),
                true,
            );
            let next = match &mode {
                Mode::Delegated { from } => Mode::Delegated { from: from.clone() },
                _ => Mode::Nudge,
            };
            return turn(shared, cid, &agent.id, next, turns, depth);
        }
    }
    if depth >= MAX_DEPTH || delegates.is_empty() {
        return;
    }
    if delegates.len() == 1 {
        turn(shared, cid, &delegates[0], Mode::Delegated { from: agent.id.clone() }, turns, depth + 1);
    } else {
        // Several teammates briefed at once: they work in parallel, like people would.
        std::thread::scope(|scope| {
            for d in &delegates {
                let from = agent.id.clone();
                scope.spawn(move || turn(shared, cid, d, Mode::Delegated { from }, turns, depth + 1));
            }
        });
    }
    if !shared.cancelled(cid) {
        let next = match mode {
            // A delegate that sub-delegated reports back to its own delegator afterwards.
            Mode::Delegated { from } => Mode::Delegated { from },
            _ => Mode::FollowUp,
        };
        turn(shared, cid, &agent.id, next, turns, depth);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_prompt(ch: &Channel, me: &Agent, members: &[Agent], data: &Data, mode: &Mode, fresh: bool, read_aloud: bool, shared: &Shared) -> String {
    let mut p = String::new();
    let start;
    if !fresh {
        let seen = ch.seen.get(&me.id).copied().unwrap_or(0).min(ch.messages.len());
        start = seen.max(ch.messages.len().saturating_sub(HISTORY));
        p.push_str(&format!("## New messages in #{} since your last turn\n\n", ch.name));
        p.push_str(&format!(
            "(You can read the user's other projects in {}. If they name one you can't place, look there and ask.)\n\n",
            model::projects_dir().replacen(&model::home_dir(), "~", 1)
        ));
    } else {
        p.push_str(&preamble(ch, me, members, shared));
        start = ch.messages.len().saturating_sub(HISTORY);
        p.push_str("\n## Conversation so far\n\n");
    }
    for m in &ch.messages[start..] {
        let who = match &m.author {
            Author::User => "User".to_string(),
            Author::Agent(id) if *id == me.id => "You".to_string(),
            Author::Agent(id) => data.agent(id).map(|a| a.label()).unwrap_or_else(|| "@agent".into()),
            Author::Heartbeat => "Heartbeat (file watcher)".to_string(),
            Author::System => continue,
        };
        if m.error {
            // Tell the others an agent failed (so the lead can take over), without the noise.
            let first = m.text.lines().next().unwrap_or_default();
            p.push_str(&format!("**{who}:** (failed, could not do it: {})\n\n", agents::short(first, 200)));
            continue;
        }
        let text: String = if m.text.chars().count() > MAX_MSG_CHARS {
            format!("{}… [truncated]", m.text.chars().take(MAX_MSG_CHARS).collect::<String>())
        } else {
            m.text.clone()
        };
        p.push_str(&format!("**{who}:** {text}\n"));
        if !m.attachments.is_empty() {
            p.push_str(&format!(
                "(Attached image{}: {}. Open {} to see what the user means.)\n",
                if m.attachments.len() == 1 { "" } else { "s" },
                m.attachments.join(", "),
                if m.attachments.len() == 1 { "it" } else { "them" }
            ));
        }
        p.push('\n');
    }
    let name = |id: &str| data.agent(id).map(|a| a.handle.clone()).unwrap_or_else(|| "agent".into());
    p.push_str("## Your turn\n");
    p.push_str(&match mode {
        Mode::User => "Respond to the user's latest message above. If their last few messages belong together (a \
                       correction like \"I mean X\", or a detail added right after), answer them as one request."
            .to_string(),
        Mode::Delegated { from } => format!(
            "@{from} handed you a task: the line starting with \"@{me}:\" above (or, if you're standing in for a busy \
             teammate, the latest \"@name:\" line from @{from}), plus anything said since. Do it, then reply with a short \
             report of what you did and the result. Your reply goes back to @{from}.",
            from = name(from),
            me = me.handle
        ),
        Mode::Nudge => "Your last reply said work was starting or done, but you ran no command, edited no file and \
                        handed nothing off, so NOTHING IS HAPPENING. Fix that now: do the work with your tools, or hand it off \
                        with a line that starts with \"@name:\" and a complete brief. If no work is actually needed, say so \
                        plainly in one sentence. Don't describe plans."
            .to_string(),
        Mode::FollowUp => "The agents you handed work to have replied above. Check their results. If something is \
                           missing or wrong, hand it off again; otherwise give the user a short final answer."
            .to_string(),
    });
    if read_aloud && !matches!(mode, Mode::Delegated { .. }) {
        p.push_str(
            "\n(Your reply is read aloud to the user. Keep it short: one or two plain spoken sentences that \
             answer or summarize. Add details, code or lists only if the user asked for them.)",
        );
    }
    // Facts from the app, so the main agent never reports imaginary progress.
    if members.first().is_some_and(|m| m.id == me.id) && !matches!(mode, Mode::Delegated { .. }) {
        p.push_str(&facts(ch, me, data, shared));
    }
    // The main agent can run the app itself. Repeated every turn so it also reaches ongoing sessions.
    let is_main = members.first().is_some_and(|m| m.id == me.id);
    if is_main && !matches!(mode, Mode::Delegated { .. }) {
        let names: Vec<String> = data.channels.iter().map(|c| format!("#{}", c.name)).collect();
        p.push_str(&format!(
            "\n(App actions: you can operate the YesHarness app. Write these on their own lines and the app does them:\n\
             /new-channel <name> [folder]   creates a project channel (folder defaults to ~/Development/<name>) and opens it\n\
             /send <channel> <message>      posts a request in that channel so the team there starts on it\n\
             /switch <channel>              opens a channel\n\
             Use them whenever the user asks for a channel or a new project; never tell the user to do it themselves. \
             Existing channels: {}.)",
            names.join(", ")
        ));
    }
    p
}

/// Unregisters a LeadTurn when the turn ends (unless a newer turn replaced it).
struct LeadTurnGuard<'a> {
    shared: &'a Shared,
    cid: &'a str,
    flag: Arc<AtomicBool>,
}

impl Drop for LeadTurnGuard<'_> {
    fn drop(&mut self) {
        let mut t = self.shared.lead_turns.lock().unwrap();
        if t.get(self.cid).is_some_and(|l| Arc::ptr_eq(&l.flag, &self.flag)) {
            t.remove(self.cid);
        }
    }
}

/// Older messages related to the latest request, for a full prompt (beyond its HISTORY window) and a router
/// prompt (beyond its last 12 messages). Empty when memory is off or not set up.
fn memory_recall(shared: &Shared, cid: &str) -> (String, String) {
    let (query, full_ts, router_ts) = {
        let data = shared.data.lock().unwrap();
        if !data.settings.memory {
            return (String::new(), String::new());
        }
        let Some(ch) = data.channel(cid) else { return (String::new(), String::new()) };
        let query = ch
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.author, Author::User | Author::Agent(_)) && !m.error)
            .map(|m| m.text.chars().take(2000).collect::<String>())
            .unwrap_or_default();
        let ts_at = |n: usize| ch.messages.get(ch.messages.len().saturating_sub(n)).map(|m| m.ts).unwrap_or(0);
        (query, ts_at(HISTORY), ts_at(ROUTER_HISTORY))
    };
    let mut r = crate::memory::recall_sections(cid, &query, &[full_ts, router_ts]).into_iter();
    (r.next().unwrap_or_default(), r.next().unwrap_or_default())
}

/// Adds the memory note (at the start of a conversation) and recalled messages, just before "## Your turn".
fn with_memory(mut prompt: String, cid: &str, fresh: bool, recall: &str, shared: &Shared) -> String {
    if !shared.data.lock().unwrap().settings.memory {
        // What you told the app about yourself is used even with memory off.
        if fresh {
            insert_memory(&mut prompt, &crate::memory::you_section());
        }
        return prompt;
    }
    let mut extra = if fresh { format!("{}{}", crate::memory::you_section(), crate::memory::note_section(cid)) } else { String::new() };
    extra.push_str(recall);
    insert_memory(&mut prompt, &extra);
    prompt
}

fn insert_memory(prompt: &mut String, extra: &str) {
    if extra.trim().is_empty() {
        return;
    }
    let block = format!("{}\n\n", extra.trim());
    match prompt.rfind("## Your turn") {
        Some(i) => prompt.insert_str(i, &block),
        None => {
            prompt.push_str("\n\n");
            prompt.push_str(&block);
        }
    }
}

/// Router-mode prompt: briefing, team, facts and the recent conversation, kept small (one call, no tools).
#[allow(clippy::too_many_arguments)]
fn build_router_prompt(ch: &Channel, me: &Agent, members: &[Agent], data: &Data, mode: &Mode, read_aloud: bool, shared: &Shared) -> String {
    let mut p = format!(
        "You are {} (@{}), the main agent in channel #{}. Project folder: {}.\n",
        me.name(),
        me.handle,
        ch.name,
        ch.folder
    );
    if !me.briefing.trim().is_empty() {
        p.push_str(&format!("\n## Your briefing\n{}\n", me.briefing.trim()));
    }
    let others: Vec<&Agent> = members.iter().filter(|a| a.id != me.id && shared.effective(a).is_some()).collect();
    p.push_str("\n## Your team (hand off with a line \"@name: brief\")\n");
    for a in &others {
        let desc = if a.description.is_empty() { a.kind.label().to_string() } else { a.description.clone() };
        p.push_str(&format!("- {}: {}\n", a.label(), desc));
    }
    p.push_str(&facts(ch, me, data, shared));
    p.push_str(&format!(
        "\nThe user speaks; names in transcripts get mangled. Their projects are folders in {}: if they name one you \
         can't place, reply /work to look, or ask which one they mean. Never guess a different project.\n",
        model::projects_dir().replacen(&model::home_dir(), "~", 1)
    ));
    p.push_str("\n## Recent conversation\n");
    let start = ch.messages.len().saturating_sub(ROUTER_HISTORY);
    for m in &ch.messages[start..] {
        let who = match &m.author {
            Author::User => "User".to_string(),
            Author::Agent(id) if *id == me.id => "You".to_string(),
            Author::Agent(id) => data.agent(id).map(|a| a.label()).unwrap_or_else(|| "@agent".into()),
            Author::Heartbeat => "Heartbeat".to_string(),
            Author::System => continue,
        };
        let text: String = if m.error {
            format!("(failed: {})", agents::short(m.text.lines().next().unwrap_or_default(), 160))
        } else if m.text.chars().count() > 600 {
            format!("{}… [trimmed]", m.text.chars().take(600).collect::<String>())
        } else {
            m.text.clone()
        };
        p.push_str(&format!("**{who}:** {text}\n\n"));
    }
    p.push_str("## Your turn\n");
    p.push_str(match mode {
        Mode::FollowUp => "Your teammates replied above. Check their results: if done, tell the user in one or two sentences what \
                           happened and where to find it; if something is missing, hand it off again.",
        _ => "Reply to the user's latest message.",
    });
    if read_aloud {
        p.push_str(" Your reply is read aloud: plain spoken sentences.");
    }
    let names: Vec<String> = data.channels.iter().map(|c| format!("#{}", c.name)).collect();
    p.push_str(&format!("\nExisting channels: {}.", names.join(", ")));
    p
}

fn preamble(ch: &Channel, me: &Agent, members: &[Agent], shared: &Shared) -> String {
    let is_main = members.first().map(|a| a.id == me.id).unwrap_or(false);
    let role = if me.description.is_empty() { me.kind.label().to_string() } else { me.description.clone() };
    let mut p = format!(
        "You are @{me}, one of the AI agents in the user's small virtual office (the YesHarness app), in the \
         channel #{chan}. Your role: {role}\nThe project folder for this channel is {folder} (your working directory).\n",
        me = me.handle,
        chan = ch.name,
        folder = ch.folder
    );
    p.push_str(&format!("Your display name is {}. Your routing handle remains @{}.\n", me.name(), me.handle));
    if !me.briefing.trim().is_empty() {
        p.push_str(&format!("\n## Your briefing\n{}\n", me.briefing.trim()));
    }
    // Only offer handoffs to agents that can actually work right now (on any engine).
    let others: Vec<&Agent> = members.iter().filter(|a| a.id != me.id && shared.effective(a).is_some()).collect();
    p.push_str("\n## Your team\n");
    if others.is_empty() {
        p.push_str("You are the only agent available in this channel right now, so do everything yourself.\n");
    } else {
        for a in &others {
            let desc = if a.description.is_empty() { a.kind.label().to_string() } else { a.description.clone() };
            let tag = if members.first().is_some_and(|m| m.id == a.id) { " (main agent)" } else { "" };
            p.push_str(&format!("- {}{tag}: {}\n", a.label(), desc));
        }
        if is_main {
            p.push_str(
                "\nYou are the MAIN agent: the user talks to you and doesn't care which agent does the work. \
                 Decide who should do it. ",
            );
        } else {
            p.push_str(
                "\nThe main agent coordinates and usually hands you tasks; focus on your role and report back \
                 concisely. ",
            );
        }
        p.push_str(&format!(
            "To hand a task to a teammate, write a line that starts with \"@name:\" followed by a complete, \
             self-contained brief, e.g.\n@{ex}: …\nThe app runs that agent and shows you its reply afterwards. \
             Hand off when the task fits a teammate's role better than yours, or to work in parallel; otherwise \
             just do it yourself. Never start a line with \"@name:\" unless you mean to hand off a task.\n",
            ex = others[0].handle
        ));
    }
    p.push_str(&format!(
        "\n## House rules\nShared notes live in AGENTS.md (how to work in this project), TASKS.md (the task list: \
         keep it up to date as you finish or discover work) and docs/. A heartbeat reports changes to these files \
         in the channel every minute.\n\
         Use your tools to actually do the work in the project folder. Reply in a concise chat style \
         (Markdown is fine). The user mostly speaks instead of typing, so their messages are voice transcripts \
         that may contain mistakes (e.g. \"docks\" for \"docs\"); interpret them sensibly and don't ask \
         about obvious slips. Names get mangled most (\"Moon pace\" for \"Moonbase\"): when the user names a \
         project, look for it (their projects are folders in {projects}, which you can read; also their GitHub) \
         and if nothing clearly matches, say what you found and ask which one they mean instead of guessing.\n",
        projects = model::projects_dir().replacen(&model::home_dir(), "~", 1)
    ));
    p
}

/// Something an agent asked the app to do.
#[derive(Debug, PartialEq, Clone)]
pub enum AppAction {
    NewChannel { name: String, folder: Option<String> },
    Switch(String),
    Send { channel: String, text: String },
}

/// Pull `/new-channel`, `/send` and `/switch` lines out of a reply. Returns the rest of the text and the actions.
pub fn app_actions(reply: &str) -> (String, Vec<AppAction>) {
    let mut kept = Vec::new();
    let mut actions = Vec::new();
    let mut in_code = false;
    for line in reply.lines() {
        let l = line.trim();
        if l.starts_with("```") {
            in_code = !in_code;
            kept.push(line);
            continue;
        }
        let cmd = l.trim_start_matches(['-', '*', '>', ' ']).trim_matches('`').trim();
        let parsed = if in_code {
            None
        } else if let Some(rest) = cmd.strip_prefix("/new-channel ") {
            // "<name, maybe with spaces> [~/path, /path or C:\path]"
            let rest = rest.trim();
            let cut = [" ~/", " /", " \"~/", " \"/"].iter().filter_map(|p| rest.find(p)).chain(windows_path_start(rest)).min();
            let (name, folder) = match cut {
                Some(i) => (&rest[..i], Some(rest[i..].trim().trim_matches(['"', '\'', '`']).to_string())),
                None => (rest, None),
            };
            let name = name.trim().trim_start_matches('#').to_string();
            (!name.is_empty()).then_some(AppAction::NewChannel { name, folder: folder.filter(|f| !f.is_empty()) })
        } else if let Some(rest) = cmd.strip_prefix("/send ") {
            let mut parts = rest.trim().splitn(2, char::is_whitespace);
            let channel = parts.next().unwrap_or("").trim_start_matches('#').to_string();
            let text = parts.next().unwrap_or("").trim().to_string();
            (!channel.is_empty() && !text.is_empty()).then_some(AppAction::Send { channel, text })
        } else if let Some(rest) = cmd.strip_prefix("/switch ") {
            let channel = rest.trim().trim_start_matches('#').to_string();
            (!channel.is_empty()).then_some(AppAction::Switch(channel))
        } else {
            None
        };
        match parsed {
            Some(a) => actions.push(a),
            None => kept.push(line),
        }
    }
    (kept.join("\n").trim().to_string(), actions)
}

/// Channel names are lowercase-with-dashes.
pub fn channel_slug(s: &str) -> String {
    let s: String = s.trim().trim_start_matches('#').to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
    s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
}

/// Where a new channel's folder goes: what the agent said if it's inside your home folder,
/// otherwise ~/Development/<name>.
/// Where a Windows path (" C:\…" or " C:/…", maybe quoted) starts in a /new-channel line.
fn windows_path_start(rest: &str) -> Option<usize> {
    let b = rest.as_bytes();
    (0..b.len()).find(|&i| {
        let j = if b.get(i + 1) == Some(&b'"') { i + 2 } else { i + 1 };
        b[i] == b' ' && b.get(j).is_some_and(|c| c.is_ascii_alphabetic()) && b.get(j + 1) == Some(&b':') && matches!(b.get(j + 2), Some(b'\\' | b'/'))
    })
}

fn channel_folder(name: &str, folder: Option<&str>) -> String {
    use std::path::{Component, Path};
    let home = model::home_dir();
    let fallback = format!("{home}/Development/{name}");
    let Some(f) = folder else { return fallback };
    let f = if let Some(rest) = f.strip_prefix("~/") { format!("{home}/{rest}") } else { f.to_string() };
    // Allowed: inside your home folder, or a temporary folder (macOS calls /tmp also /private/tmp).
    // Compared by path components, so Windows paths (C:\Users\…) work too.
    let tmp = std::env::temp_dir();
    let roots = [Path::new(&home), tmp.as_path(), Path::new("/tmp"), Path::new("/private/tmp"), Path::new("/private/var/folders")];
    let p = Path::new(&f);
    let inside = roots.iter().any(|r| p.starts_with(r) && p.components().count() > r.components().count());
    if inside && !p.components().any(|c| c == Component::ParentDir) {
        f.trim_end_matches(['/', '\\']).to_string()
    } else {
        fallback
    }
}

fn run_app_actions(shared: &Arc<Shared>, from: &str, actions: &[AppAction]) {
    for a in actions {
        match a {
            AppAction::NewChannel { name, folder } => {
                let name = channel_slug(name);
                if name.is_empty() {
                    continue;
                }
                let folder = channel_folder(&name, folder.as_deref());
                crate::inbox::handle(shared, &crate::inbox::new_channel(&name, &folder));
                let shown = folder.replacen(&model::home_dir(), "~", 1);
                shared.post(from, Author::System, format!("Opened #{name} ({shown})."), false);
            }
            AppAction::Switch(ch) => {
                let id = shared.data.lock().unwrap().channels.iter().find(|c| c.name == channel_slug(ch)).map(|c| c.id.clone());
                match id {
                    Some(id) => {
                        shared.data.lock().unwrap().selected = Some(id);
                        shared.dirty.store(true, Ordering::Relaxed);
                        (shared.repaint)();
                    }
                    None => shared.post(from, Author::System, format!("There's no channel called #{}.", channel_slug(ch)), true),
                }
            }
            AppAction::Send { channel, text } => {
                let name = channel_slug(channel);
                let exists = shared.data.lock().unwrap().channels.iter().any(|c| c.name == name);
                if !exists {
                    shared.post(from, Author::System, format!("There's no channel called #{name}."), true);
                    continue;
                }
                let origin = shared.data.lock().unwrap().channel(from).map(|c| c.name.clone()).unwrap_or_default();
                crate::inbox::handle(shared, &crate::inbox::send(&name, &format!("(Passed on from #{origin}) {text}")));
                shared.post(from, Author::System, format!("Handed over to #{name}."), false);
            }
        }
    }
}

/// Who the user is addressing: `@handle` anywhere, or by voice a leading "Codex, …" / "Hey Claude …".
/// `@all`, "everyone, …" = every member.
pub fn user_mentions(text: &str, members: &[Agent]) -> Vec<String> {
    let mut out = Vec::new();
    for tok in mention_tokens(text) {
        if tok == "all" || tok == "everyone" {
            return members.iter().map(|a| a.id.clone()).collect();
        }
        if let Some(a) = members.iter().find(|a| a.answers_to(&tok)) {
            if !out.contains(&a.id) {
                out.push(a.id.clone());
            }
        }
    }
    if out.is_empty() {
        out = spoken_address(text, members);
    }
    out
}

/// "Codex, run the tests" / "Hey Claude and Codex, …" / "Everyone: status?"
fn spoken_address(text: &str, members: &[Agent]) -> Vec<String> {
    let words: Vec<String> = text
        .split(|c: char| c.is_whitespace())
        .take(6)
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let mut i = 0;
    while i < words.len() && ["hey", "hi", "ok", "okay", "so", "and", "yo", "hello"].contains(&words[i].as_str()) {
        i += 1;
    }
    if i < words.len() && ["everyone", "everybody", "all", "team", "guys"].contains(&words[i].as_str()) {
        return members.iter().map(|a| a.id.clone()).collect();
    }
    let mut out = Vec::new();
    while i < words.len() {
        match members.iter().find(|a| a.answers_to(&words[i])) {
            Some(a) => {
                if !out.contains(&a.id) {
                    out.push(a.id.clone());
                }
                i += 1;
                if i < words.len() && words[i] == "and" {
                    i += 1;
                    continue;
                }
                break;
            }
            None => break,
        }
    }
    out
}

fn mention_tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '-') {
                j += 1;
            }
            if j > i + 1 {
                out.push(chars[i + 1..j].iter().collect::<String>().trim_end_matches('-').to_lowercase());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Does this reply claim that work is starting, ongoing or done? (Checked against what actually happened.)
pub fn claims_work(text: &str, members: &[Agent]) -> bool {
    let t = text.to_lowercase().replace('’', "'");
    const PHRASES: [&str; 29] = [
        "i'll ", "i will ", "on it", "right away", "i'm starting", "starting now", "i'm on ", "i'm going to",
        "i've created", "i've added", "i've updated", "i've fixed", "i've changed", "i have created", "i have updated",
        "i fixed", "i created", "i updated", "working on it", "dive into", "diving into", "will start", "kick off",
        "is working on it", "are working on it", "has started on", "have started on", "i've asked", "i asked",
    ];
    if PHRASES.iter().any(|p| t.contains(p)) {
        return true;
    }
    // Mentions a teammate as if assigning work, without a real handoff.
    members.iter().any(|m| t.contains(&format!("@{}", m.handle.to_lowercase())))
}

/// The reply asks you for something (a question, a link, a choice): it's waiting on you, not promising work.
pub fn asks_user(text: &str) -> bool {
    let t = text.to_lowercase().replace('’', "'");
    const ASKS: [&str; 12] = [
        "send me", "tell me", "give me", "let me know", "could you", "can you", "would you", "which one", "do you want",
        "should i", "please share", "please send",
    ];
    t.contains('?') || ASKS.iter().any(|a| t.contains(a))
}

/// What really happened recently in this channel, for the main agent's prompt.
fn facts(ch: &Channel, me: &Agent, data: &Data, shared: &Shared) -> String {
    let working: Vec<String> = shared
        .runs
        .lock()
        .unwrap()
        .get(&ch.id)
        .map(|r| r.working.iter().filter(|(h, _)| *h != me.handle).map(|(h, st)| format!("@{h} ({st})")).collect())
        .unwrap_or_default();
    let now = model::now();
    let recent: Vec<String> = ch
        .messages
        .iter()
        .rev()
        .filter(|m| matches!(m.author, Author::Agent(_)))
        .take(8)
        .map(|m| {
            let who = match &m.author {
                Author::Agent(id) => data.agent(id).map(|a| a.name().to_string()).unwrap_or_default(),
                _ => String::new(),
            };
            let did = if m.receipt.is_empty() { "no actions".to_string() } else { m.receipt.clone() };
            let flag = if m.idle_claim { " (claimed work but did nothing)" } else { "" };
            format!("- {} min ago, {who}: {did}{flag}", now.saturating_sub(m.ts) / 60)
        })
        .collect();
    format!(
        "\n## Facts from the app (trust these over what anyone said)\n\
         Working right now besides you: {}.\n\
         What agents actually did in their recent turns (newest first):\n{}\n\
         Work only happens when someone runs commands, edits files, or you hand it off with a line \"@name: task\". \
         Never say work is in progress unless it shows up here or you start it in this reply.\n\
         If teammates are still busy with earlier requests, don't wait for them: answer the user's newest message now \
         (do it yourself or hand it to someone who is free).\n",
        if working.is_empty() { "nobody".to_string() } else { working.join(", ") },
        if recent.is_empty() { "- nothing yet".to_string() } else { recent.join("\n") }
    )
}

/// Handoffs in an agent reply: `@codex: do X`, `- **@codex:** do X`, and also `@developer, do X` at the start of a
/// line or sentence (weaker models write it that way). "Thanks @developer, …" mid-sentence is not a handoff.
pub fn delegations(reply: &str, members: &[Agent], me: &str, exclude: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_code = false;
    for line in reply.lines() {
        let l = line.trim();
        if l.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let l = l.trim_start_matches(|c: char| c == '-' || c == '*' || c == '>' || c == '_' || c.is_whitespace());
        // Candidates: the start of the line, and the start of each sentence in it.
        let mut starts = vec![0usize];
        for (i, _) in l.match_indices(['.', '!', '?']) {
            let rest = &l[i + 1..];
            let skip = rest.len() - rest.trim_start().len();
            if skip > 0 {
                starts.push(i + 1 + skip);
            }
        }
        for st in starts {
            let seg = l[st..].trim_start_matches(['*', '_']);
            let Some(rest) = seg.strip_prefix('@') else { continue };
            let handle: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
            let after = rest[handle.len()..].trim_start_matches(['*', '_']);
            if !(after.starts_with(':') || after.starts_with(',')) || after[1..].trim().len() < 3 {
                continue;
            }
            if let Some(a) = members.iter().find(|a| a.answers_to(&handle)) {
                if a.id != me && Some(a.id.as_str()) != exclude && !out.contains(&a.id) {
                    out.push(a.id.clone());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentKind;

    fn agent(id: &str, handle: &str, cmd: &str) -> Agent {
        Agent {
            id: id.into(),
            handle: handle.into(),
            display_name: String::new(),
            kind: AgentKind::Custom,
            model: String::new(),
            full_access: false,
            command: cmd.into(),
            description: String::new(),
            briefing: String::new(),
            voice: String::new(),
            eleven_voice: String::new(),
            effort: String::new(),
            use_my_plugins: false,
            internet: true,
            color: [0, 0, 0],
        }
    }

    /// A command line for a stand-in agent that works on every OS (examples/fake_agent.rs): the first
    /// (needle, reply) whose needle is in the prompt wins; "" always matches; "sleep=N;" waits first.
    fn fake(rules: &[(&str, &str)]) -> String {
        let bin = fake_agent_bin();
        let mut cmd = format!("\"{}\"", bin.display());
        for (needle, reply) in rules {
            cmd.push_str(&format!(" \"{needle}=>{reply}\""));
        }
        cmd
    }

    /// Builds examples/fake_agent once (cargo knows exactly where it puts it, whatever its version or layout).
    fn fake_agent_bin() -> &'static std::path::Path {
        static BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        BIN.get_or_init(|| {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let out = std::process::Command::new(cargo)
                .args(["build", "--quiet", "--example", "fake_agent", "--message-format=json"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .expect("run cargo to build examples/fake_agent");
            let path = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).find_map(|m| {
                (m["reason"] == "compiler-artifact" && m["target"]["name"] == "fake_agent").then(|| m["executable"].as_str().map(PathBuf::from)).flatten()
            });
            path.unwrap_or_else(|| panic!("couldn't build examples/fake_agent: {}", String::from_utf8_lossy(&out.stderr)))
        })
    }

    fn channel(members: &[&str]) -> Channel {
        Channel {
            id: "c".into(),
            name: "test".into(),
            folder: std::env::temp_dir().to_string_lossy().into_owned(),
            members: members.iter().map(|s| s.to_string()).collect(),
            messages: vec![],
            sessions: HashMap::new(),
            seen: HashMap::new(),
            watch: HashMap::new(),
            unread: false,
        }
    }

    fn wait(shared: &Arc<Shared>) -> Vec<(String, String)> {
        let t = Instant::now();
        while shared.runs.lock().unwrap().contains_key("c") {
            assert!(t.elapsed().as_secs() < 20, "timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let data = shared.data.lock().unwrap();
        data.channels[0].messages.iter().map(|m| (format!("{:?}", m.author), m.text.clone())).collect()
    }

    /// Exercise the native cmd /C execution branch, including stdin and stdout.
    #[cfg(windows)]
    #[test]
    fn windows_custom_command_replies() {
        let lead = agent("L", "lead", "more >nul & echo WINDOWS_OK");
        let data = Data { agents: vec![lead], channels: vec![channel(&["L"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "hello".into());
        let msgs = wait(&shared);
        assert_eq!(msgs.last().unwrap().1, "WINDOWS_OK");
        assert!(!shared.data.lock().unwrap().channels[0].messages.last().unwrap().error);
    }

    #[test]
    fn catches_handoffs_weaker_models_write() {
        let m = vec![agent("a", "main", ""), agent("b", "developer", "")];
        // The exact reply from the screenshot:
        let r = "We can add optional 11 Labs voice support. @developer, create a toggle in the audio settings.";
        assert_eq!(delegations(r, &m, "a", None), vec!["b".to_string()]);
        assert!(delegations("Thanks @developer, that looks good.", &m, "a", None).is_empty());
        assert!(claims_work("Got it—I'll dive into the Windows test failure right away.", &m));
        assert!(claims_work("I asked @developer to look at it.", &m));
        assert!(!claims_work("A harness is the software around an AI.", &m));
        // Facts about the world aren't promises (the reply from the Turin screenshot).
        assert!(!claims_work("In past years the two semis have started around early afternoon, Turin time.", &m));
        assert!(claims_work("The developer is working on it now.", &m));
        // Waiting on the user, not an empty promise (a real reply).
        assert!(asks_user("Send me a link, or give the agents access to its folder, and I'll review it."));
        assert!(!asks_user("On it, I'll start right away."));
        assert!(!claims_work("Sure, let me know which file.", &m));
    }

    /// The lead promises work but does nothing: the app flags it and sends it back once; then it hands off for real.
    #[test]
    fn empty_promises_are_sent_back() {
        let lead = agent(
            "L",
            "lead",
            &fake(&[("NOTHING IS HAPPENING", "@helper: add the toggle"), ("have replied above", "Done: toggle added."), ("", "On it, I'll start right away.")]),
        );
        let helper = agent("H", "helper", &fake(&[("", "Added the toggle.")]));
        let data = Data { agents: vec![lead, helper], channels: vec![channel(&["L", "H"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "add a toggle".into());
        let msgs = wait(&shared);
        assert!(msgs.iter().any(|(_, t)| t.contains("nothing was run, edited or handed off")), "{msgs:#?}");
        assert!(msgs.iter().any(|(_, t)| t == "Added the toggle."), "helper never ran: {msgs:#?}");
        let data = shared.data.lock().unwrap();
        let first = data.channels[0].messages.iter().find(|m| m.text.starts_with("On it")).unwrap();
        assert!(first.idle_claim);
        let handoff = data.channels[0].messages.iter().find(|m| m.text.starts_with("@helper")).unwrap();
        assert!(handoff.receipt.contains("handed off to helper") && !handoff.idle_claim, "{}", handoff.receipt);
    }

    /// You ask something quick while a teammate is busy with a slow task: the lead answers right away.
    #[test]
    fn answers_new_messages_while_teammates_work() {
        let lead = agent(
            "L",
            "lead",
            &fake(&[("have replied above", "All done"), ("quick question", "quick answer"), ("", "@helper: slow task")]),
        );
        let helper = agent("H", "helper", &fake(&[("", "sleep=4;slow done")]));
        let data = Data { agents: vec![lead, helper], channels: vec![channel(&["L", "H"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "do the slow thing".into());
        // Wait until the slow task has actually been handed off (the lead's first turn is done).
        let t = Instant::now();
        while !shared.data.lock().unwrap().channels[0].messages.iter().any(|m| m.text.contains("@helper: slow task")) {
            assert!(t.elapsed().as_secs() < 10, "lead never handed off");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        send(&shared, "c", "quick question: is the domain free?".into());
        let msgs = wait(&shared);
        let pos = |t: &str| msgs.iter().position(|(_, m)| m == t).unwrap_or_else(|| panic!("missing {t}: {msgs:#?}"));
        assert!(pos("quick answer") < pos("slow done"), "the quick question waited for the slow task: {msgs:#?}");
        assert_eq!(msgs.iter().filter(|(_, m)| m == "All done").count(), 1);
    }

    /// The lead is still busy when you send something else: a General helper takes it instead of making you wait.
    #[test]
    fn follow_up_restarts_the_lead_with_both_messages() {
        // "Look up Moon pace" … "Moonbase." The lead starts over and answers both together; no helper answers
        // the correction on its own.
        let lead = agent("L", "main", &fake(&[("Moonbase.", "Found Moonbase"), ("", "sleep=4;answered the first only")]));
        let data = Data { agents: vec![lead], channels: vec![channel(&["L"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "Can you look up Moon pace, my project?".into());
        std::thread::sleep(std::time::Duration::from_millis(800));
        send(&shared, "c", "Moonbase.".into());
        let msgs = wait(&shared);
        assert!(msgs.iter().any(|(_, m)| m.contains("starting over with both")), "{msgs:#?}");
        assert!(msgs.iter().any(|(_, m)| m == "Found Moonbase"), "{msgs:#?}");
        assert!(!msgs.iter().any(|(_, m)| m.contains("first only") || m.contains("General")), "{msgs:#?}");
        assert!(!msgs.iter().any(|(_, m)| m.contains("(stopped)")), "the stopped turn stays silent: {msgs:#?}");
    }

    #[test]
    fn general_helper_steps_in_when_busy() {
        let lead = agent("L", "main", &fake(&[("Your role: General", "second done"), ("", "sleep=2;first done")]));
        let data = Data { agents: vec![lead], channels: vec![channel(&["L"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "first, a slow one".into());
        std::thread::sleep(std::time::Duration::from_millis(500));
        // A separate request, a while later (a quick follow-up would restart the lead instead).
        shared.data.lock().unwrap().channels[0].messages.iter_mut().filter(|m| m.author == Author::User).for_each(|m| m.ts -= 120);
        send(&shared, "c", "second, quick".into());
        let msgs = wait(&shared);
        let pos = |t: &str| msgs.iter().position(|(_, m)| m == t).unwrap_or_else(|| panic!("missing {t}: {msgs:#?}"));
        assert!(msgs.iter().any(|(_, m)| m.contains("is busy, so General is taking this")), "{msgs:#?}");
        assert!(pos("second done") < pos("first done"), "{msgs:#?}");
        let data = shared.data.lock().unwrap();
        let g = data.agents.iter().find(|a| a.handle == "general").expect("General created");
        assert!(data.channels[0].members.contains(&g.id));
        assert_eq!(g.kind, AgentKind::Custom, "runs on the main agent's engine");
    }

    #[test]
    fn step_caps_by_role() {
        let d = Data::default();
        assert_eq!(d.agents[0].step_cap(), 8, "main agent routes, it shouldn't investigate");
        assert_eq!(d.agents[1].step_cap(), 30);
    }

    #[test]
    fn router_prompt_is_compact_and_escalates_with_work() {
        let mut data = Data::default();
        for i in 0..40 {
            data.channels[0].messages.push(Message::system(format!("note {i}")));
            data.channels[0].messages.push(Message { author: Author::User, text: "x".repeat(2000), ts: 0, error: false, to_user: false, via: String::new(), attachments: Vec::new(), receipt: String::new(), idle_claim: false });
        }
        let shared = Shared::quiet(data.clone());
        let ch = &data.channels[0];
        let members = data.members(ch);
        let p = build_router_prompt(ch, &members[0], &members, &data, &Mode::User, true, &shared);
        assert!(p.contains("@writer") && p.contains("## Facts from the app"), "{p}");
        assert!(p.len() < 12_000, "router prompt should stay small, got {} chars", p.len());
        assert!(agents::wants_full_agent("/work"));
        assert!(agents::wants_full_agent("I need to look.\n`/work`"));
        assert!(!agents::wants_full_agent("I'll work on it via @developer"));
    }

    #[test]
    fn finds_delegations() {
        let m = vec![agent("a", "claude", ""), agent("b", "codex", "")];
        let r = "Plan:\n- **@codex:** write tests\n@claude: me\n```\n@codex: not this\n```\nTalk to @codex later";
        assert_eq!(delegations(r, &m, "a", None), vec!["b".to_string()]);
        assert!(delegations("@codex: x", &m, "a", Some("b")).is_empty());
        assert!(delegations("@codex:", &m, "a", None).is_empty());
    }

    #[test]
    fn main_agent_answers_to_alex() {
        let data = Data::default();
        let main = data.agents[0].id.clone();
        assert_eq!(data.agents[0].name(), "Alex");
        for said in ["Alex, write a blog post", "Hey Alex, what's new?", "@alex status?", "@main status?", "OK alex and writer, go"] {
            assert!(user_mentions(said, &data.agents).contains(&main), "{said}");
        }
        assert!(user_mentions("Alexander the Great", &data.agents).is_empty());
        let writer = data.agents.iter().find(|a| a.handle == "writer").unwrap().id.clone();
        assert_eq!(delegations("@Alex: please review", &data.agents, &writer, None), vec![main]);
        assert_eq!(data.agents[0].label(), "Alex (@main)");
    }

    #[test]
    fn finds_user_mentions() {
        let m = vec![agent("a", "claude", ""), agent("b", "codex", "")];
        assert_eq!(user_mentions("hey @Codex, look", &m), vec!["b".to_string()]);
        assert_eq!(user_mentions("@all status?", &m).len(), 2);
        assert!(user_mentions("mail me at x@codex.com", &m).is_empty());
        // Spoken
        assert_eq!(user_mentions("Codex, run the tests.", &m), vec!["b".to_string()]);
        assert_eq!(user_mentions("Hey Claude and Codex, status?", &m), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(user_mentions("Everyone, what are you doing?", &m).len(), 2);
        assert!(user_mentions("Run the tests and tell codex later", &m).is_empty());
    }

    /// Real processes: a lead that delegates to a helper, then wraps up.
    #[test]
    fn lead_delegates_then_wraps_up() {
        let lead = agent(
            "L",
            "lead",
            &fake(&[("have replied above", "Final: helper says {last:ANSWER=}"), ("", "On it.\\n@helper: compute 2+2")]),
        );
        let helper = agent("H", "helper", &fake(&[("", "ANSWER=4")]));
        let data = Data { agents: vec![lead, helper], channels: vec![channel(&["L", "H"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "what is 2+2?".into());
        let msgs = wait(&shared);
        assert_eq!(msgs.len(), 4, "{msgs:#?}");
        assert!(msgs[1].1.contains("@helper: compute 2+2"));
        assert_eq!(msgs[2].1, "ANSWER=4");
        assert_eq!(msgs[3].1, "Final: helper says ANSWER=4");
        assert!(shared.activity.lock().unwrap()["c"].contains("helper"));
    }

    /// Two teammates briefed in one message work at the same time.
    #[test]
    fn handoffs_run_in_parallel() {
        // Each helper marks that it started, waits, then checks whether the other one started too.
        // Run one after another, neither would see the other; in parallel, both do.
        let dir = std::env::temp_dir().join(format!("yh-par-{}", model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("one").display().to_string(), dir.join("two").display().to_string());
        let lead = agent("L", "lead", &fake(&[("have replied above", "All done"), ("", "@one: task A\\n@two: task B")]));
        let one = agent("1", "one", &fake(&[("", &format!("touch={a};sleep=2;{{file:{b}?A saw B|A alone}}"))]));
        let two = agent("2", "two", &fake(&[("", &format!("touch={b};sleep=2;{{file:{a}?B saw A|B alone}}"))]));
        let data = Data { agents: vec![lead, one, two], channels: vec![channel(&["L", "1", "2"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "do A and B".into());
        let msgs = wait(&shared);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(msgs.iter().any(|(_, m)| m == "A saw B") && msgs.iter().any(|(_, m)| m == "B saw A"), "handoffs didn't overlap: {msgs:#?}");
        assert_eq!(msgs.last().unwrap().1, "All done");
    }

    #[test]
    fn parses_app_actions() {
        let r = "Sure, setting that up.\n/new-channel Recipe Site ~/Development/recipes\n`/send recipe-site Build a recipe website`\n```\n/switch nope\n```";
        let (text, actions) = app_actions(r);
        assert_eq!(text, "Sure, setting that up.\n```\n/switch nope\n```");
        assert_eq!(
            actions,
            vec![
                AppAction::NewChannel { name: "Recipe Site".into(), folder: Some("~/Development/recipes".into()) },
                AppAction::Send { channel: "recipe-site".into(), text: "Build a recipe website".into() },
            ]
        );
        assert_eq!(channel_slug("My Site!"), "my-site");
        let home = model::home_dir();
        assert_eq!(channel_folder("x", None), format!("{home}/Development/x"));
        assert_eq!(channel_folder("x", Some("~/Projects/x")), format!("{home}/Projects/x"));
        assert_eq!(channel_folder("x", Some("/etc")), format!("{home}/Development/x"));
        assert_eq!(channel_folder("x", Some("/private/tmp/a/x")), "/private/tmp/a/x");
        let (_, a) = app_actions("/new-channel recipes C:\\Users\\me\\recipes\n/new-channel Two Words \"D:/work/site\"");
        assert_eq!(
            a,
            vec![
                AppAction::NewChannel { name: "recipes".into(), folder: Some("C:\\Users\\me\\recipes".into()) },
                AppAction::NewChannel { name: "Two Words".into(), folder: Some("D:/work/site".into()) },
            ]
        );
        assert_eq!(channel_folder("x", Some(&format!("{home}/../etc"))), format!("{home}/Development/x"));
        assert_eq!(channel_folder("x", Some(&home)), format!("{home}/Development/x"), "not the home folder itself");
    }

    /// The main agent creates a channel and hands the work over; the team there starts on it.
    #[test]
    fn main_agent_creates_channel_and_hands_over() {
        let dir = std::env::temp_dir().join(format!("yh-newch-{}", model::new_id()));
        let lead = agent(
            "L",
            "lead",
            &fake(&[
                ("Passed on from", "Starting on recipes"),
                ("", &format!("Opening a channel for that.\\n/new-channel recipes {}\\n/send recipes Build a recipe website", dir.display())),
            ]),
        );
        let data = Data { agents: vec![lead], channels: vec![channel(&["L"])], selected: Some("c".into()), settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "make a new channel for a recipe site".into());
        let msgs = wait(&shared);
        assert!(msgs.iter().any(|(_, t)| t == "Opening a channel for that."), "{msgs:#?}");
        assert!(msgs.iter().any(|(_, t)| t.starts_with("Opened #recipes")), "{msgs:#?}");
        assert!(!msgs.iter().any(|(_, t)| t.contains("/new-channel")));
        // The new channel exists, is open, and its team got going.
        let t = Instant::now();
        loop {
            let done = {
                let data = shared.data.lock().unwrap();
                let ch = data.channels.iter().find(|c| c.name == "recipes").unwrap_or_else(|| {
                    let all: Vec<(&str, &str)> = data.channels.iter().map(|c| (c.name.as_str(), c.folder.as_str())).collect();
                    panic!("channel not created; channels: {all:?}; messages: {msgs:#?}")
                });
                assert_eq!(data.selected.as_deref(), Some(ch.id.as_str()));
                ch.messages.iter().any(|m| m.text == "Starting on recipes")
            };
            if done {
                break;
            }
            assert!(t.elapsed().as_secs() < 15, "team in the new channel never started");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A Claude role whose engine is logged out runs on Codex instead, keeping its identity.
    #[test]
    fn role_falls_back_to_other_engine() {
        let mut writer = agent("W", "writer", "");
        writer.kind = AgentKind::Claude;
        writer.briefing = "Write blog posts.".into();
        let data = Data { agents: vec![writer.clone()], channels: vec![channel(&["W"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        let ok = Health { installed: true, logged_in: Some(true), ..Default::default() };
        let out = Health { installed: true, logged_in: Some(false), ..Default::default() };
        shared.health.lock().unwrap().insert("Claude Code".into(), out);
        assert!(shared.effective(&writer).is_none(), "no known-good stand-in yet");
        shared.health.lock().unwrap().insert("Codex".into(), ok);
        let eff = shared.effective(&writer).unwrap();
        assert_eq!(eff.kind, AgentKind::Codex);
        assert_eq!(eff.handle, "writer");
        assert_eq!(eff.briefing, "Write blog posts.");
        assert_ne!(session_key(&writer), session_key(&eff));
    }

    /// The main agent is told it routes work; specialists get their briefing.
    #[test]
    fn prompts_carry_roles_and_briefings() {
        let data = Data::default();
        let shared = Shared::quiet(data.clone());
        let ch = &data.channels[0];
        let members = data.members(ch);
        let main = preamble(ch, &members[0], &members, &shared);
        assert!(main.contains("You are the MAIN agent"), "{main}");
        assert!(main.contains("@writer: Blog post writer"), "{main}");
        let writer = members.iter().find(|a| a.handle == "writer").unwrap();
        let w = preamble(ch, writer, &members, &shared);
        assert!(w.contains("## Your briefing") && w.contains("docs/blog/"), "{w}");
        assert!(w.contains("Alex (@main) (main agent)"), "{w}");
    }

    /// "Everyone" with one agent out of quota: the other answers once, not twice.
    #[test]
    fn stand_in_answers_once() {
        let lead = agent("L", "lead", &fake(&[("", "fail;You've hit your usage limit")]));
        let backup = agent("B", "backup", &fake(&[("", "backup here")]));
        let data = Data { agents: vec![lead, backup], channels: vec![channel(&["L", "B"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "Everyone, status?".into());
        let msgs = wait(&shared);
        assert_eq!(msgs.iter().filter(|(_, t)| t == "backup here").count(), 1, "{msgs:#?}");
    }

    /// The lead is out of quota: the other agent answers instead.
    #[test]
    fn fails_over_when_lead_hits_its_limit() {
        let lead = agent("L", "lead", &fake(&[("", "fail;You've hit your usage limit")]));
        let backup = agent("B", "backup", &fake(&[("", "backup here")]));
        let data = Data { agents: vec![lead, backup], channels: vec![channel(&["L", "B"])], selected: None, settings: Default::default() };
        let shared = Shared::quiet(data);
        send(&shared, "c", "hello".into());
        let msgs = wait(&shared);
        assert!(msgs.iter().any(|(_, t)| t.contains("backup is taking over")), "{msgs:#?}");
        assert_eq!(msgs.last().unwrap().1, "backup here");
        // Next message skips the blocked lead straight away.
        send(&shared, "c", "again".into());
        let msgs = wait(&shared);
        assert!(msgs.iter().any(|(_, t)| t.contains("lead is usage limit reached") || t.contains("so backup is taking this")), "{msgs:#?}");
        assert_eq!(msgs.last().unwrap().1, "backup here");
    }

    #[test]
    fn memory_goes_before_your_turn() {
        let mut p = "## Conversation so far\n\n**User:** hi\n\n## Your turn\nRespond.".to_string();
        insert_memory(&mut p, "\n## Long-term memory\n- Likes short answers\n");
        assert_eq!(p, "## Conversation so far\n\n**User:** hi\n\n## Long-term memory\n- Likes short answers\n\n## Your turn\nRespond.");
        let before = p.clone();
        insert_memory(&mut p, "  \n");
        assert_eq!(p, before, "nothing to add");
    }
}
