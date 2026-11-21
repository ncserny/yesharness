//! What can each agent do right now? Asks the CLIs themselves:
//! - Codex: `codex app-server` → `model/list` + `account/rateLimits/read` (plan, 5h/weekly usage)
//! - Claude Code: stream-json `initialize` control request → models + whether you're logged in;
//!   usage limits arrive as `rate_limit_event`s during each run.
//! The result drives automatic model choice and failover between agents.

use crate::env;
use crate::model::{now, Agent, AgentKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub hidden: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Limit {
    /// "5h", "week", "opus week", ...
    pub label: String,
    pub used_pct: f32,
    pub resets_at: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Health {
    pub installed: bool,
    pub logged_in: Option<bool>,
    pub plan: Option<String>,
    pub models: Vec<ModelInfo>,
    pub limits: Vec<Limit>,
    /// Limit reached: unusable until this unix time.
    pub blocked_until: Option<u64>,
    pub blocked_reason: Option<String>,
    /// Models that failed with "not supported" for this account.
    pub bad_models: Vec<String>,
    pub error: Option<String>,
    /// Can't run for another reason (e.g. Ollama isn't running, no local model).
    pub down: Option<String>,
}

impl Health {
    pub fn blocked(&self) -> bool {
        self.blocked_until.is_some_and(|t| t > now())
    }

    /// Usable right now, as far as we know.
    pub fn available(&self) -> bool {
        self.installed && self.logged_in != Some(false) && !self.blocked() && self.down.is_none()
    }

    /// Short reason it's not available, for chat notes.
    pub fn why_unavailable(&self) -> Option<String> {
        if !self.installed {
            Some("not installed".into())
        } else if let Some(d) = &self.down {
            Some(d.clone())
        } else if self.logged_in == Some(false) {
            Some("not logged in".into())
        } else if self.blocked() {
            let when = self.blocked_until.map(|t| format!(", resets {}", when(t))).unwrap_or_default();
            Some(format!("{}{when}", self.blocked_reason.clone().unwrap_or_else(|| "usage limit reached".into())))
        } else {
            None
        }
    }

    /// The most-used limit window, for the sidebar.
    pub fn busiest(&self) -> Option<&Limit> {
        self.limits.iter().max_by(|a, b| a.used_pct.total_cmp(&b.used_pct))
    }
}

/// Codex models picked automatically before Codex's own default, cheapest-that's-good-enough first.
const CODEX_PREFERRED: [&str; 1] = ["gpt-6-sol"];

/// Claude/Codex share one account per kind; custom agents are tracked one by one.
pub fn key(a: &Agent) -> String {
    match a.kind {
        AgentKind::Custom => format!("custom:{}", a.id),
        k => k.label().to_string(),
    }
}

/// Which model to use for this agent right now. `None` = let the CLI use its own default.
pub fn pick_model(agent: &Agent, h: Option<&Health>) -> Option<String> {
    let m = agent.model.trim();
    if !m.is_empty() && !m.eq_ignore_ascii_case("auto") {
        return Some(m.to_string());
    }
    let h = h?;
    match agent.kind {
        AgentKind::Codex => {
            let ok = |m: &&ModelInfo| !h.bad_models.contains(&m.id);
            // Passing the model explicitly also fixes a broken `model =` in ~/.codex/config.toml.
            // Preferred: GPT-6-Sol, much cheaper on your quota than the default (Astra); then Codex's default.
            h.models
                .iter()
                .filter(ok)
                .find(|m| CODEX_PREFERRED.contains(&m.id.as_str()))
                .or_else(|| h.models.iter().filter(ok).find(|m| m.is_default))
                .or_else(|| h.models.iter().filter(ok).find(|m| !m.hidden))
                .map(|m| m.id.clone())
        }
        AgentKind::Claude => {
            // Weekly Opus limit used up → Sonnet still works.
            let opus_out = h.limits.iter().any(|l| l.label.contains("opus") && l.used_pct >= 99.0);
            let first_ok = |pref: &str| {
                h.models.iter().find(|m| m.id == pref && !h.bad_models.contains(&m.id)).map(|m| m.id.clone())
            };
            if opus_out || h.bad_models.iter().any(|b| b == "default" || b == "opus") {
                first_ok("sonnet").or_else(|| first_ok("haiku"))
            } else {
                None
            }
        }
        AgentKind::Local => {
            let ok = |m: &&ModelInfo| !h.bad_models.contains(&m.id);
            h.models.iter().filter(ok).find(|m| m.is_default).or_else(|| h.models.iter().find(|m| !h.bad_models.contains(&m.id))).map(|m| m.id.clone())
        }
        AgentKind::Custom => None,
    }
}

/// Local models we know work well as agents, best first (substring match on the Ollama name).
const LOCAL_PREFERENCE: [&str; 8] = ["gpt-oss:20b", "qwen3-coder", "qwen3.6", "qwen3.8", "devstral", "gpt-oss", "qwen3", "gemma"];

fn probe_local(h: &mut Health) {
    h.installed = h.installed && env::which("codex").is_some();
    if !h.installed {
        return;
    }
    let mut tags = ollama_tags();
    if tags.is_none() {
        // Not running: start it, give it a few seconds.
        start_ollama();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            tags = ollama_tags();
            if tags.is_some() {
                break;
            }
        }
    }
    let Some(tags) = tags else {
        h.down = Some("Ollama isn't running".into());
        return;
    };
    h.logged_in = Some(true);
    h.plan = Some("on this Mac, free".into());
    let mut names: Vec<String> = tags["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["name"].as_str().map(String::from))
        .filter(|n| !n.contains("embed") && !n.starts_with("bge") && !n.starts_with("nomic") && !n.contains("minilm"))
        .collect();
    let rank = |n: &String| LOCAL_PREFERENCE.iter().position(|p| n.contains(p)).unwrap_or(LOCAL_PREFERENCE.len());
    names.sort_by_key(rank);
    if names.is_empty() {
        h.down = Some("no local model installed (ollama pull gpt-oss:20b)".into());
    }
    h.models = names.iter().enumerate().map(|(i, n)| ModelInfo { id: n.clone(), name: n.clone(), is_default: i == 0, hidden: false }).collect();
}

/// Starts the Ollama server in the background (the Ollama app on macOS).
pub fn start_ollama() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").args(["-g", "-a", "Ollama"]).status();
    #[cfg(not(target_os = "macos"))]
    let _ = env::command("ollama").arg("serve").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// GET http://127.0.0.1:11434/api/tags without an HTTP library.
pub fn ollama_tags() -> Option<Value> {
    use std::io::Read;
    let mut s = std::net::TcpStream::connect_timeout(&"127.0.0.1:11434".parse().ok()?, Duration::from_millis(800)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    s.write_all(b"GET /api/tags HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    let body = buf.split_once("\r\n\r\n")?.1;
    serde_json::from_str(body).ok()
}

pub fn probe(kind: AgentKind) -> Health {
    let mut h = Health::default();
    let Some(program) = kind.program() else {
        h.installed = true;
        return h;
    };
    h.installed = env::which(program).is_some();
    if !h.installed {
        return h;
    }
    match kind {
        AgentKind::Codex => probe_codex(&mut h),
        AgentKind::Claude => probe_claude(&mut h),
        AgentKind::Local => probe_local(&mut h),
        AgentKind::Custom => {}
    }
    h
}

/// Run `program args`, write `requests` as JSON lines, collect replies until `done` says so.
fn converse(program: &str, args: &[&str], requests: &[Value], mut done: impl FnMut(&Value) -> bool) -> Result<(), String> {
    let mut child = env::command(program)
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut watchdog = env::guard(child.id());
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    for r in requests {
        let _ = writeln!(stdin, "{r}");
    }
    let _ = stdin.flush();
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut result = Err("timed out".to_string());
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(v) => {
                if done(&v) {
                    result = Ok(());
                    break;
                }
            }
            Err(_) => break,
        }
    }
    drop(stdin);
    let pid = child.id();
    env::kill_tree(pid);
    let _ = child.kill();
    let _ = child.wait();
    if let Some(w) = watchdog.as_mut() {
        let _ = w.kill();
        let _ = w.wait();
    }
    result
}

fn probe_codex(h: &mut Health) {
    let reqs = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"yesharness","version":env!("CARGO_PKG_VERSION")}}}),
        json!({"jsonrpc":"2.0","method":"initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"account/rateLimits/read"}),
        json!({"jsonrpc":"2.0","id":3,"method":"model/list","params":{}}),
    ];
    let (mut got_limits, mut got_models) = (false, false);
    let res = converse("codex", &["app-server"], &reqs, |v| {
        match v["id"].as_i64() {
            Some(2) => {
                got_limits = true;
                if v.get("error").is_some() {
                    let msg = v["error"]["message"].as_str().unwrap_or_default().to_lowercase();
                    if msg.contains("auth") || msg.contains("login") || msg.contains("logged") {
                        h.logged_in = Some(false);
                    }
                } else {
                    h.logged_in = Some(true);
                    parse_codex_limits(&v["result"], h);
                }
            }
            Some(3) => {
                got_models = true;
                for m in v["result"]["data"].as_array().into_iter().flatten() {
                    h.models.push(ModelInfo {
                        id: m["id"].as_str().unwrap_or_default().to_string(),
                        name: m["displayName"].as_str().unwrap_or_default().to_string(),
                        is_default: m["isDefault"].as_bool().unwrap_or(false),
                        hidden: m["hidden"].as_bool().unwrap_or(false),
                    });
                }
            }
            _ => {}
        }
        got_limits && got_models
    });
    if let Err(e) = res {
        h.error = Some(format!("couldn't query Codex: {e}"));
    }
}

