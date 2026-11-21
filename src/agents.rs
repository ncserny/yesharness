//! Running one agent turn: spawn the CLI, stream its JSON events (for live status), and
//! return the final reply text plus the session id to resume next time.

use crate::env;
use crate::model::{Agent, AgentKind};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub struct Outcome {
    pub text: String,
    pub session: Option<String>,
    pub error: bool,
    /// Claude's latest `rate_limit_info`, if it sent one.
    pub rate: Option<Value>,
    /// What the agent actually did, from the engine's own events (not from what it says).
    pub receipt: Receipt,
}

/// The work a turn really did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Receipt {
    pub commands: usize,
    /// Files created or edited (names).
    pub edited: Vec<String>,
    /// Reads, searches, web lookups, other tools.
    pub lookups: usize,
    /// Tokens the engine reported for this turn (input incl. cached + output): what eats your quota.
    pub tokens: u64,
}

/// "140k tokens" / "2.9M tokens"
pub fn tokens_label(t: u64) -> String {
    if t >= 1_000_000 {
        format!("{:.1}M tokens", t as f64 / 1e6)
    } else {
        format!("{}k tokens", (t + 500) / 1000)
    }
}

impl Receipt {
    /// Real work: ran something or changed files. (Looking things up alone doesn't count.)
    pub fn did_work(&self) -> bool {
        self.commands > 0 || !self.edited.is_empty()
    }

    fn edit(&mut self, path: &str) {
        let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string());
        if !name.is_empty() && !self.edited.contains(&name) {
            self.edited.push(name);
        }
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match self.commands {
            0 => {}
            1 => parts.push("ran 1 command".to_string()),
            n => parts.push(format!("ran {n} commands")),
        }
        if !self.edited.is_empty() {
            let shown: Vec<&str> = self.edited.iter().take(3).map(String::as_str).collect();
            let more = if self.edited.len() > 3 { format!(" +{}", self.edited.len() - 3) } else { String::new() };
            parts.push(format!("edited {}{more}", shown.join(", ")));
        }
        match self.lookups {
            0 => {}
            1 => parts.push("1 lookup".to_string()),
            n => parts.push(format!("{n} lookups")),
        }
        if self.tokens > 0 {
            parts.push(tokens_label(self.tokens));
        }
        parts.join(" · ")
    }
}

/// Hooks the orchestrator gives a running agent.
pub struct Ctl<'a> {
    pub cancel: &'a AtomicBool,
    pub pid: &'a Mutex<Option<u32>>,
    pub status: &'a (dyn Fn(String) + Sync),
}

/// `model`: what to pass to the CLI (None = its own default). See probe::pick_model.
/// `images`: pictures attached to the message (Codex gets them as image inputs; everyone gets the paths in the prompt).
pub fn run(agent: &Agent, folder: &Path, prompt: &str, session: Option<&str>, model: Option<&str>, images: &[String], ctl: &Ctl) -> Outcome {
    let mut out = match agent.kind {
        AgentKind::Claude => run_claude(agent, folder, prompt, session, model, ctl),
        AgentKind::Codex => run_codex(agent, folder, prompt, session, model, false, images, ctl),
        AgentKind::Local => run_codex(agent, folder, prompt, session, model, true, &[], ctl),
        AgentKind::Custom => run_custom(agent, folder, prompt, ctl),
    };
    if out.error {
        out.text.push_str(&hint(agent, &out.text));
    }
    out
}

// ---------------------------------------------------------------- Claude Code

fn run_claude(agent: &Agent, folder: &Path, prompt: &str, session: Option<&str>, model: Option<&str>, ctl: &Ctl) -> Outcome {
    let mut cmd = env::command("claude");
    cmd.current_dir(folder)
        .arg("-p")
        .arg(prompt)
        .args(["--output-format", "stream-json", "--verbose"])
        .args(["--permission-mode", if agent.full_access { "bypassPermissions" } else { "acceptEdits" }]);
    if let Some(s) = session {
        cmd.args(["--resume", s]);
    }
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    if !agent.effort.is_empty() {
        cmd.args(["--effort", &agent.effort]);
    }
    if !agent.use_my_plugins {
        // Lean: no global MCP servers to start and describe on every turn.
        cmd.arg("--strict-mcp-config");
    }

    let mut p = ClaudeParser::default();
    let res = stream(cmd, None, ctl, |line| {
        if let Some(st) = p.feed(line) {
            (ctl.status)(st);
        }
    });
    let receipt = p.receipt.clone();
    let mut out = finish("claude", res, p.session, p.result.or(p.last_text), p.is_error, p.denied, ctl);
    out.rate = p.rate;
    out.receipt = receipt;
    out
}

