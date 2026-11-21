//! Every minute: look at each channel's AGENTS.md, TASKS.md and docs/ and tell you what changed
//! (and which agents were working). Also refreshes agent health every few minutes.

use crate::model::{now, Author, Message};
use crate::orchestrator::Shared;
use crate::probe;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub const INTERVAL: Duration = Duration::from_secs(60);
const ROOT_FILES: [&str; 2] = ["AGENTS.md", "TASKS.md"];
const DOC_DIRS: [&str; 2] = ["docs", "doc"];
const DOC_EXTS: [&str; 9] = ["md", "mdx", "markdown", "txt", "rst", "adoc", "yaml", "yml", "json"];
const MAX_FILES: usize = 400;
const MAX_BYTES: u64 = 512 * 1024;
const SHOW_LINES: usize = 8;

#[derive(Clone, Debug)]
pub struct Snap {
    pub hash: u64,
    pub text: Option<String>,
}

pub type Snapshot = BTreeMap<String, Snap>;

pub fn start(shared: Arc<Shared>) {
    std::thread::spawn(move || {
        let mut mem: HashMap<String, Snapshot> = HashMap::new();
        let mut tick: u64 = 0;
        let wake = shared.wake.1.clone();
        loop {
            if tick % 5 == 0 {
                let s = shared.clone();
                std::thread::spawn(move || refresh_health(&s));
            }
            beat(&shared, &mut mem);
            tick += 1;
            // Sleep a minute, or until someone asks for a check now.
            let _ = wake.lock().unwrap().recv_timeout(INTERVAL);
        }
    });
}

pub fn refresh_health(shared: &Shared) {
    // Every engine, not just the ones agents use: each is a fallback for the others.
    let kinds: Vec<_> = crate::model::ENGINES.to_vec();
    let mut seen = Vec::new();
    for kind in kinds {
        if seen.contains(&kind) {
            continue;
        }
        seen.push(kind);
        let mut fresh = probe::probe(kind);
        let mut health = shared.health.lock().unwrap();
        if let Some(old) = health.get(kind.label()) {
            // Keep what we learned from runs: bad models, and Claude's limits (only known from runs).
            fresh.bad_models = old.bad_models.clone();
            if fresh.limits.is_empty() {
                fresh.limits = old.limits.clone();
                if fresh.blocked_until.is_none() && old.blocked() {
                    fresh.blocked_until = old.blocked_until;
                    fresh.blocked_reason = old.blocked_reason.clone();
                }
            }
        }
        health.insert(kind.label().to_string(), fresh);
        drop(health);
        (shared.repaint)();
    }
}

fn beat(shared: &Arc<Shared>, mem: &mut HashMap<String, Snapshot>) {
    let channels: Vec<(String, String, HashMap<String, u64>)> = {
        let data = shared.data.lock().unwrap();
        data.channels.iter().map(|c| (c.id.clone(), c.folder.clone(), c.watch.clone())).collect()
    };
    for (cid, folder, persisted) in channels {
        shared.project.lock().unwrap().insert(cid.clone(), crate::project::inspect(Path::new(&folder)));
        let new = scan(Path::new(&folder));
        let changes = match mem.get(&cid) {
            Some(old) => diff(old, &new),
            // First look since the app started: compare with what we saved last time (hashes only).
            None if !persisted.is_empty() => diff_hashes(&persisted, &new),
            None => Vec::new(),
        };
        let first_look = !mem.contains_key(&cid);
        mem.insert(cid.clone(), new.clone());
        let who: Vec<String> = shared.activity.lock().unwrap().remove(&cid).map(|s| s.into_iter().collect()).unwrap_or_default();

        let mut data = shared.data.lock().unwrap();
        let selected = data.selected.clone();
        let read_aloud = data.settings.read_aloud;
        let Some(ch) = data.channel_mut(&cid) else { continue };
        ch.watch = new.iter().map(|(k, v)| (k.clone(), v.hash)).collect();
        if changes.is_empty() {
            continue;
        }
        let text = describe(&changes, &who, first_look);
        ch.messages.push(Message { author: Author::Heartbeat, text, ts: now(), error: false, to_user: false, via: String::new(), attachments: Vec::new(), receipt: String::new(), idle_claim: false });
        let name = ch.name.clone();
        if selected.as_deref() != Some(cid.as_str()) {
            ch.unread = true;
        }
        drop(data);
        shared.dirty.store(true, Ordering::Relaxed);
        let files: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        let summary = format!("{} changed in #{name}", list(&files));
        if !shared.app_focused.load(Ordering::Relaxed) {
            (shared.notify)(summary.clone());
        }
        if read_aloud && selected.as_deref() == Some(cid.as_str()) {
            (shared.speak)((summary, String::new()));
        }
        (shared.repaint)();
    }
    *shared.last_beat.lock().unwrap() = Some(std::time::Instant::now());
    (shared.repaint)();
}