pub fn parse_codex_limits(r: &Value, h: &mut Health) {
    let rl = &r["rateLimits"];
    h.plan = rl["planType"].as_str().map(String::from);
    h.limits.clear();
    for key in ["primary", "secondary"] {
        let w = &rl[key];
        if w.is_null() {
            continue;
        }
        let mins = w["windowDurationMins"].as_u64().unwrap_or(0);
        let label = match mins {
            0 => key.to_string(),
            m if m <= 60 * 24 => format!("{}h", m / 60),
            m if m >= 60 * 24 * 7 => "week".to_string(),
            m => format!("{}d", m / 60 / 24),
        };
        h.limits.push(Limit { label, used_pct: w["usedPercent"].as_f64().unwrap_or(0.0) as f32, resets_at: w["resetsAt"].as_u64() });
    }
    let reached = !rl["rateLimitReachedType"].is_null() || r["ordinaryUsageAllowed"].as_bool() == Some(false);
    if reached {
        let until = h.limits.iter().filter(|l| l.used_pct >= 99.0).filter_map(|l| l.resets_at).max();
        h.blocked_until = Some(until.unwrap_or(now() + 15 * 60));
        h.blocked_reason = Some(rl["rateLimitReachedType"].as_str().unwrap_or("usage limit reached").replace('_', " "));
    } else {
        h.blocked_until = None;
        h.blocked_reason = None;
    }
}