#[derive(Default)]
pub struct ClaudeParser {
    pub session: Option<String>,
    pub last_text: Option<String>,
    pub result: Option<String>,
    pub is_error: bool,
    pub denied: usize,
    pub rate: Option<Value>,
    pub receipt: Receipt,
}

impl ClaudeParser {
    /// Returns a status line to show, if any.
    pub fn feed(&mut self, line: &str) -> Option<String> {
        let v: Value = serde_json::from_str(line).ok()?;
        if let Some(s) = v["session_id"].as_str() {
            self.session = Some(s.to_string());
        }
        match v["type"].as_str()? {
            "rate_limit_event" => {
                self.rate = Some(v["rate_limit_info"].clone());
                None
            }
            "assistant" => {
                let mut status = None;
                for c in v["message"]["content"].as_array()? {
                    match c["type"].as_str() {
                        Some("text") => {
                            let t = c["text"].as_str().unwrap_or_default().trim();
                            if !t.is_empty() {
                                self.last_text = Some(t.to_string());
                                status = Some("writing…".to_string());
                            }
                        }
                        Some("tool_use") => {
                            let name = c["name"].as_str().unwrap_or("tool");
                            match name {
                                "Bash" => self.receipt.commands += 1,
                                "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
                                    self.receipt.edit(c["input"]["file_path"].as_str().or(c["input"]["notebook_path"].as_str()).unwrap_or("file"))
                                }
                                _ => self.receipt.lookups += 1,
                            }
                            status = Some(describe_tool(name, &c["input"]));
                        }
                        Some("thinking") => status = Some("thinking…".into()),
                        _ => {}
                    }
                }
                status
            }
            "result" => {
                self.is_error = v["is_error"].as_bool().unwrap_or(false);
                if let Some(r) = v["result"].as_str() {
                    if !r.trim().is_empty() {
                        self.result = Some(r.trim().to_string());
                    }
                }
                self.denied = v["permission_denials"].as_array().map(|a| a.len()).unwrap_or(0);
                let u = &v["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                self.receipt.tokens = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens") + n("output_tokens");
                None
            }
            _ => None,
        }
    }
}

fn describe_tool(name: &str, input: &Value) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    let file = |k: &str| {
        let p = s(k);
        Path::new(&p).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or(p)
    };
    match name {
        "Bash" => format!("running `{}`", short(&s("command"), 70)),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => format!("editing {}", file("file_path")),
        "Read" => format!("reading {}", file("file_path")),
        "Grep" | "Glob" => format!("searching for {}", short(&s("pattern"), 40)),
        "WebSearch" | "WebFetch" => "searching the web".into(),
        "Task" | "Agent" => "running a sub-agent".into(),
        "TodoWrite" => "planning".into(),
        other => format!("using {other}"),
    }
}

// ---------------------------------------------------------------- Codex

/// `local`: run Codex against a model on this Mac (Ollama) instead of OpenAI's servers.
#[allow(clippy::too_many_arguments)]
fn run_codex(agent: &Agent, folder: &Path, prompt: &str, session: Option<&str>, model: Option<&str>, local: bool, images: &[String], ctl: &Ctl) -> Outcome {
    let mut cmd = env::command("codex");
    cmd.current_dir(folder).arg("exec");
    if session.is_some() {
        cmd.arg("resume");
    }
    cmd.args(["--json", "--skip-git-repo-check"]);
    if local {
        cmd.env("CODEX_HOME", crate::model::local_codex_home());
        cmd.args(["--oss", "--local-provider", "ollama"]);
    } else if !agent.use_my_plugins {
        // Lean: skip ~/.codex/config.toml (MCP servers, plugins). Login still works. ~30% faster per turn.
        cmd.arg("--ignore-user-config");
    }
    if !agent.effort.is_empty() {
        cmd.args(["-c", &format!("model_reasoning_effort=\"{}\"", agent.effort)]);
    }
    for img in images.iter().filter(|p| Path::new(p).is_file()) {
        cmd.args(["-i", img]);
    }
    if agent.full_access {
        cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    } else {
        cmd.args(["-c", "sandbox_mode=\"workspace-write\""]);
    }
    if let Some(m) = model {
        cmd.args(["-m", m]);
    }
    if let Some(s) = session {
        cmd.arg(s);
    }
    cmd.arg(prompt);

    let mut p = CodexParser::default();
    let res = stream(cmd, None, ctl, |line| {
        if let Some(st) = p.feed(line) {
            (ctl.status)(st);
        }
    });
    let is_error = p.error.is_some() && p.last_text.is_none();
    let text = if is_error { p.error.clone() } else { p.last_text.clone() };
    let mut out = finish(if local { "codex (local)" } else { "codex" }, res, p.thread, text, is_error, 0, ctl);
    out.receipt = p.receipt;
    out
}

