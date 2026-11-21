//! Long-term memory per channel, in the style of Letta (MemGPT), without any extra service:
//!
//! - **Memory blocks**: a short Markdown note per channel ("About you", "Project", "Decisions") that every
//!   agent gets at the start of a conversation. When a conversation pauses, the local Ollama model rewrites
//!   the note from what was said, so it costs nothing against your Claude/Codex limits. You can edit it.
//! - **Recall**: every message is embedded with bge-m3 (Ollama, downloaded automatically if missing) and
//!   stored next to the note. Agents get the few older messages most relevant to what you just asked,
//!   instead of the whole history.
//!
//! Everything lives in `<app data>/YesHarness/memory/<channel id>/`.

use crate::model::{self, Author, Message};
use crate::orchestrator::Shared;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The embedding model (multilingual, ~1.2 GB). Pulled through Ollama when it isn't installed.
pub const EMBED_MODEL: &str = "bge-m3";
/// How similar an older message must be to count as relevant (cosine, bge-m3).
const MIN_SCORE: f32 = 0.5;
/// Older messages recalled per turn.
const RECALL: usize = 4;
const SNIPPET_CHARS: usize = 400;
/// Longest piece of a message embedded as one entry.
const CHUNK_CHARS: usize = 1200;
/// The note stays small: it goes into prompts.
const MAX_BLOCKS_CHARS: usize = 3000;
/// Rewrite the note once a conversation has been quiet this long…
const QUIET_SECS: u64 = 90;
/// …and at least this many messages are new.
const MIN_NEW: usize = 2;

const SECTIONS: [&str; 3] = ["## About the user", "## Project", "## Decisions"];

// ------------------------------------------------------------------ status (shown in the header)

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Starting,
    /// Ollama isn't installed or won't start: the note still works, recall doesn't.
    NoOllama,
    /// Downloading bge-m3: percent.
    Downloading(u8),
    Ready,
    Failed(String),
}

fn status_cell() -> &'static Mutex<Status> {
    static S: OnceLock<Mutex<Status>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Status::Starting))
}

pub fn status() -> Status {
    status_cell().lock().unwrap().clone()
}

fn set_status(s: Status, shared: Option<&Shared>) {
    let mut cur = status_cell().lock().unwrap();
    if *cur != s {
        *cur = s;
        drop(cur);
        if let Some(sh) = shared {
            (sh.repaint)();
        }
    }
}

// ------------------------------------------------------------------ files

pub fn dir(cid: &str) -> PathBuf {
    let base = model::data_file().parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    base.join("memory").join(cid)
}

pub fn blocks_path(cid: &str) -> PathBuf {
    dir(cid).join("memory.md")
}

fn template(channel: &str) -> String {
    format!(
        "# Memory for #{channel}\n\n{}\n\n{}\n\n{}\n",
        SECTIONS[0], SECTIONS[1], SECTIONS[2]
    )
}

/// The note, if it says anything yet.
pub fn read_blocks(cid: &str) -> Option<String> {
    let text = std::fs::read_to_string(blocks_path(cid)).ok()?;
    has_content(&text).then(|| text.trim().to_string())
}

/// Bullet points (or any line that isn't a heading).
fn has_content(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim();
        !l.is_empty() && !l.starts_with('#')
    })
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct State {
    /// Messages up to this time are in the recall index.
    #[serde(default)]
    indexed_ts: u64,
    /// Messages up to this time are reflected in the note.
    #[serde(default)]
    blocks_ts: u64,
}

