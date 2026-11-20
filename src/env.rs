//! Apps launched from Finder get a tiny PATH (/usr/bin:/bin:...), so `claude` and `codex`
//! wouldn't be found. We ask the user's login shell for its PATH once and add common dirs.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static PATH: OnceLock<String> = OnceLock::new();

/// Warm up in the background so the first message isn't delayed.
pub fn init() {
    std::thread::spawn(|| {
        let _ = path();
    });
}

pub fn path() -> &'static str {
    PATH.get_or_init(compute)
}

fn compute() -> String {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(p) = login_shell_path() {
        dirs.extend(std::env::split_paths(&p));
    }
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    if let Some(home) = dirs::home_dir() {
        for d in [".local/bin", ".cargo/bin", ".bun/bin", ".npm-global/bin", ".volta/bin", ".claude/local"] {
            dirs.push(home.join(d));
        }
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        dirs.push(PathBuf::from(d));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    std::env::join_paths(dirs).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(unix)]
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args(["-l", "-c", "printf '__YH__%s__YH__' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(5) => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let start = s.find("__YH__")? + 6;
    let end = s[start..].find("__YH__")? + start;
    Some(s[start..end].to_string())
}

#[cfg(not(unix))]
fn login_shell_path() -> Option<String> {
    None
}

/// Full path of a program on our PATH, if it exists.
pub fn which(program: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    for dir in std::env::split_paths(path()) {
        for ext in exts {
            let p = dir.join(format!("{program}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// A Command with our PATH, a clean environment, and its own process group (so Stop kills
/// the whole tree: agents spawn shells, test runners, ...).
pub fn command(program: &str) -> Command {
    let resolved = which(program).map(|p| p.into_os_string()).unwrap_or_else(|| program.into());
    let mut cmd = Command::new(resolved);
    cmd.env("PATH", path());
    // If this app itself was started from inside Claude Code, don't leak its session vars
    // into the agents we spawn (they'd think they're nested and use the wrong auth).
    let nested = std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_some();
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if k == "CLAUDECODE"
            || k.starts_with("CLAUDE_CODE_")
            || k.starts_with("CLAUDE_AGENT_SDK")
            || (nested && k == "ANTHROPIC_BASE_URL")
        {
            cmd.env_remove(k.as_ref());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

/// A watchdog for a process tree we started: if this app goes away (quit, crash, force-quit),
/// it stops the tree, so agents never keep running on their own. Kill the returned child when the
/// work finishes normally.
pub fn guard(pid: u32) -> Option<std::process::Child> {
    guard_for(std::process::id(), pid)
}

fn guard_for(app: u32, pid: u32) -> Option<std::process::Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let script = format!(
            "while kill -0 {app} 2>/dev/null && kill -0 {pid} 2>/dev/null; do sleep 1; done; \
             if kill -0 {pid} 2>/dev/null; then kill -TERM -{pid} 2>/dev/null; sleep 2; kill -KILL -{pid} 2>/dev/null; fi"
        );
        Command::new("/bin/sh")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0) // survives the app, by design
            .spawn()
            .ok()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

/// Kill a process and everything it started.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        let group = format!("-{pid}");
        let _ = Command::new("kill").args(["-TERM", &group]).stderr(Stdio::null()).status();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            let _ = Command::new("kill").args(["-KILL", &group]).stderr(Stdio::null()).status();
        });
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).status();
    }
}

/// A system notification (macOS Notification Center).
pub fn notify(text: String) {
    #[cfg(target_os = "macos")]
    {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!("display notification \"{}\" with title \"YesHarness\"", esc(&text));
        let _ = Command::new("osascript").args(["-e", &script]).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = text;
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn alive(pid: u32) -> bool {
        Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
    }

    /// A stand-in "app" exits; the watchdog must then stop the "agent" it was guarding.
    #[test]
    fn watchdog_stops_agent_when_app_is_gone() {
        let mut app = Command::new("sleep").arg("1").spawn().unwrap();
        let mut agent = Command::new("sleep").arg("60").process_group(0).spawn().unwrap();
        let agent_pid = agent.id();
        let mut w = super::guard_for(app.id(), agent_pid).unwrap();
        let _ = app.wait();
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(8) {
            if let Ok(Some(_)) = agent.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let stopped = !alive(agent_pid) || agent.try_wait().ok().flatten().is_some();
        let _ = agent.kill();
        let _ = w.kill();
        assert!(stopped, "agent still running after the app went away");
    }
}
