# YesHarness

A simple chat app for your local AI agents. One channel per project; in each channel your agents
(Claude Code, Codex, or any CLI) work in that project's folder. You talk (or type) in one chat, and
the agents hand work to each other.

Rust + egui. macOS is the currently built platform; Linux and Windows text-mode desktop builds are candidates, not verified releases. Voice input and read-aloud are macOS-only. See [platform compatibility](docs/platform-compatibility.md).

## Build & run

```bash
scripts/bundle.sh             # builds dist/YesHarness.app
open dist/YesHarness.app
scripts/bundle.sh --install   # or: copy it to /Applications
```

Needs Rust (`cargo`) and Xcode command line tools (Swift, for the voice helper in `helper/`, a Swift package
that pulls in FluidAudio on first build).

## How it works

It's a small virtual office. You talk; the **main agent** listens, decides who should do the work and
hands it out to the specialists, then tells you how it went.

### Your team
- **Each agent is a role on an engine.** A role is a name, a one-line description, a briefing (standing
  instructions) and its own voice. The engine is Claude Code, Codex or any CLI, plus a model. Several roles
  can share an engine and model.
- **Starter team:** **Alex** (the main agent you talk to; handle `@main`), **@developer**, **@writer** (blog posts) and **@social** (social media).
  Add more from templates (Researcher, Reviewer/editor, a blank agent, or a custom command) under ⚙ Team.
  New agents join every channel.
- **The main agent (★) routes the work.** It splits your request by role and briefs each specialist with a
  line like `@writer: …`. When they report back, it checks the work and gives you a one- or two-sentence answer.
  You can still talk to a specialist directly: "Writer, make it punchier."
- **The main agent runs the app too:** say "make a new channel for my recipe site" and it creates the
  channel (folder defaults to `~/Development/<name>`), opens it, and passes your request to the team there.
  It does this with action lines in its reply (`/new-channel`, `/send`, `/switch`) that the app carries out.
- **Receipts, not promises:** under every agent reply the app shows what the turn actually did (commands run,
  files edited, handoffs started, tokens used), counted from the engines' own event streams (`codex exec --json`, Claude Code
  `stream-json`), not from what the agent says. If a reply claims work but nothing happened, it's marked "No action
  taken", and the agent is sent back once to do it for real. The main agent also gets these facts every turn, so it
  can't report imaginary progress. Handoffs count whether written as `@developer: …` or `@developer, …` at the start
  of a sentence.
- **Nobody waits:** each message is handled right away, even while teammates work on earlier ones. An agent runs
  one turn at a time; if the one needed is busy, a **General helper** steps in. It's created automatically, runs
  on the main agent's engine, and starts extra copies (up to four) when it's busy too.
- **Quick routing (on by default):** Alex's routine turns (answering, delegating, reporting back) are one
  Codex call with short instructions and no tools, about 13k tokens instead of 20–80k. When Alex needs to look at
  the project itself, it says `/work` and that turn re-runs with full tools. Switch it in Settings → Agents.
- **Token limits:** every tool step re-sends the whole conversation, so long loops get expensive fast.
  - **Steps per turn:** Alex stops after 8 steps (it should hand work off, not investigate), General after 15,
    specialists after 30.
  - **Context size:** command output is trimmed to ~4k tokens, and Codex compacts the conversation past 60k.
  - **Per message:** once a request has used 800k tokens, it pauses. Say "continue" to go on.
- **Parallel handoffs:** when the main agent briefs several teammates in one message, they work at the
  same time, and the chat shows everyone who's working and what they're doing.
- **Speed settings per agent** (Team → Engine):
  - **Thinking:** Quick, Normal or Deep. The main agent defaults to Quick, since it mostly routes and summarizes.
  - **Your plugins:** off by default, so agents run lean, without your global MCP servers and plugins. That's
    about 30% faster per Codex turn, with smaller prompts and less quota. Turn it on for agents that need them.
- **Engines stand in for each other.** The order is Claude Code, then Codex, then **Local**. If a role's own
  engine is logged out or out of quota, it runs on the first engine that's available. The chat says so once,
  and every reply shows the engine and model that produced it.