#[derive(Default)]
pub struct CodexParser {
    pub thread: Option<String>,
    pub last_text: Option<String>,
    pub error: Option<String>,
    pub receipt: Receipt,
}

impl CodexParser {
    pub fn feed(&mut self, line: &str) -> Option<String> {
        let v: Value = serde_json::from_str(line).ok()?;
        match v["type"].as_str()? {
            "thread.started" => {
                self.thread = v["thread_id"].as_str().map(String::from);
                Some("starting…".into())
            }
            "item.started" | "item.updated" | "item.completed" => {
                let item = &v["item"];
                let done = v["type"] == "item.completed";
                if done {
                    match item["type"].as_str().unwrap_or("") {
                        "command_execution" => self.receipt.commands += 1,
                        "file_change" => {
                            for ch in item["changes"].as_array().into_iter().flatten() {
                                self.receipt.edit(ch["path"].as_str().unwrap_or("file"));
                            }
                        }
                        "mcp_tool_call" | "web_search" => self.receipt.lookups += 1,
                        _ => {}
                    }
                }
                match item["type"].as_str()? {
                    "agent_message" if done => {
                        let t = item["text"].as_str().unwrap_or_default().trim().to_string();
                        if !t.is_empty() {
                            self.last_text = Some(t);
                        }
                        Some("writing…".into())
                    }
                    "command_execution" => Some(format!("running `{}`", short(item["command"].as_str().unwrap_or("command"), 70))),
                    "reasoning" => Some("thinking…".into()),
                    "file_change" => Some("editing files".into()),
                    "mcp_tool_call" => Some(format!("using {}", item["tool"].as_str().unwrap_or("a tool"))),
                    "web_search" => Some("searching the web".into()),
                    "todo_list" => Some("planning".into()),
                    _ => None,
                }
            }
            "turn.completed" => {
                let u = &v["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                // input_tokens already includes the cached part
                self.receipt.tokens += n("input_tokens") + n("output_tokens") + n("reasoning_output_tokens");
                None
            }
            "turn.failed" => {
                self.error = Some(clean_error(v["error"]["message"].as_str().unwrap_or("Codex failed")));
                None
            }
            "error" => {
                self.error = Some(clean_error(v["message"].as_str().unwrap_or("Codex error")));
                None
            }
            _ => None,
        }
    }
}

/// Codex wraps API errors as a JSON string inside the message; dig out the human part.
fn clean_error(msg: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(msg) {
        for ptr in ["/error/message", "/message", "/detail"] {
            if let Some(s) = v.pointer(ptr).and_then(|x| x.as_str()) {
                return s.to_string();
            }
        }
    }
    msg.to_string()
}

// ---------------------------------------------------------------- Custom command

fn run_custom(agent: &Agent, folder: &Path, prompt: &str, ctl: &Ctl) -> Outcome {
    if agent.command.trim().is_empty() {
        return Outcome { text: "This agent has no command set. Open Agents settings and add one.".into(), session: None, error: true, rate: None, receipt: Receipt::default() };
    }
    let mut cmd = if cfg!(windows) {
        let mut c = env::command("cmd");
        c.args(["/C", &agent.command]);
        c
    } else {
        let mut c = env::command("sh");
        c.args(["-c", &agent.command]);
        c
    };
    cmd.current_dir(folder);
    (ctl.status)("running…".into());
    let mut out = String::new();
    let res = stream(cmd, Some(prompt.to_string()), ctl, |line| {
        out.push_str(line);
        out.push('\n');
        (ctl.status)("writing…".into());
    });
    let text = out.trim().to_string();
    let ok = matches!(&res, Ok((true, _)));
    finish(&agent.handle, res, None, (!text.is_empty()).then_some(text), !ok, 0, ctl)
}

// ---------------------------------------------------------------- process plumbing

/// Spawns `cmd`, feeds stdin, calls `on_line` for each stdout line. Returns (success, stderr).
fn stream(mut cmd: Command, stdin: Option<String>, ctl: &Ctl, mut on_line: impl FnMut(&str)) -> Result<(bool, String), String> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("Couldn't find `{}` on your PATH.", cmd.get_program().to_string_lossy())
        } else {
            format!("Couldn't start `{}`: {e}", cmd.get_program().to_string_lossy())
        }
    })?;
    *ctl.pid.lock().unwrap() = Some(child.id());
    let mut watchdog = env::guard(child.id());

    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = si.write_all(data.as_bytes());
        });
    }
    let mut stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_end(&mut buf);
        }
        let s = String::from_utf8_lossy(&buf).into_owned();
        // Keep the tail; that's where the useful error usually is.
        let n = s.chars().count();
        if n > 4000 { s.chars().skip(n - 4000).collect() } else { s }
    });

    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if ctl.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end_matches(['\n', '\r']);
                    if !line.is_empty() {
                        on_line(line);
                    }
                }
            }
        }
    }
    if ctl.cancel.load(Ordering::Relaxed) {
        let _ = child.kill();
    }
    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    *ctl.pid.lock().unwrap() = None;
    if let Some(w) = watchdog.as_mut() {
        let _ = w.kill();
        let _ = w.wait();
    }
    let stderr = err_reader.join().unwrap_or_default();
    Ok((ok, stderr))
}

