//! ElevenLabs voices for reading replies aloud (optional; Apple's voices are the default and the fallback).
//!
//! Your API key lives in the macOS Keychain ("YesHarness ElevenLabs"), never in the app's files.
//! Replies are cut into sentences; the next piece is fetched while the current one plays, so speech
//! starts after the first sentence instead of after the whole reply.

use serde_json::{json, Value};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

const SERVICE: &str = "YesHarness ElevenLabs";
const ACCOUNT: &str = "yesharness";
const API: &str = "https://api.elevenlabs.io/v1";
/// Fast, cheap and multilingual; good enough for short spoken replies.
const MODEL: &str = "eleven_flash_v2_5";
/// "Rachel", one of ElevenLabs' default voices: used until your voice list has loaded.
pub const DEFAULT_VOICE: &str = "21m00Tcm4TlvDq8ikWAM";

#[derive(Clone, Debug, PartialEq)]
pub struct Voice {
    pub id: String,
    pub name: String,
}

/// The key, once read from the Keychain (None inside = no key saved).
static KEY: Mutex<Option<Option<String>>> = Mutex::new(None);
static VOICES: Mutex<Option<Vec<Voice>>> = Mutex::new(None);
/// The last problem talking to ElevenLabs, shown in Settings (cleared when speech works again).
static PROBLEM: Mutex<Option<String>> = Mutex::new(None);

// ------------------------------------------------------------------ key (macOS Keychain)

pub fn api_key() -> Option<String> {
    let mut k = KEY.lock().unwrap();
    if k.is_none() {
        let out = Command::new("security").args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"]).stderr(Stdio::null()).output();
        let key = out.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
        *k = Some(key);
    }
    k.clone().flatten()
}

pub fn has_key() -> bool {
    api_key().is_some()
}

/// Saves the key in the Keychain. It goes to `security` on stdin, so it never shows up in a process list.
pub fn save_key(key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() || key.contains(['"', '\n', '\\']) {
        return Err("That doesn't look like an API key.".into());
    }
    use std::io::Write;
    let mut child = Command::new("security").arg("-i").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("add-generic-password -U -s \"{SERVICE}\" -a \"{ACCOUNT}\" -w \"{key}\"\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() || !out.stderr.is_empty() {
        return Err(format!("Couldn't save it in the Keychain: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    *KEY.lock().unwrap() = Some(Some(key.to_string()));
    *VOICES.lock().unwrap() = None;
    *PROBLEM.lock().unwrap() = None;
    Ok(())
}

pub fn remove_key() {
    let _ = Command::new("security").args(["delete-generic-password", "-s", SERVICE, "-a", ACCOUNT]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    *KEY.lock().unwrap() = Some(None);
    *VOICES.lock().unwrap() = None;
    *PROBLEM.lock().unwrap() = None;
}

pub fn problem() -> Option<String> {
    PROBLEM.lock().unwrap().clone()
}

fn set_problem(p: Option<String>) {
    *PROBLEM.lock().unwrap() = p;
}

// ------------------------------------------------------------------ API

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).http_status_as_error(false).build().into()
}

/// A plain-language reason for a failed request.
pub fn explain(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let detail = v["detail"]["status"].as_str().or(v["detail"]["code"].as_str()).unwrap_or_default().to_string();
    let message = v["detail"]["message"].as_str().or(v["detail"].as_str()).unwrap_or_default().to_string();
    match (status, detail.as_str()) {
        // ElevenLabs reports used-up credits as 401 too, so check that first.
        (_, "quota_exceeded") | (402, _) => "Your ElevenLabs credits are used up.".into(),
        (401, _) | (_, "invalid_api_key") => "ElevenLabs didn't accept the API key.".into(),
        (429, _) => "ElevenLabs is busy or you hit your plan's limit; try again shortly.".into(),
        (_, "voice_not_found") | (404, _) => "That ElevenLabs voice isn't available on your account.".into(),
        _ if !message.is_empty() => format!("ElevenLabs: {message}"),
        _ => format!("ElevenLabs answered with error {status}."),
    }
}

/// Your voices (premade and your own), loaded once.
pub fn voices() -> Result<Vec<Voice>, String> {
    if let Some(v) = VOICES.lock().unwrap().clone() {
        return Ok(v);
    }
    let key = api_key().ok_or("No ElevenLabs API key saved.")?;
    let mut resp = agent().get(&format!("{API}/voices")).header("xi-api-key", &key).call().map_err(|e| format!("Couldn't reach ElevenLabs: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    if status != 200 {
        let e = explain(status, &body);
        set_problem(Some(e.clone()));
        return Err(e);
    }
    let list = parse_voices(&body);
    *VOICES.lock().unwrap() = Some(list.clone());
    Ok(list)
}

/// Voices already loaded (doesn't block the UI).
pub fn cached_voices() -> Vec<Voice> {
    VOICES.lock().unwrap().clone().unwrap_or_default()
}

/// Loads the voice list in the background.
pub fn load_voices(repaint: impl Fn() + Send + 'static) {
    static LOADING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if VOICES.lock().unwrap().is_some() || !has_key() || LOADING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let _ = voices();
        LOADING.store(false, std::sync::atomic::Ordering::SeqCst);
        repaint();
    });
}

pub fn parse_voices(body: &str) -> Vec<Voice> {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let mut list: Vec<(bool, Voice)> = v["voices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| {
            let id = x["voice_id"].as_str()?.to_string();
            let name = x["name"].as_str()?.to_string();
            // Your own voices first, then ElevenLabs' defaults.
            Some((x["category"].as_str() == Some("premade"), Voice { id, name }))
        })
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase())));
    list.into_iter().map(|(_, v)| v).collect()
}