fn probe_claude(h: &mut Health) {
    let reqs = [json!({"type":"control_request","request_id":"yh-init","request":{"subtype":"initialize"}})];
    let res = converse(
        "claude",
        &["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"],
        &reqs,
        |v| {
            if v["type"] != "control_response" {
                return false;
            }
            let r = if v["response"]["response"].is_object() { &v["response"]["response"] } else { &v["response"] };
            for m in r["models"].as_array().into_iter().flatten() {
                let id = m["value"].as_str().unwrap_or_default().to_string();
                h.models.push(ModelInfo {
                    is_default: id == "default",
                    name: m["displayName"].as_str().unwrap_or_default().to_string(),
                    id,
                    hidden: false,
                });
            }
            let acct = &r["account"];
            let source = acct["tokenSource"].as_str().unwrap_or("");
            let has_key = acct["apiKeySource"].as_str().is_some_and(|s| s != "none");
            h.logged_in = Some(!(source == "none" && !has_key) || !acct.is_object());
            h.plan = acct["subscriptionType"].as_str().map(String::from);
            true
        },
    );
    if let Err(e) = res {
        h.error = Some(format!("couldn't query Claude Code: {e}"));
    }
}

/// Claude's `rate_limit_event.rate_limit_info` → our limits.
pub fn apply_claude_rate_info(info: &Value, h: &mut Health) {
    let pct = |v: &Value| {
        let x = v.as_f64().unwrap_or(0.0);
        (if x <= 1.0 { x * 100.0 } else { x }) as f32
    };
    let mut seen = Vec::new();
    if let Some(w) = info["unifiedWindows"].as_object() {
        for (k, v) in w {
            let label = claude_window_label(k);
            seen.push(label.clone());
            upsert(h, Limit { label, used_pct: pct(&v["utilization"]), resets_at: v["resetsAt"].as_u64() });
        }
    }
    if let Some(t) = info["rateLimitType"].as_str() {
        let label = claude_window_label(t);
        if !seen.contains(&label) && !info["utilization"].is_null() {
            upsert(h, Limit { label, used_pct: pct(&info["utilization"]), resets_at: info["resetsAt"].as_u64() });
        }
    }
    if info["status"] == "rejected" {
        h.blocked_until = Some(info["resetsAt"].as_u64().unwrap_or(now() + 15 * 60));
        h.blocked_reason = Some(format!("{} limit reached", info["rateLimitType"].as_str().map(claude_window_label).unwrap_or_else(|| "usage".into())));
    } else if info["status"].is_string() {
        h.blocked_until = None;
        h.blocked_reason = None;
    }
}