fn finish(
    name: &str,
    res: Result<(bool, String), String>,
    session: Option<String>,
    text: Option<String>,
    is_error: bool,
    denied: usize,
    ctl: &Ctl,
) -> Outcome {
    if ctl.cancel.load(Ordering::Relaxed) {
        return Outcome { text: "(stopped)".into(), session, error: true, rate: None, receipt: Receipt::default() };
    }
    match res {
        Err(e) => Outcome { text: e, session: None, error: true, rate: None, receipt: Receipt::default() },
        Ok((ok, stderr)) => {
            let mut text = text.unwrap_or_default();
            let mut error = is_error;
            if text.is_empty() {
                error = true;
                let tail = stderr
                    .lines()
                    .filter(|l| !l.contains("rmcp::") && !l.trim().is_empty())
                    .collect::<Vec<_>>();
                let tail = tail[tail.len().saturating_sub(12)..].join("\n");
                text = if tail.is_empty() {
                    format!("`{name}` finished without a reply{}.", if ok { "" } else { " (it exited with an error)" })
                } else {
                    format!("`{name}` failed:\n```\n{tail}\n```")
                };
            }
            if denied > 0 && !error {
                text.push_str(&format!(
                    "\n\n_({denied} action(s) were blocked by permissions. Turn on \"Full access\" for this agent in Agents settings if you want it to run commands freely.)_"
                ));
            }
            Outcome { text, session, error, rate: None, receipt: Receipt::default() }
        }
    }
}