fn list(files: &[&str]) -> String {
    match files.len() {
        0 => String::new(),
        1 => files[0].to_string(),
        2 => format!("{} and {}", files[0], files[1]),
        n => format!("{}, {} and {} more", files[0], files[1], n - 2),
    }
}

/// The files we watch in a project folder.
pub fn scan(folder: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    let Ok(entries) = std::fs::read_dir(folder) else { return out };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if path.is_file() && ROOT_FILES.iter().any(|f| f.eq_ignore_ascii_case(&name)) {
            add(&mut out, &name, &path);
        } else if path.is_dir() && DOC_DIRS.iter().any(|d| d.eq_ignore_ascii_case(&name)) {
            walk(&mut out, &path, &name, 0);
        }
    }
    out
}

fn walk(out: &mut Snapshot, dir: &Path, rel: &str, depth: usize) {
    if depth > 5 || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let path = e.path();
        let r = format!("{rel}/{name}");
        if path.is_dir() {
            walk(out, &path, &r, depth + 1);
        } else if DOC_EXTS.iter().any(|x| path.extension().is_some_and(|e| e.eq_ignore_ascii_case(x))) {
            add(out, &r, &path);
        }
        if out.len() >= MAX_FILES {
            return;
        }
    }
}

fn add(out: &mut Snapshot, rel: &str, path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() > MAX_BYTES {
        // Too big to diff: hash size + mtime.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        meta.len().hash(&mut h);
        meta.modified().ok().hash(&mut h);
        out.insert(rel.to_string(), Snap { hash: h.finish(), text: None });
        return;
    }
    let Ok(bytes) = std::fs::read(path) else { return };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    out.insert(rel.to_string(), Snap { hash: h.finish(), text: String::from_utf8(bytes).ok() });
}

#[derive(Debug)]
pub struct Change {
    pub path: String,
    pub kind: Kind,
}

#[derive(Debug)]
pub enum Kind {
    Added(usize),
    Removed,
    Modified { plus: Vec<String>, minus: Vec<String> },
    /// Changed, but we have no old text to diff against.
    Touched,
}

pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let mut out = Vec::new();
    for (path, n) in new {
        match old.get(path) {
            None => out.push(Change { path: path.clone(), kind: Kind::Added(n.text.as_deref().map(|t| t.lines().count()).unwrap_or(0)) }),
            Some(o) if o.hash != n.hash => {
                let kind = match (&o.text, &n.text) {
                    (Some(a), Some(b)) => {
                        let (plus, minus) = line_diff(a, b);
                        Kind::Modified { plus, minus }
                    }
                    _ => Kind::Touched,
                };
                out.push(Change { path: path.clone(), kind });
            }
            _ => {}
        }
    }
    for path in old.keys() {
        if !new.contains_key(path) {
            out.push(Change { path: path.clone(), kind: Kind::Removed });
        }
    }
    out
}

fn diff_hashes(old: &HashMap<String, u64>, new: &Snapshot) -> Vec<Change> {
    let mut out = Vec::new();
    for (path, n) in new {
        match old.get(path) {
            None => out.push(Change { path: path.clone(), kind: Kind::Added(n.text.as_deref().map(|t| t.lines().count()).unwrap_or(0)) }),
            Some(h) if *h != n.hash => out.push(Change { path: path.clone(), kind: Kind::Touched }),
            _ => {}
        }
    }
    for path in old.keys() {
        if !new.contains_key(path) {
            out.push(Change { path: path.clone(), kind: Kind::Removed });
        }
    }
    out
}

/// Lines added / removed (LCS-based; falls back to a set difference for huge files).
pub fn line_diff(a: &str, b: &str) -> (Vec<String>, Vec<String>) {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    // Trim common prefix/suffix first: most edits are local.
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    if a.len() * b.len() > 4_000_000 {
        let plus = b.iter().filter(|l| !a.contains(l)).map(|s| s.to_string()).collect();
        let minus = a.iter().filter(|l| !b.contains(l)).map(|s| s.to_string()).collect();
        return (plus, minus);
    }
    let (n, m) = (a.len(), b.len());
    let mut t = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * (m + 1) + j] =
                if a[i] == b[j] { t[(i + 1) * (m + 1) + j + 1] + 1 } else { t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let (mut plus, mut minus) = (Vec::new(), Vec::new());
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if t[(i + 1) * (m + 1) + j] >= t[i * (m + 1) + j + 1] {
            minus.push(a[i].to_string());
            i += 1;
        } else {
            plus.push(b[j].to_string());
            j += 1;
        }
    }
    minus.extend(a[i..].iter().map(|s| s.to_string()));
    plus.extend(b[j..].iter().map(|s| s.to_string()));
    (plus, minus)
}