fn claude_window_label(k: &str) -> String {
    match k {
        "five_hour" => "5h".into(),
        "seven_day" => "week".into(),
        "seven_day_opus" => "opus week".into(),
        "seven_day_sonnet" => "sonnet week".into(),
        other => other.replace('_', " "),
    }
}

fn upsert(h: &mut Health, l: Limit) {
    match h.limits.iter_mut().find(|x| x.label == l.label) {
        Some(x) => *x = l,
        None => h.limits.push(l),
    }
}

/// "in 2h 10m" / "at 14:30"-ish relative time.
pub fn when(t: u64) -> String {
    let n = now();
    if t <= n {
        return "now".into();
    }
    let s = t - n;
    if s < 3600 {
        format!("in {}m", s.div_ceil(60))
    } else if s < 86400 {
        format!("in {}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("in {}d", s.div_ceil(86400))
    }
}

/// What kind of failure an agent reply was.
#[derive(PartialEq, Debug)]
pub enum Failure {
    Auth,
    Limit,
    /// The model name that was rejected, if we could tell.
    Model(Option<String>),
    Other,
}

pub fn classify(text: &str) -> Failure {
    let t = text.to_lowercase();
    if t.contains("not logged in") || t.contains("failed to authenticate") || t.contains("/login") || t.contains("unauthorized") || t.contains("401") || t.contains("invalid api key") {
        Failure::Auth
    } else if t.contains("usage limit") || t.contains("rate limit") || t.contains("limit reached") || t.contains("hit your limit") || t.contains("429") || t.contains("quota") || t.contains("credits") || t.contains("too many requests") {
        Failure::Limit
    } else if t.contains("model") && (t.contains("not supported") || t.contains("does not exist") || t.contains("not found") || t.contains("not available")) {
        // e.g. "The 'gpt-6.1-sol' model is not supported ..."
        let name = text.split('\'').nth(1).filter(|s| !s.contains(' ')).map(String::from);
        Failure::Model(name)
    } else {
        Failure::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_limits_and_model_pick() {
        let r = json!({"ordinaryUsageAllowed":true,"rateLimits":{"planType":"plus","rateLimitReachedType":null,
            "primary":{"usedPercent":55,"windowDurationMins":300,"resetsAt":1},"secondary":{"usedPercent":29,"windowDurationMins":10080,"resetsAt":2}}});
        let mut h = Health { installed: true, ..Default::default() };
        parse_codex_limits(&r, &mut h);
        assert_eq!(h.plan.as_deref(), Some("plus"));
        assert_eq!(h.limits[0].label, "5h");
        assert_eq!(h.limits[1].label, "week");
        assert!(h.available());
        h.models = vec![
            ModelInfo { id: "a".into(), is_default: true, ..Default::default() },
            ModelInfo { id: "b".into(), ..Default::default() },
        ];
        let mut agent = crate::model::Data::default().agents[1].clone();
        assert_eq!(pick_model(&agent, Some(&h)).as_deref(), Some("a"));
        h.bad_models.push("a".into());
        assert_eq!(pick_model(&agent, Some(&h)).as_deref(), Some("b"));
        agent.model = "manual".into();
        assert_eq!(pick_model(&agent, Some(&h)).as_deref(), Some("manual"));
    }

    #[test]
    fn local_model_pick() {
        let mut h = Health { installed: true, logged_in: Some(true), ..Default::default() };
        h.models = vec![
            ModelInfo { id: "gpt-oss:20b".into(), is_default: true, ..Default::default() },
            ModelInfo { id: "gemma4:latest".into(), ..Default::default() },
        ];
        let mut a = crate::model::Data::default().agents[0].clone();
        a.kind = AgentKind::Local;
        assert_eq!(pick_model(&a, Some(&h)).as_deref(), Some("gpt-oss:20b"));
        h.bad_models.push("gpt-oss:20b".into());
        assert_eq!(pick_model(&a, Some(&h)).as_deref(), Some("gemma4:latest"));
        h.down = Some("Ollama isn't running".into());
        assert!(!h.available());
    }

    #[test]
    #[ignore] // needs Ollama running: cargo test -- --ignored
    fn probes_ollama() {
        let h = probe(AgentKind::Local);
        assert!(h.available(), "{h:?}");
        assert!(!h.models.is_empty());
    }

    #[test]
    fn codex_prefers_sol_over_the_default() {
        let mut h = Health { installed: true, logged_in: Some(true), ..Default::default() };
        h.models = vec![
            ModelInfo { id: "gpt-6-astra".into(), is_default: true, ..Default::default() },
            ModelInfo { id: "gpt-6-sol".into(), ..Default::default() },
        ];
        let agent = crate::model::Data::default().agents[1].clone();
        assert_eq!(pick_model(&agent, Some(&h)).as_deref(), Some("gpt-6-sol"));
        h.bad_models.push("gpt-6-sol".into());
        assert_eq!(pick_model(&agent, Some(&h)).as_deref(), Some("gpt-6-astra"), "falls back to the default");
    }

    #[test]
    fn classifies_failures() {
        assert_eq!(classify("Not logged in · Please run /login"), Failure::Auth);
        assert_eq!(classify("You've hit your usage limit"), Failure::Limit);
        assert_eq!(
            classify("The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."),
            Failure::Model(Some("gpt-6.1-sol".into()))
        );
    }

    #[test]
    fn claude_rate_info() {
        let mut h = Health::default();
        apply_claude_rate_info(&json!({"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.91,"resetsAt":99}), &mut h);
        assert_eq!(h.limits[0].label, "5h");
        assert!((h.limits[0].used_pct - 91.0).abs() < 0.1);
        apply_claude_rate_info(&json!({"status":"rejected","rateLimitType":"seven_day_opus","resetsAt":now()+100}), &mut h);
        assert!(h.blocked());
    }
}