fn load_state(cid: &str) -> State {
    std::fs::read_to_string(dir(cid).join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_state(cid: &str, s: &State) {
    let _ = std::fs::create_dir_all(dir(cid));
    let _ = std::fs::write(dir(cid).join("state.json"), serde_json::to_string_pretty(s).unwrap_or_default());
}

// ------------------------------------------------------------------ Ollama (plain HTTP, no extra crates)

const OLLAMA: &str = "127.0.0.1:11434";

fn connect(read_timeout: Duration) -> Result<TcpStream, String> {
    let addr = OLLAMA.parse().map_err(|_| "bad address".to_string())?;
    let s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).map_err(|_| "Ollama isn't running".to_string())?;
    s.set_read_timeout(Some(read_timeout)).ok();
    Ok(s)
}

fn send_request(s: &mut TcpStream, path: &str, body: &Value) -> Result<(), String> {
    let body = body.to_string();
    // HTTP/1.0: no chunked encoding; the server closes the connection when it's done.
    let req = format!(
        "POST {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())
}

fn post(path: &str, body: &Value, timeout: Duration) -> Result<Value, String> {
    let mut s = connect(timeout)?;
    send_request(&mut s, path, body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<Value, String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("empty response")?;
    let v: Value = serde_json::from_str(body.trim()).map_err(|_| format!("unexpected reply: {}", body.chars().take(200).collect::<String>()))?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return Err(v["error"].as_str().unwrap_or(head.lines().next().unwrap_or("error")).to_string());
    }
    Ok(v)
}

fn installed_models() -> Option<Vec<String>> {
    let tags = crate::probe::ollama_tags()?;
    Some(tags["models"].as_array()?.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect())
}

fn has_model(names: &[String], model: &str) -> bool {
    names.iter().any(|n| n == model || n.strip_suffix(":latest") == Some(model) || n.split(':').next() == Some(model) && !model.contains(':'))
}

/// Downloads a model through Ollama, reporting progress (0–100).
pub fn pull(model: &str, mut progress: impl FnMut(u8)) -> Result<(), String> {
    let mut s = connect(Duration::from_secs(120))?;
    send_request(&mut s, "/api/pull", &json!({ "model": model, "stream": true }))?;
    let mut r = BufReader::new(s);
    // Skip the HTTP headers.
    let mut line = String::new();
    let mut status_line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("Ollama closed the connection".into());
        }
        if status_line.is_empty() {
            status_line = line.trim().to_string();
        }
        if line.trim().is_empty() {
            break;
        }
    }
    let mut last_err = None;
    let mut ok = false;
    for l in r.lines() {
        let l = l.map_err(|e| e.to_string())?;
        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
        if let Some(e) = v["error"].as_str() {
            last_err = Some(e.to_string());
        }
        if let (Some(done), Some(total)) = (v["completed"].as_u64(), v["total"].as_u64()) {
            // Only the big layer matters; small ones flash 0→100 and would make the number jump.
            if total > 20_000_000 {
                progress((done * 100 / total.max(1)).min(99) as u8);
            }
        }
        if v["status"].as_str() == Some("success") {
            ok = true;
        }
    }
    match (ok, last_err) {
        (true, _) => {
            progress(100);
            Ok(())
        }
        (false, Some(e)) => Err(e),
        (false, None) => Err(format!("download didn't finish ({status_line})")),
    }
}

/// Starts Ollama if needed and makes sure bge-m3 is there, downloading it the first time.
pub fn ensure_embedder(shared: Option<&Shared>) -> bool {
    let mut names = installed_models();
    if names.is_none() {
        crate::probe::start_ollama();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            names = installed_models();
            if names.is_some() {
                break;
            }
        }
    }
    let Some(names) = names else {
        set_status(Status::NoOllama, shared);
        return false;
    };
    if has_model(&names, EMBED_MODEL) {
        set_status(Status::Ready, shared);
        return true;
    }
    set_status(Status::Downloading(0), shared);
    match pull(EMBED_MODEL, |p| set_status(Status::Downloading(p), shared)) {
        Ok(()) => {
            set_status(Status::Ready, shared);
            true
        }
        Err(e) => {
            set_status(Status::Failed(e), shared);
            false
        }
    }
}

/// Unit-length embeddings (so similarity is a dot product).
pub fn embed(texts: &[String]) -> Option<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Some(Vec::new());
    }
    let v = post("/api/embed", &json!({ "model": EMBED_MODEL, "input": texts, "truncate": true }), Duration::from_secs(120)).ok()?;
    let out: Vec<Vec<f32>> = v["embeddings"]
        .as_array()?
        .iter()
        .map(|e| normalize(e.as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect()).unwrap_or_default()))
        .collect();
    (out.len() == texts.len() && out.iter().all(|e| !e.is_empty())).then_some(out)
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The local chat model that keeps the notes: a fast one first.
fn note_model(names: &[String]) -> Option<String> {
    const PREFER: [&str; 6] = ["gemma4", "gpt-oss:20b", "qwen3", "gemma", "gpt-oss", "llama"];
    let usable: Vec<&String> = names.iter().filter(|n| !n.contains("embed") && !n.starts_with("bge") && !n.starts_with("nomic") && !n.contains("minilm")).collect();
    PREFER.iter().find_map(|p| usable.iter().find(|n| n.contains(p))).or(usable.first()).map(|n| n.to_string())
}

