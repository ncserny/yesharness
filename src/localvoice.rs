//! Natural voices that run on this Mac (the default for reading replies aloud):
//! Kokoro-82M for English (Apache-2.0, ~90 MB) and Kyutai Pocket TTS for German, French, Italian and
//! Portuguese (downloaded the first time it's needed). Both run in the voice helper (`yh-dictate --speak`,
//! see helper/Sources/yh-dictate/Speak.swift), which stays running so replies start quickly.
//! Anything it can't read (another language, an error) is read by the Apple voice instead.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Mutex;
use std::time::Duration;

/// English voices (Kokoro), nicest first: (id, name).
pub const VOICES: [(&str, &str); 10] = [
    ("af_heart", "Heart (American)"),
    ("am_michael", "Michael (American)"),
    ("bf_emma", "Emma (British)"),
    ("am_fenrir", "Fenrir (American)"),
    ("af_bella", "Bella (American)"),
    ("bm_george", "George (British)"),
    ("af_nicole", "Nicole (American)"),
    ("am_puck", "Puck (American)"),
    ("bf_isabella", "Isabella (British)"),
    ("bm_daniel", "Daniel (British)"),
];

/// A different voice for each agent unless you pick one.
pub fn auto_voice(n: usize) -> &'static str {
    VOICES[n % VOICES.len()].0
}

pub fn voice_name(id: &str) -> String {
    VOICES.iter().find(|(v, _)| *v == id).map(|(_, n)| n.to_string()).unwrap_or_else(|| id.to_string())
}

struct Server {
    child: Child,
    /// Kills the helper if the app dies without cleaning up.
    _watchdog: Option<Child>,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// One request at a time (the helper answers in order).
static BUSY: Mutex<()> = Mutex::new(());
/// "" = ready; otherwise what it's doing ("Loading the local voice…") or the last problem.
static STATUS: Mutex<String> = Mutex::new(String::new());

pub fn status() -> String {
    STATUS.lock().unwrap().clone()
}

/// Starts the voice helper if it isn't running (loads the English voice in the background).
pub fn start() {
    let mut s = SERVER.lock().unwrap();
    if s.as_mut().is_some_and(|srv| matches!(srv.child.try_wait(), Ok(None))) {
        return;
    }
    *s = None;
    let Some(path) = crate::voice::helper_path() else {
        *STATUS.lock().unwrap() = "Voice helper not found.".into();
        return;
    };
    let child = Command::new(path).arg("--speak").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let Ok(mut child) = child else {
        *STATUS.lock().unwrap() = "Couldn't start the local voice.".into();
        return;
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else { return };
    let (tx, rx) = channel();
    *STATUS.lock().unwrap() = "Loading the local voice…".into();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // CoreML sometimes prints its own messages on the same line; keep what's ours.
            let line = match ["D:", "E:", "I:", "R:"].iter().filter_map(|t| line.find(t)).min() {
                Some(i) => line[i..].to_string(),
                None => continue,
            };
            if let Some(info) = line.strip_prefix("I:") {
                *STATUS.lock().unwrap() = info.to_string();
            } else if line.starts_with("R:") {
                STATUS.lock().unwrap().clear();
            } else if tx.send(line).is_err() {
                break;
            }
        }
    });
    let watchdog = crate::env::guard(child.id());
    *s = Some(Server { child, _watchdog: watchdog, stdin, lines: rx });
}

pub fn stop() {
    if let Some(mut s) = SERVER.lock().unwrap().take() {
        let _ = s.child.kill();
        let _ = s.child.wait();
        if let Some(mut w) = s._watchdog.take() {
            let _ = w.kill();
            let _ = w.wait();
        }
    }
}

/// Speech for `text` as a WAV file. `speed`: 1.0 = normal.
pub fn synthesize(text: &str, voice: &str, speed: f32) -> Result<PathBuf, String> {
    let _one = BUSY.lock().unwrap_or_else(|e| e.into_inner());
    start();
    let out = std::env::temp_dir().join(format!("yh-voice-{}.wav", crate::model::new_id()));
    let req = serde_json::json!({ "text": text, "voice": voice, "speed": speed.clamp(0.5, 2.0), "out": out.to_string_lossy() });
    let mut guard = SERVER.lock().unwrap();
    let srv = guard.as_mut().ok_or("The local voice isn't running.")?;
    if writeln!(srv.stdin, "{req}").and_then(|_| srv.stdin.flush()).is_err() {
        *guard = None;
        return Err("The local voice stopped.".into());
    }
    let path = out.to_string_lossy().into_owned();
    // The first reply may wait for the download (~90 MB, once); later ones take well under a second.
    loop {
        let line = match srv.lines.recv_timeout(Duration::from_secs(180)) {
            Ok(l) => l,
            Err(_) => {
                drop(guard);
                stop();
                return Err("The local voice didn't answer.".into());
            }
        };
        if line.strip_prefix("D:") == Some(path.as_str()) {
            STATUS.lock().unwrap().clear();
            return Ok(out);
        }
        if let Some(rest) = line.strip_prefix("E:") {
            let (p, msg) = rest.split_once('\t').unwrap_or(("", rest));
            if p == path || p.is_empty() {
                if msg != "unsupported" {
                    *STATUS.lock().unwrap() = format!("Local voice problem: {msg}");
                }
                return Err(msg.to_string());
            }
        }
    }
}

/// Pieces read one after another: the first one short, so speech starts quickly.
pub fn pieces(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for s in text.split_inclusive(['.', '!', '?', '\n']) {
        let limit = if out.is_empty() { 160 } else { 400 };
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
    fn every_agent_gets_a_voice() {
        assert_eq!(auto_voice(0), "af_heart");
        assert_ne!(auto_voice(1), auto_voice(0));
        assert_eq!(voice_name("bf_emma"), "Emma (British)");
    }

    /// Needs the helper built (scripts/bundle.sh or swift build): cargo test -- --ignored live_local_voice
    #[test]
    #[ignore]
    fn live_local_voice() {
        let f = synthesize("Hi, I'm Alex. This runs on your Mac.", "af_heart", 1.2).unwrap();
        assert!(std::fs::metadata(&f).unwrap().len() > 10_000);
        let de = synthesize("Hallo, ich bin Alex. Das läuft auf deinem Mac.", "af_heart", 1.0).unwrap();
        assert!(std::fs::metadata(&de).unwrap().len() > 10_000);
        let _ = std::fs::remove_file(f);
        let _ = std::fs::remove_file(de);
        stop();
    }
}