/// Friendly next step for common setup problems.
fn hint(agent: &Agent, text: &str) -> String {
    let t = text.to_lowercase();
    let h = match agent.kind {
        AgentKind::Claude if t.contains("authenticate") || t.contains("login") || t.contains("oauth") || t.contains("api key") => {
            "Claude Code isn't logged in. Open Terminal, run `claude`, then type `/login`."
        }
        AgentKind::Codex if t.contains("model") && (t.contains("not supported") || t.contains("not found") || t.contains("does not exist")) => {
            let models = codex_models();
            let list = if models.is_empty() { String::new() } else { format!(" Models Codex knows: {}.", models.join(", ")) };
            return format!(
                "\n\n**Fix:** Codex is set to a model your account can't use. Set a model for this agent in Agents settings, or fix `model` in ~/.codex/config.toml.{list}"
            );
        }
        AgentKind::Codex if t.contains("401") || t.contains("unauthorized") || t.contains("login") => {
            "Codex isn't logged in. Open Terminal and run `codex login`."
        }
        AgentKind::Local if t.contains("connection refused") || t.contains("connect") || t.contains("ollama") => {
            "The local model needs Ollama running. Open the Ollama app (or run `ollama serve`)."
        }
        AgentKind::Local if t.contains("context size") || t.contains("context length") => {
            "The conversation is too long for the local model. Use ••• → Reset Agent Memory to start fresh."
        }
        _ if t.contains("couldn't find") => match agent.kind {
            AgentKind::Claude => "Install Claude Code: `curl -fsSL https://claude.ai/install.sh | bash`",
            AgentKind::Codex => "Install Codex: `npm i -g @openai/codex` (or `brew install codex`)",
            AgentKind::Local => "Install Ollama from ollama.com and Codex (`npm i -g @openai/codex`).",
            AgentKind::Custom => "Check the command in Agents settings.",
        },
        _ => return String::new(),
    };
    format!("\n\n**Fix:** {h}")
}

/// Model slugs from `codex debug models`, best effort.
fn codex_models() -> Vec<String> {
    let Ok(out) = env::command("codex").args(["debug", "models"]).stdin(Stdio::null()).stderr(Stdio::null()).output() else {
        return vec![];
    };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else { return vec![] };
    let list = v.get("models").unwrap_or(&v);
    list.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["slug"].as_str().or(m["id"].as_str()))
                .filter(|s| s.starts_with("gpt"))
                .map(|s| format!("`{s}`"))
                .take(8)
                .collect()
        })
        .unwrap_or_default()
}

pub fn short(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > max {
        format!("{}…", one_line.chars().take(max).collect::<String>())
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_stream() {
        let mut p = ClaudeParser::default();
        p.feed(r#"{"type":"system","subtype":"init","session_id":"abc"}"#);
        let st = p.feed(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]},"session_id":"abc"}"#);
        assert_eq!(st.as_deref(), Some("running `cargo test`"));
        p.feed(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"All good"}]},"session_id":"abc"}"#);
        p.feed(r#"{"type":"result","is_error":false,"result":"Done: all tests pass","session_id":"abc","permission_denials":[]}"#);
        assert_eq!(p.session.as_deref(), Some("abc"));
        assert_eq!(p.result.as_deref(), Some("Done: all tests pass"));
        assert!(!p.is_error);
    }

    #[test]
    fn receipts_count_real_work() {
        let mut c = CodexParser::default();
        c.feed(r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo test"}}"#);
        c.feed(r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"/p/src/ui.rs","kind":"update"},{"path":"/p/README.md","kind":"add"}]}}"#);
        assert_eq!(c.receipt.summary(), "ran 1 command · edited ui.rs, README.md");
        c.feed(r#"{"type":"turn.completed","usage":{"input_tokens":139500,"cached_input_tokens":120000,"output_tokens":800}}"#);
        assert_eq!(c.receipt.summary(), "ran 1 command · edited ui.rs, README.md · 140k tokens");
        assert!(c.receipt.did_work());

        let mut cl = ClaudeParser::default();
        cl.feed(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/a/b.rs"}}]}}"#);
        assert!(!cl.receipt.did_work(), "reading isn't doing");
        cl.feed(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/a/b.rs"}}]}}"#);
        assert_eq!(cl.receipt.summary(), "edited b.rs · 1 lookup");
    }

    #[test]
    fn parses_codex_stream() {
        let mut p = CodexParser::default();
        p.feed(r#"{"type":"thread.started","thread_id":"t1"}"#);
        let st = p.feed(r#"{"type":"item.started","item":{"id":"i","type":"command_execution","command":"ls -la"}}"#);
        assert_eq!(st.as_deref(), Some("running `ls -la`"));
        p.feed(r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"Here you go"}}"#);
        assert_eq!(p.thread.as_deref(), Some("t1"));
        assert_eq!(p.last_text.as_deref(), Some("Here you go"));

        let mut p = CodexParser::default();
        p.feed(r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'x' model is not supported\"}}"}}"#);
        assert_eq!(p.error.as_deref(), Some("The 'x' model is not supported"));
    }
}