fn chat(model: &str, system: &str, user: &str) -> Result<String, String> {
    let mut body = json!({
        "model": model,
        "stream": false,
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
        "options": {"temperature": 0.2, "num_ctx": 16384},
    });
    if model.contains("gpt-oss") {
        body["think"] = json!("low");
    }
    let v = post("/api/chat", &body, Duration::from_secs(300))?;
    Ok(v["message"]["content"].as_str().unwrap_or_default().to_string())
}

// ------------------------------------------------------------------ recall index

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub ts: u64,
    pub who: String,
    pub text: String,
}

#[derive(Default)]
struct Index {
    entries: Vec<Entry>,
    vecs: Vec<Vec<f32>>,
}

fn indexes() -> &'static Mutex<HashMap<String, Index>> {
    static I: OnceLock<Mutex<HashMap<String, Index>>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(HashMap::new()))
}

/// entries.jsonl + vectors.bin (f32 little endian, one row per entry).
fn load_index(cid: &str) -> Index {
    let d = dir(cid);
    let entries: Vec<Entry> = std::fs::read_to_string(d.join("entries.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let bytes = std::fs::read(d.join("vectors.bin")).unwrap_or_default();
    let dims = std::fs::read_to_string(d.join("dims")).ok().and_then(|t| t.trim().parse::<usize>().ok()).unwrap_or(0);
    if dims == 0 || entries.is_empty() {
        return Index::default();
    }
    let floats: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let vecs: Vec<Vec<f32>> = floats.chunks_exact(dims).map(|c| c.to_vec()).collect();
    let n = entries.len().min(vecs.len());
    Index { entries: entries[..n].to_vec(), vecs: vecs[..n].to_vec() }
}

fn append_index(cid: &str, entries: &[Entry], vecs: &[Vec<f32>]) -> std::io::Result<()> {
    let d = dir(cid);
    std::fs::create_dir_all(&d)?;
    let dims = vecs.first().map(|v| v.len()).unwrap_or(0);
    let mut idx = indexes().lock().unwrap();
    let index = idx.entry(cid.to_string()).or_insert_with(|| load_index(cid));
    // Keep the two files the same length, even after a crash halfway through a write.
    if index.entries.is_empty() {
        let _ = std::fs::remove_file(d.join("entries.jsonl"));
        let _ = std::fs::remove_file(d.join("vectors.bin"));
        std::fs::write(d.join("dims"), dims.to_string())?;
    } else if index.vecs.first().map(|v| v.len()) != Some(dims) {
        return Err(std::io::Error::other("embedding size changed"));
    }
    let mut ef = std::fs::OpenOptions::new().create(true).append(true).open(d.join("entries.jsonl"))?;
    let mut vf = std::fs::OpenOptions::new().create(true).append(true).open(d.join("vectors.bin"))?;
    for (e, v) in entries.iter().zip(vecs) {
        let mut buf = Vec::with_capacity(v.len() * 4);
        v.iter().for_each(|x| buf.extend_from_slice(&x.to_le_bytes()));
        vf.write_all(&buf)?;
        writeln!(ef, "{}", serde_json::to_string(e).unwrap_or_default())?;
        index.entries.push(e.clone());
        index.vecs.push(v.clone());
    }
    Ok(())
}

/// Splits a long message into pieces of at most CHUNK_CHARS, at paragraph or sentence ends where possible.
pub fn chunks(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.chars().count() <= CHUNK_CHARS {
        return if text.is_empty() { Vec::new() } else { vec![text.to_string()] };
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for part in text.split_inclusive(['\n', '.', '!', '?']) {
        if cur.chars().count() + part.chars().count() > CHUNK_CHARS && !cur.trim().is_empty() {
            out.push(cur.trim().to_string());
            cur.clear();
        }
        cur.push_str(part);
        while cur.chars().count() > CHUNK_CHARS {
            let head: String = cur.chars().take(CHUNK_CHARS).collect();
            out.push(head.trim().to_string());
            cur = cur.chars().skip(CHUNK_CHARS).collect();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Who wrote a message, for memory ("User", "Alex (@main)"). None = not worth remembering.
fn speaker(m: &Message, data: &model::Data) -> Option<String> {
    if m.error {
        return None;
    }
    match &m.author {
        Author::User => Some("User".into()),
        Author::Agent(id) => Some(data.agent(id).map(|a| a.label()).unwrap_or_else(|| "Agent".into())),
        Author::System | Author::Heartbeat => None,
    }
}

/// Adds messages newer than the last run to the recall index.
fn index_new(shared: &Shared, cid: &str, state: &mut State) -> bool {
    let new: Vec<Entry> = {
        let data = shared.data.lock().unwrap();
        let Some(ch) = data.channel(cid) else { return false };
        ch.messages
            .iter()
            .filter(|m| m.ts > state.indexed_ts)
            .filter_map(|m| speaker(m, &data).map(|who| (m, who)))
            .flat_map(|(m, who)| chunks(&m.text).into_iter().map(move |text| Entry { ts: m.ts, who: who.clone(), text }))
            .collect()
    };
    if new.is_empty() {
        return false;
    }
    let last = new.iter().map(|e| e.ts).max().unwrap_or(state.indexed_ts);
    for batch in new.chunks(16) {
        let texts: Vec<String> = batch.iter().map(|e| format!("{}: {}", e.who, e.text)).collect();
        let Some(vecs) = embed(&texts) else { return false };
        if append_index(cid, batch, &vecs).is_err() {
            return false;
        }
    }
    state.indexed_ts = last;
    true
}

fn top(index: &Index, q: &[f32], before_ts: u64, k: usize) -> Vec<(f32, Entry)> {
    let mut scored: Vec<(f32, &Entry)> = index
        .entries
        .iter()
        .zip(&index.vecs)
        .filter(|(e, _)| e.ts < before_ts)
        .map(|(e, v)| (dot(q, v), e))
        .filter(|(s, _)| *s >= MIN_SCORE)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out: Vec<(f32, Entry)> = Vec::new();
    for (s, e) in scored {
        if out.iter().any(|(_, o)| o.text == e.text) {
            continue;
        }
        out.push((s, e.clone()));
        if out.len() == k {
            break;
        }
    }
    out
}

// ------------------------------------------------------------------ what agents see

/// The channel's note, for the start of a conversation ("" if it says nothing yet).
pub fn note_section(cid: &str) -> String {
    let Some(note) = read_blocks(cid) else { return String::new() };
    let body: String = note.lines().filter(|l| !l.starts_with("# ")).collect::<Vec<_>>().join("\n");
    format!("\n## Long-term memory (notes the app keeps about this user and project; may lag behind)\n{}\n", body.trim())
}

/// Older messages related to `query`, once per prompt window: `windows[i]` is the time of the oldest message
/// that prompt already shows. One embedding call for all of them.
pub fn recall_sections(cid: &str, query: &str, windows: &[u64]) -> Vec<String> {
    let empty = vec![String::new(); windows.len()];
    if query.trim().is_empty() || status() != Status::Ready {
        return empty;
    }
    let Some(q) = embed(&[query.to_string()]).and_then(|mut v| v.pop()) else { return empty };
    let mut idx = indexes().lock().unwrap();
    let index = idx.entry(cid.to_string()).or_insert_with(|| load_index(cid));
    windows.iter().map(|w| recall_text(&top(index, &q, *w, RECALL))).collect()
}

fn recall_text(hits: &[(f32, Entry)]) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let mut p = String::from("\n## Earlier in this channel (older messages related to the latest request)\n");
    for (_, e) in hits {
        let text: String = e.text.chars().take(SNIPPET_CHARS).collect();
        let more = if e.text.chars().count() > SNIPPET_CHARS { "…" } else { "" };
        p.push_str(&format!("- [{}] {}: {}{more}\n", day(e.ts), e.who, text.replace('\n', " ")));
    }
    p
}

fn day(ts: u64) -> String {
    let ago = model::now().saturating_sub(ts);
    match ago / 86_400 {
        0 => "today".into(),
        1 => "yesterday".into(),
        d => format!("{d} days ago"),
    }
}

// ------------------------------------------------------------------ keeping the note up to date

const NOTE_INSTRUCTIONS: &str = "You maintain the long-term memory of a small team of AI agents that work with one user \
in a chat channel. The memory is a short Markdown note with exactly three sections of bullet points:\n\
- About the user: their preferences, how they like to work and be answered, who they are.\n\
- Project: durable facts about the project (what it is, stack, important files, URLs, accounts, conventions).\n\
- Decisions: what was decided and, briefly, why.\n\n\
Update the note with what's worth remembering from the new conversation. Keep only facts that will still matter \
in future conversations; skip chit-chat, progress updates and anything only true for a moment. Merge duplicates, \
update facts that changed and drop ones that are no longer true. The user may have edited the note by hand: keep \
their edits. If the user explicitly asked to remember something, keep it. Never write down passwords, API keys, \
tokens or other secrets. At most 8 bullets per section, each under 25 words. If nothing new is worth \
remembering, return the note unchanged.\n\n\
Reply with ONLY the complete note, in exactly this format (no code fences, no other text):\n\
# Memory for #<channel>\n\n## About the user\n- …\n\n## Project\n- …\n\n## Decisions\n- …";

/// Checks and tidies the model's note; None if it isn't usable.
pub fn clean_note(reply: &str, channel: &str) -> Option<String> {
    let mut t = reply.trim();
    // Some models wrap it in a code fence anyway.
    if let Some(rest) = t.strip_prefix("```") {
        t = rest.split_once('\n').map(|x| x.1).unwrap_or("");
        t = t.trim_end().strip_suffix("```").unwrap_or(t).trim();
    }
    let start = t.find("## About the user")?;
    let body = t[start..].trim();
    if !SECTIONS.iter().all(|s| body.contains(s)) || body.chars().count() > MAX_BLOCKS_CHARS {
        return None;
    }
    Some(format!("# Memory for #{channel}\n\n{body}\n"))
}

fn transcript(msgs: &[(String, String)]) -> String {
    let mut out = String::new();
    // The most recent part of a long conversation is what matters; keep the prompt small for the local model.
    let mut budget = 24_000usize;
    let mut picked = Vec::new();
    for (who, text) in msgs.iter().rev() {
        let t: String = text.chars().take(1500).collect();
        if t.len() + who.len() > budget {
            break;
        }
        budget -= t.len() + who.len();
        picked.push(format!("**{who}:** {t}\n\n"));
    }
    picked.reverse();
    picked.iter().for_each(|p| out.push_str(p));
    out
}

/// Rewrites the note when the conversation has paused and there's something new.
fn maybe_update_note(shared: &Shared, cid: &str, state: &mut State, model_name: &str) -> bool {
    let (channel, msgs, last_ts, busy) = {
        let data = shared.data.lock().unwrap();
        let Some(ch) = data.channel(cid) else { return false };
        let msgs: Vec<(String, String)> = ch
            .messages
            .iter()
            .filter(|m| m.ts > state.blocks_ts)
            .filter_map(|m| speaker(m, &data).map(|w| (w, m.text.clone())))
            .collect();
        let busy = shared.runs.lock().unwrap().get(cid).is_some_and(|r| r.active > 0 || !r.working.is_empty());
        (ch.name.clone(), msgs, ch.messages.last().map(|m| m.ts).unwrap_or(0), busy)
    };
    if busy || msgs.len() < MIN_NEW || model::now().saturating_sub(last_ts) < QUIET_SECS {
        return false;
    }
    let path = blocks_path(cid);
    let before = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let current = std::fs::read_to_string(&path).unwrap_or_else(|_| template(&channel));
    let user = format!(
        "Channel: #{channel}\n\nThe note now:\n\n{current}\n\nNew conversation since the note was last updated:\n\n{}",
        transcript(&msgs)
    );
    let Ok(reply) = chat(model_name, NOTE_INSTRUCTIONS, &user) else { return false };
    let Some(note) = clean_note(&reply, &channel) else {
        // Unusable reply: try again after the next messages rather than looping on the same ones.
        state.blocks_ts = last_ts;
        return true;
    };
    // You edited it while the model was thinking: yours wins; try again next time.
    if std::fs::metadata(&path).and_then(|m| m.modified()).ok() != before {
        return false;
    }
    let _ = std::fs::create_dir_all(dir(cid));
    if std::fs::write(&path, note).is_ok() {
        state.blocks_ts = last_ts;
        shared.dirty.store(true, Ordering::Relaxed);
        (shared.repaint)();
    }
    true
}

/// One pass over all channels: index new messages, refresh notes.
pub fn tick(shared: &Shared) {
    if !shared.data.lock().unwrap().settings.memory {
        return;
    }
    let ids: Vec<String> = shared.data.lock().unwrap().channels.iter().map(|c| c.id.clone()).collect();
    let names = installed_models().unwrap_or_default();
    let model_name = note_model(&names);
    for cid in ids {
        let mut state = load_state(&cid);
        let mut changed = false;
        if status() == Status::Ready {
            changed |= index_new(shared, &cid, &mut state);
        }
        if let Some(m) = &model_name {
            changed |= maybe_update_note(shared, &cid, &mut state, m);
        }
        if changed {
            save_state(&cid, &state);
        }
    }
}

/// Background thread: sets up bge-m3 once (retrying if that fails), then keeps memory up to date every minute.
pub fn start(shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut tries = 0u32;
        loop {
            let ready = status() == Status::Ready;
            if !ready && shared.data.lock().unwrap().settings.memory && tries % 10 == 0 {
                ensure_embedder(Some(&shared));
            }
            tries += 1;
            tick(&shared);
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_messages() {
        assert_eq!(chunks("  hi  "), vec!["hi"]);
        assert!(chunks("").is_empty());
        let long = "This is a sentence. ".repeat(200);
        let c = chunks(&long);
        assert!(c.len() >= 3);
        assert!(c.iter().all(|x| x.chars().count() <= CHUNK_CHARS));
        assert_eq!(c.join(" ").split_whitespace().count(), long.split_whitespace().count());
        // No sentence breaks at all.
        let blob = "x".repeat(3000);
        assert!(chunks(&blob).iter().all(|x| x.chars().count() <= CHUNK_CHARS));
    }

    #[test]
    fn ranks_by_similarity_and_skips_recent() {
        let e = |ts, t: &str| Entry { ts, who: "User".into(), text: t.into() };
        let index = Index {
            entries: vec![e(1, "deploy"), e(2, "cats"), e(3, "deploy again"), e(50, "recent deploy")],
            vecs: vec![normalize(vec![1.0, 0.1]), normalize(vec![0.0, 1.0]), normalize(vec![0.9, 0.2]), normalize(vec![1.0, 0.0])],
        };
        let hits = top(&index, &normalize(vec![1.0, 0.0]), 10, 4);
        let texts: Vec<&str> = hits.iter().map(|(_, e)| e.text.as_str()).collect();
        assert_eq!(texts, vec!["deploy", "deploy again"], "cats is unrelated, the recent one is already in the prompt");
    }

    #[test]
    fn cleans_the_models_note() {
        let reply = "```markdown\n# Memory for #x\n\n## About the user\n- Likes short answers\n\n## Project\n- Rust app\n\n## Decisions\n- Use Parakeet\n```";
        let note = clean_note(reply, "site").unwrap();
        assert!(note.starts_with("# Memory for #site\n\n## About the user\n- Likes short answers"));
        assert!(note.contains("## Decisions\n- Use Parakeet"));
        assert!(clean_note("Sure! Here you go.", "site").is_none());
        assert!(clean_note("## About the user\n- a\n## Project\n", "site").is_none(), "missing a section");
        assert!(!has_content(&template("site")));
        assert!(has_content(&note));
    }

    #[test]
    fn finds_installed_models() {
        let names = vec!["bge-m3:latest".to_string(), "gpt-oss:20b".into(), "gemma4:latest".into()];
        assert!(has_model(&names, "bge-m3"));
        assert!(!has_model(&names, "all-minilm"));
        assert_eq!(note_model(&names).as_deref(), Some("gemma4:latest"));
        assert_eq!(note_model(&["bge-m3:latest".to_string()]), None);
    }

    #[test]
    fn parses_ollama_replies() {
        let ok = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"embeddings\":[[1,2]]}";
        assert_eq!(parse_response(ok).unwrap()["embeddings"][0][1], 2);
        let err = b"HTTP/1.0 404 Not Found\r\n\r\n{\"error\":\"model \\\"bge-m3\\\" not found\"}";
        assert!(parse_response(err).unwrap_err().contains("not found"));
    }

    /// Needs Ollama with bge-m3: cargo test -- --ignored live_recall
    #[test]
    #[ignore]
    fn live_recall() {
        assert!(ensure_embedder(None));
        let texts = vec![
            "User: Our website is deployed on Netlify from the main branch.".to_string(),
            "User: I love Italian food, especially pizza.".to_string(),
            "Alex (@main): The blog uses Astro with Tailwind.".to_string(),
        ];
        let vecs = embed(&texts).unwrap();
        let index = Index { entries: texts.iter().enumerate().map(|(i, t)| Entry { ts: i as u64, who: "x".into(), text: t.clone() }).collect(), vecs };
        let q = embed(&["How do we deploy the site?".to_string()]).unwrap().pop().unwrap();
        let all: Vec<(f32, &str)> = index.entries.iter().zip(&index.vecs).map(|(e, v)| (dot(&q, v), e.text.as_str())).collect();
        eprintln!("{all:?}");
        let hits = top(&index, &q, 100, 4);
        assert_eq!(hits.first().map(|h| h.1.text.contains("Netlify")), Some(true));
        assert!(!hits.iter().any(|h| h.1.text.contains("pizza")), "unrelated message recalled");
    }
}

#[cfg(test)]
mod live {
    /// Downloads a small model the same way bge-m3 is downloaded: YH_TEST_PULL=all-minilm cargo test -- --ignored live_pull
    #[test]
    #[ignore]
    fn live_pull() {
        let model = std::env::var("YH_TEST_PULL").unwrap_or_else(|_| "all-minilm".into());
        let mut seen = Vec::new();
        super::pull(&model, |p| seen.push(p)).unwrap();
        eprintln!("progress: {:?}", seen.iter().step_by((seen.len() / 12).max(1)).collect::<Vec<_>>());
        assert_eq!(seen.last(), Some(&100));
        assert!(super::has_model(&super::installed_models().unwrap(), &model));
    }
}

#[cfg(test)]
mod live_note {
    use super::*;

    /// The local model writes the note from a conversation: cargo test -- --ignored live_note --nocapture
    #[test]
    #[ignore]
    fn live_note() {
        let tmp = std::env::temp_dir().join(format!("yh-mem-{}", model::new_id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("YH_DATA_FILE", tmp.join("state.json"));
        let mut data = model::Data::default();
        let ch = &mut data.channels[0];
        let cid = ch.id.clone();
        let lead = data.agents[0].id.clone();
        let old = model::now() - 600;
        let msg = |author: Author, text: &str| Message { author, text: text.into(), ts: old, error: false, to_user: false, via: String::new(), attachments: Vec::new(), receipt: String::new(), idle_claim: false };
        let ch = &mut data.channels[0];
        ch.messages = vec![
            msg(Author::User, "Hey, I'm Sam. Please always answer me in short sentences, I listen while driving."),
            msg(Author::Agent(lead.clone()), "Got it, Sam."),
            msg(Author::User, "We're building a recipe website with Astro, hosted on Netlify. Let's use Tailwind, not Bootstrap, it's lighter. My API key is sk-test-123, keep it handy."),
            msg(Author::Agent(lead), "Okay: Astro, Netlify, Tailwind. I won't store keys."),
            msg(Author::User, "What's the weather like today?"),
        ];
        let shared = Shared::quiet(data);
        let names = installed_models().unwrap();
        let m = note_model(&names).unwrap();
        let mut state = State::default();
        let t = std::time::Instant::now();
        assert!(maybe_update_note(&shared, &cid, &mut state, &m));
        let note = std::fs::read_to_string(blocks_path(&cid)).unwrap();
        eprintln!("{m} in {:.1}s:\n{note}", t.elapsed().as_secs_f32());
        assert!(note.contains("Astro") && note.contains("Tailwind"));
        assert!(!note.contains("sk-test-123"), "secret written to memory");
        let _ = std::fs::remove_dir_all(tmp);
    }
}