pub fn describe(changes: &[Change], who: &[String], first_look: bool) -> String {
    let by = if !who.is_empty() {
        format!(" while {} {} working", who.iter().map(|h| format!("@{h}")).collect::<Vec<_>>().join(" and "), if who.len() == 1 { "was" } else { "were" })
    } else if first_look {
        " while the app was closed".into()
    } else {
        " (not by an agent in this app)".into()
    };
    let mut s = format!("**Heartbeat:** project files changed{by}\n");
    for c in changes {
        match &c.kind {
            Kind::Added(n) => s.push_str(&format!("- `{}` new file ({n} lines)\n", c.path)),
            Kind::Removed => s.push_str(&format!("- `{}` deleted\n", c.path)),
            Kind::Touched => s.push_str(&format!("- `{}` changed\n", c.path)),
            Kind::Modified { plus, minus } => {
                s.push_str(&format!("- `{}` +{} −{}\n", c.path, plus.len(), minus.len()));
                let shown: Vec<String> = minus
                    .iter()
                    .map(|l| format!("- {l}"))
                    .chain(plus.iter().map(|l| format!("+ {l}")))
                    .filter(|l| l.trim().len() > 1)
                    .collect();
                if !shown.is_empty() {
                    s.push_str("```\n");
                    for l in shown.iter().take(SHOW_LINES) {
                        s.push_str(&crate::agents::short(l, 110));
                        s.push('\n');
                    }
                    if shown.len() > SHOW_LINES {
                        s.push_str(&format!("… {} more\n", shown.len() - SHOW_LINES));
                    }
                    s.push_str("```\n");
                }
            }
        }
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_task_changes() {
        let dir = std::env::temp_dir().join(format!("yh-hb-{}", crate::model::new_id()));
        std::fs::create_dir_all(dir.join("docs/api")).unwrap();
        std::fs::write(dir.join("TASKS.md"), "- [ ] write README\n- [ ] add tests\n").unwrap();
        std::fs::write(dir.join("docs/api/intro.md"), "hello\n").unwrap();
        std::fs::write(dir.join("main.rs"), "ignored").unwrap();
        let a = scan(&dir);
        assert_eq!(a.keys().cloned().collect::<Vec<_>>(), vec!["TASKS.md", "docs/api/intro.md"]);

        std::fs::write(dir.join("TASKS.md"), "- [x] write README\n- [ ] add tests\n").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "# Agents\n").unwrap();
        std::fs::remove_file(dir.join("docs/api/intro.md")).unwrap();
        let b = scan(&dir);
        let ch = diff(&a, &b);
        let text = describe(&ch, &["codex".into()], false);
        assert!(text.contains("while @codex was working"), "{text}");
        assert!(text.contains("`AGENTS.md` new file"), "{text}");
        assert!(text.contains("`TASKS.md` +1 −1"), "{text}");
        assert!(text.contains("+ - [x] write README"), "{text}");
        assert!(text.contains("`docs/api/intro.md` deleted"), "{text}");
        assert!(diff(&b, &scan(&dir)).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn beat_posts_changes_with_who_did_it() {
        let dir = std::env::temp_dir().join(format!("yh-beat-{}", crate::model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("TASKS.md"), "- [ ] one\n").unwrap();
        let mut data = crate::model::Data::default();
        data.channels[0].folder = dir.to_string_lossy().into_owned();
        data.channels[0].messages.clear();
        let cid = data.channels[0].id.clone();
        let shared = Shared::quiet(data);
        let mut mem = HashMap::new();
        beat(&shared, &mut mem); // baseline: silent
        assert!(shared.data.lock().unwrap().channels[0].messages.is_empty());
        shared.activity.lock().unwrap().entry(cid.clone()).or_default().insert("codex".into());
        std::fs::write(dir.join("TASKS.md"), "- [x] one\n").unwrap();
        beat(&shared, &mut mem);
        let data = shared.data.lock().unwrap();
        let m = &data.channels[0].messages;
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].author, Author::Heartbeat);
        assert!(m[0].text.contains("while @codex was working") && m[0].text.contains("+ - [x] one"), "{}", m[0].text);
        assert!(data.channels[0].watch.contains_key("TASKS.md"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
