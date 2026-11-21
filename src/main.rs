#![cfg_attr(windows, windows_subsystem = "windows")]

mod agents;
mod env;
mod heartbeat;
mod icons;
mod inbox;
mod memory;
mod model;
mod orchestrator;
mod probe;
mod project;
mod theme;
mod ui;
mod voice;

fn main() -> eframe::Result<()> {
    env::init();
    let args: Vec<String> = std::env::args().collect();
    // Talk to the running app: `--new-channel <name> <folder>`, `--send <channel> <message…>`.
    match args.get(1).map(String::as_str) {
        Some("--new-channel") if args.len() >= 4 => {
            let r = inbox::post(inbox::new_channel(&args[2], &args[3]));
            println!("{}", if r.is_ok() { "queued: new channel" } else { "couldn't write to the inbox" });
            return Ok(());
        }
        Some("--send") if args.len() >= 4 => {
            let r = inbox::post(inbox::send(&args[2], &args[3..].join(" ")));
            println!("{}", if r.is_ok() { "queued: message" } else { "couldn't write to the inbox" });
            return Ok(());
        }
        _ => {}
    }
    if args.get(1).map(String::as_str) == Some("--ask") {
        headless(&args[2..]);
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: {
            let v = eframe::egui::ViewportBuilder::default()
                .with_title("YesHarness")
                .with_inner_size([1120.0, 760.0])
                .with_min_inner_size([760.0, 480.0]);
            // macOS: unified, transparent title bar; the sidebar runs up behind the traffic lights.
            if cfg!(target_os = "macos") {
                v.with_fullsize_content_view(true).with_titlebar_shown(false).with_title_shown(false)
            } else {
                v
            }
        },
        ..Default::default()
    };
    eframe::run_native("YesHarness", options, Box::new(|cc| Ok(Box::new(ui::App::new(cc)))))
}

/// `yesharness --ask "message" ["next message" ...]`: send messages through the default agents
/// in the current folder and print the conversation. Handy for checking your setup from Terminal.
/// Env: YH_LEAD=codex makes Codex the lead, YH_CODEX_MODEL / YH_CLAUDE_MODEL pick models.
fn headless(texts: &[String]) {
    let mut data = model::Data::default();
    for a in data.agents.iter_mut() {
        let var = format!("YH_{}_MODEL", a.handle.to_uppercase());
        if let Ok(m) = std::env::var(var) {
            a.model = m;
        }
    }
    if std::env::var("YH_LEAD").as_deref() == Ok("codex") {
        data.channels[0].members.reverse();
    }
    let ch = &mut data.channels[0];
    ch.messages.clear();
    ch.folder = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let cid = ch.id.clone();
    let shared = orchestrator::Shared::quiet(data);
    eprintln!("  … checking agents (logins, models, limits)");
    heartbeat::refresh_health(&shared);
    for a in shared.data.lock().unwrap().agents.iter() {
        if let Some(h) = shared.health_of(a) {
            let limits: Vec<String> = h.limits.iter().map(|l| format!("{} {:.0}%", l.label, l.used_pct)).collect();
            eprintln!(
                "  @{}: logged_in={:?} plan={:?} model={:?} limits=[{}]{}",
                a.handle,
                h.logged_in,
                h.plan,
                probe::pick_model(a, Some(&h)),
                limits.join(", "),
                h.why_unavailable().map(|w| format!(" UNAVAILABLE: {w}")).unwrap_or_default()
            );
        }
    }
    let mut printed = 0;
    // YH_ATTACH=/path/image.png: attach an image to the first message (testing).
    let mut attach: Vec<String> = std::env::var("YH_ATTACH").ok().into_iter().collect();
    for text in texts {
        orchestrator::send_with(&shared, &cid, text.to_string(), std::mem::take(&mut attach));
        printed = print_until_idle(&shared, &cid, printed);
    }
}

fn print_until_idle(shared: &std::sync::Arc<orchestrator::Shared>, cid: &str, mut printed: usize) -> usize {
    let mut last_status = String::new();
    loop {
        let running = {
            let runs = shared.runs.lock().unwrap();
            if let Some(r) = runs.get(cid) {
                let s = r.working.iter().map(|(a, st)| format!("@{a} {st}")).collect::<Vec<_>>().join(" | ");
                if s != last_status {
                    eprintln!("  … {s}");
                    last_status = s;
                }
            }
            runs.contains_key(cid)
        };
        {
            let data = shared.data.lock().unwrap();
            for m in &data.channels[0].messages[printed..] {
                let who = match &m.author {
                    model::Author::User => "You".to_string(),
                    model::Author::System => "system".to_string(),
                    model::Author::Heartbeat => "heartbeat".to_string(),
                    model::Author::Agent(id) => data.agent(id).map(|a| format!("@{}", a.handle)).unwrap_or_default(),
                };
                println!("\n[{who}{}]\n{}", if m.error { " ERROR" } else { "" }, m.text);
                if m.idle_claim {
                    println!("   ⚠ no action taken");
                } else if !m.receipt.is_empty() {
                    println!("   ↳ {}", m.receipt);
                }
            }
            printed = data.channels[0].messages.len();
        }
        if !running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    printed
}