- **Local engine:** a model on your Mac via [Ollama](https://ollama.com), driven by Codex's local (`--oss`)
  mode, so it can still read and edit files and run commands. It's free, works offline, and has no usage limits.
  It uses its own clean Codex home (`~/Library/Application Support/YesHarness/codex-local`), so your global
  MCP servers and plugins don't overflow a small model's context. The model is picked automatically: it
  prefers `gpt-oss:20b`, then Qwen3-Coder and Qwen3.x, Devstral and Gemma. Setup:
  `ollama pull gpt-oss:20b` (13 GB; fits a 16 GB+ Mac). The app starts Ollama if it isn't running.

### Talk, don't type
- **Always listening (the default):** just talk, like walking up to someone in the office. Every pause sends
  what you said. The macOS mic indicator stays on while it listens.
- **Voice commands:**
  - "stop" stops the agents.
  - "quiet" stops the talking (when said between replies; while an agent talks, press Space instead).
  - "switch to <channel>" changes channel.
  - "stop listening" turns always-listening off.
  - Filler like "um" is ignored.
- **Push to talk** instead: hold **Space** (or **Ctrl+Option+Space** from any app, or the big mic).
- **Read replies:** the agents answer out loud, each in its own voice and one at a time. While an agent is
  speaking (plus a short tail for room echo), the app doesn't listen, so it can't hear itself and answer its
  own words. Anything that still sounds like an agent's recent words is dropped too. To interrupt, press
  Space or click the speaker icon.
- **No runaway agents:** every agent run has a small watchdog. If the app quits, crashes or is force-quit,
  the run is stopped within a couple of seconds.
- **Speech engine** (Settings → Voice → Recognition):
  - **Apple**: built into macOS (`SpeechAnalyzer` on macOS 26+). Live text as you speak, nothing to
    download, and agent and channel names are given as vocabulary hints.
  - **Parakeet Ultra** (default): NVIDIA's Parakeet TDT (moondream's "Ultra" post-training) running on the Neural Engine
    through [FluidAudio](https://github.com/FluidInference/FluidAudio). One-time setup of about 0.7 GB. Apple stays
    in use until it's ready. In a five-clip test it made half as many mistakes as Apple's recognizer (2.2% vs 4.5%
    words wrong), at a similar speed (~0.5 s per sentence). Live text comes from re-transcribing about every 0.6 s.
- **Images:** drag an image onto the window and it goes with your next message, typed or spoken. It's copied to
  `<project>/.yesharness/attachments/`, which is excluded from Git. Codex receives it as an image input, and the
  other engines are pointed at the file.
- The ⌨ button shows a text box if you do want to type.

### Agents pick themselves
- On launch and every 5 minutes, the app asks each CLI what you can use:
  - **Codex:** `codex app-server` reports your plan, models and 5-hour/weekly usage.
  - **Claude Code:** its `initialize` call reports models and whether you're logged in. Limits arrive with each reply.
- **Model "Auto"** (the default) uses the best model your account allows. If a model is rejected, the app marks it
  and switches. This also works around a bad `model =` line in `~/.codex/config.toml`.
- **Failover:** if an agent is logged out or hits its limit, another agent in the channel takes over, and the
  chat says so. The sidebar shows each agent's model and usage; hover for details.

### Project at a glance
Under each channel's name, small chips show what's in its folder:
- **GitHub:** the repository and branch. Click to open it on GitHub.
- **AGENTS.md:** whether it exists and has content.
- **TASKS.md:** open tasks.
- **docs/:** how many docs it has.

Clicking the tasks, AGENTS.md or docs chip opens a slide-out on the right with the file rendered as formatted
text. Tasks are real checkboxes: click one to tick it off in the file. The panel updates live as agents
change the file. The docs chip lists everything in `docs/`; click a file to read it. Close it with ✕ or Esc.
Missing pieces are one click away: "No AGENTS.md" or "No TASKS.md" asks the team to write one. Everything is read
straight from the folder (no `git` needed), refreshed every minute and after each agent turn.

### Settings (gear icon)
- **Appearance:** Automatic (follows macOS), Light or Dark.
- **Voice:** always-listening or push-to-talk, read replies, speed, and natural voices. Apple's free
  Premium/Enhanced voices (System Settings → Accessibility → Spoken Content → System voice → Manage Voices…)
  sound far more human. YesHarness picks them up automatically, and the main agent gets the best one.

### Heartbeat
Every minute the app checks each channel's **AGENTS.md**, **TASKS.md** and **docs/**. If something changed, it
posts a card with the changed lines and which agent was working at the time. It also sends a macOS notification
when the app is in the background, and reads a short summary aloud. Click "♥ checked …" in the header to check now.

### Teamwork
- **Lead agent:** each channel has a lead (★). Anything not addressed to a specific agent goes to it.
- **Handoffs:** an agent hands off work by writing a line `@other: <task>`. The app runs that agent, posts its
  reply in the chat, then gives the turn back to the first agent to check the work and answer you. At most 10 agent
  turns per message, and handoffs nest at most 2 levels deep.
- **Memory:** each agent resumes its own CLI session per channel and only sees messages it hasn't read yet.
- **Permissions** (Agents settings):
  - Claude without full access can edit files, but its shell commands are blocked.
  - Codex without full access runs sandboxed to the project folder.
  - **Internet** (on by default): agents can use the network inside their sandbox (curl, whois, package
    installs, APIs). File edits stay limited to the project folder.
  - "Full access" removes these limits. Only turn it on for projects you trust.
- **Custom agents:** any command works. The prompt is sent on stdin, and whatever the command prints is the reply.

State is stored in `~/Library/Application Support/YesHarness/state.json`.

## Control the running app from Terminal

```bash
Y=/Applications/YesHarness.app/Contents/MacOS/yesharness
$Y --new-channel website ~/Development/my-site        # creates (or opens) a channel for a folder
$Y --send website "Build a landing page for my app"   # says it in that channel, as you
```

Requests go through a small inbox folder (`~/Library/Application Support/YesHarness/inbox`). The app picks
them up within a moment, which makes it easy to script, or to use from Shortcuts or other tools.

## Troubleshooting from Terminal

```bash
target/release/yesharness --ask "hello"                      # checks agents, then runs the default agents here
YH_LEAD=codex YH_CODEX_MODEL=gpt-6-sol target/release/yesharness --ask "hi" "what did I just say?"
```

## Code map

- `src/orchestrator.rs`: routing, handoffs, prompts, session/seen tracking
- `src/agents.rs`: runs `claude` / `codex` / custom commands, parses their JSON event streams
- `src/env.rs`: PATH discovery (apps opened from Finder get a very short PATH), process-tree kill
- `src/ui.rs`: the egui interface
- `src/probe.rs`: logins, models, limits; automatic model choice
- `src/project.rs`: GitHub / AGENTS.md / TASKS.md / docs detection per channel
- `src/heartbeat.rs`: the 1-minute watcher for AGENTS.md / TASKS.md / docs
- `src/inbox.rs`: `--send` / `--new-channel` control of the running app
- `src/voice.rs` + `helper/dictate.swift`: speech-to-text (push-to-talk, hands-free) and reading replies aloud

Test the speech engine without a microphone:

```bash
say -o /tmp/t.aiff "Codex, run the tests" && target/release/yh-dictate --file /tmp/t.aiff
```