/// A voice for the n-th agent when you haven't picked one: different agents sound different.
pub fn auto_voice(n: usize) -> String {
    let list = cached_voices();
    if list.is_empty() {
        return DEFAULT_VOICE.into();
    }
    list[n % list.len()].id.clone()
}

pub fn voice_name(id: &str) -> String {
    cached_voices().into_iter().find(|v| v.id == id).map(|v| v.name).unwrap_or_else(|| if id == DEFAULT_VOICE { "Rachel".into() } else { "voice".into() })
}

/// MP3 audio for `text`. `speed`: 1.0 = normal (ElevenLabs allows 0.7–1.2).
pub fn synthesize(text: &str, voice: &str, speed: f32) -> Result<Vec<u8>, String> {
    let key = api_key().ok_or("No ElevenLabs API key saved.")?;
    let body = json!({
        "text": text,
        "model_id": MODEL,
        "voice_settings": {"stability": 0.5, "similarity_boost": 0.75, "speed": speed.clamp(0.7, 1.2)},
    });
    let url = format!("{API}/text-to-speech/{voice}?output_format=mp3_44100_128");
    let mut resp = agent()
        .post(&url)
        .header("xi-api-key", &key)
        .header("Accept", "audio/mpeg")
        .send_json(&body)
        .map_err(|e| format!("Couldn't reach ElevenLabs: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        let e = explain(status, &resp.body_mut().read_to_string().unwrap_or_default());
        set_problem(Some(e.clone()));
        return Err(e);
    }
    let audio = resp.body_mut().with_config().limit(20 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())?;
    set_problem(None);
    Ok(audio)
}

/// Pieces to fetch one after another: the first one short, so speech starts quickly.
pub fn pieces(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for s in text.split_inclusive(['.', '!', '?', '\n']) {
        let limit = if out.is_empty() { 160 } else { 600 };
        if !cur.trim().is_empty() && cur.chars().count() + s.chars().count() > limit {
            out.push(cur.trim().to_string());
            cur.clear();
        }
        cur.push_str(s);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_piece_is_short() {
        let text = "Done. The semifinals start at 2:30 PM and 8:30 PM in Turin. Doubles start at noon. Check the official schedule for changes, because times can move if matches run long. Enjoy the games!";
        let p = pieces(text);
        assert_eq!(p[0], "Done. The semifinals start at 2:30 PM and 8:30 PM in Turin. Doubles start at noon.");
        assert_eq!(p.join(" "), text);
        assert!(pieces("").is_empty());
    }

    #[test]
    fn explains_errors() {
        assert!(explain(401, r#"{"detail":{"status":"invalid_api_key","message":"Invalid API key"}}"#).contains("didn't accept"));
        assert!(explain(401, r#"{"detail":{"status":"quota_exceeded","message":"This request exceeds your quota"}}"#).contains("credits"));
        assert!(explain(500, "oops").contains("500"));
    }

    #[test]
    fn lists_your_voices_first() {
        let body = r#"{"voices":[{"voice_id":"a","name":"Rachel","category":"premade"},{"voice_id":"b","name":"My Clone","category":"cloned"},{"voice_id":"c","name":"Adam","category":"premade"}]}"#;
        let names: Vec<String> = parse_voices(body).into_iter().map(|v| v.name).collect();
        assert_eq!(names, vec!["My Clone", "Adam", "Rachel"]);
    }
}

#[cfg(test)]
mod live {
    /// Talks to the real API with an invalid key: cargo test -- --ignored live_bad_key
    #[test]
    #[ignore]
    fn live_bad_key() {
        *super::KEY.lock().unwrap() = Some(Some("sk_invalid_yesharness_test".into()));
        let e = super::synthesize("Hello there.", super::DEFAULT_VOICE, 1.0).unwrap_err();
        eprintln!("{e}");
        assert!(e.contains("didn't accept"), "{e}");
        assert_eq!(super::problem().as_deref(), Some(e.as_str()));
    }
}
