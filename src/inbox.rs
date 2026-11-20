//! Talk to the running app from the command line (or scripts, Shortcuts, other tools):
//!   yesharness --new-channel <name> <folder>
//!   yesharness --send <channel> <message…>
//! Each command drops a small JSON file into the inbox folder; the app picks it up within a moment.
//! Only processes running as you can write there, same as the app's own state file.

use crate::model::{self, Channel, Message};
use crate::orchestrator::{self, Shared};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub fn dir() -> PathBuf {
    model::data_file().parent().map(|p| p.join("inbox")).unwrap_or_else(|| PathBuf::from("inbox"))
}

/// Queue a request for the app (atomic write, so the app never reads half a file).
pub fn post(req: Value) -> std::io::Result<PathBuf> {
    let d = dir();
    std::fs::create_dir_all(&d)?;
    let path = d.join(format!("{}.json", model::new_id()));
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&req)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn send(channel: &str, text: &str) -> Value {
    json!({ "op": "send", "channel": channel, "text": text })
}

pub fn new_channel(name: &str, folder: &str) -> Value {
    json!({ "op": "new_channel", "name": name, "folder": folder })
}

pub fn start(shared: Arc<Shared>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(300));
        let Ok(entries) = std::fs::read_dir(dir()) else { continue };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort(); // ids are time-ordered: handle in the order they were sent
        for f in files {
            let req = std::fs::read(&f).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let _ = std::fs::remove_file(&f);
            if let Some(req) = req {
                handle(&shared, &req);
            }
        }
    });
}

fn find_channel(shared: &Shared, name: &str) -> Option<String> {
    let want = orchestrator::channel_slug(name);
    let data = shared.data.lock().unwrap();
    data.channels.iter().find(|c| c.name == want || c.id == want).map(|c| c.id.clone())
}

pub fn handle(shared: &Arc<Shared>, req: &Value) {
    match req["op"].as_str() {
        Some("new_channel") => {
            let name = orchestrator::channel_slug(req["name"].as_str().unwrap_or("new"));
            let folder = req["folder"].as_str().unwrap_or("").to_string();
            if folder.is_empty() {
                return;
            }
            let _ = std::fs::create_dir_all(&folder);
            if let Some(id) = find_channel(shared, &name) {
                shared.data.lock().unwrap().selected = Some(id);
            } else {
                let mut data = shared.data.lock().unwrap();
                let ch = Channel {
                    id: model::new_id(),
                    name: name.clone(),
                    folder: folder.clone(),
                    members: data.agents.iter().map(|a| a.id.clone()).collect(),
                    messages: vec![Message::system(format!("Channel created. The team works in {folder}."))],
                    sessions: HashMap::new(),
                    seen: HashMap::new(),
                    watch: HashMap::new(),
                    unread: false,
                };
                data.selected = Some(ch.id.clone());
                data.channels.push(ch);
            }
            shared.dirty.store(true, Ordering::Relaxed);
            let _ = shared.wake.0.lock().unwrap().send(()); // heartbeat: baseline its files now
            (shared.repaint)();
        }
        Some("send") => {
            let (Some(ch), Some(text)) = (req["channel"].as_str(), req["text"].as_str()) else { return };
            let Some(id) = find_channel(shared, ch) else { return };
            shared.data.lock().unwrap().selected = Some(id.clone());
            shared.dirty.store(true, Ordering::Relaxed);
            orchestrator::send(shared, &id, text.to_string());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_channel_and_sends() {
        let shared = Shared::quiet(model::Data::default());
        let folder = std::env::temp_dir().join(format!("yh-inbox-{}", model::new_id()));
        handle(&shared, &new_channel("#site", folder.to_str().unwrap()));
        assert!(folder.is_dir());
        let data = shared.data.lock().unwrap();
        let ch = data.channels.iter().find(|c| c.name == "site").expect("channel");
        assert_eq!(data.selected.as_deref(), Some(ch.id.as_str()));
        assert_eq!(ch.members.len(), data.agents.len());
        drop(data);
        let _ = std::fs::remove_dir_all(folder);
    }
}
