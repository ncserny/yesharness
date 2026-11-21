# Platform compatibility audit

Audited 2026-11-20 against the local source and locked dependencies.

**Update 2026-11-21:** hosted Linux tests/release build now pass; Windows compiled but failed one attachment-path test. Hosted macOS Rust tests/release build pass, with bundling still running at review time. No platform is claimed as an interactively verified release. See TASKS.md for current evidence.

Historical audit: **macOS was the only platform built in the original session. Linux and Windows were not verified releases.** Only `aarch64-apple-darwin` is installed here. The shared Rust UI, orchestration and persistence provide a plausible port, but do not prove that either other platform compiles or works end to end.

| Area | macOS | Linux | Windows |
| --- | --- | --- | --- |
| Desktop UI and text chat | Native debug/release build passes | eframe has Linux backends; native build/runtime untested | eframe has Windows backend; native build/runtime untested |
| Dictation | Bundled Swift helper, Apple Speech frameworks | No implementation | No implementation |
| Read aloud | `say`, system voices | No implementation | No implementation |
| Global voice shortcut | Ctrl+Option+Space registration; interactive check pending | Disabled because voice is unavailable | Disabled because voice is unavailable |
| Notifications | `osascript` Notification Center command | No-op | No-op |
| Custom commands | `sh -c` | `sh -c` branch present, untested | `cmd /C` branch present, untested |
| Stop process tree | Unix process group + TERM/KILL | Same Unix branch; needs process-tree test | `taskkill /T /F /PID`; needs native process-tree test |
| CLI discovery | Login-shell PATH + common install paths | Unix code present; fallback `/bin/zsh` may be absent, then inherited PATH is used | Inherited PATH, `.exe/.cmd/.bat` discovery; actual CLI launch/quoting untested |
| State | `~/Library/Application Support/YesHarness/state.json` | `dirs::data_dir()/YesHarness/state.json` (normally XDG data directory) | `dirs::data_dir()/YesHarness/state.json` (normally roaming AppData) |
| Packaging | `scripts/bundle.sh` produces signed `.app` | No package/installer pipeline | No package/installer pipeline |

The `global-hotkey` dependency documents Linux support as **X11 only**, not native Wayland: [upstream supported platforms](https://github.com/tauri-apps/global-hotkey#supported-platforms). Adding Linux voice later would also require a Wayland shortcut strategy. Do not confuse this dependency limitation with eframe's window-rendering support.

## Changes made during the interface refresh

- Linux/Windows force text input on and voice settings off when loading state; no unusable microphone dock or voice hotkey registration.
- Fresh settings use platform-aware defaults; the welcome message works with either input method.
- Header path shortening recognizes both slash directions.
- Existing macOS voice preferences and saved conversations remain intact.

## Validation and remaining work

`cargo test` passes all 17 tests on macOS. These cover stream parsing, routing, handoffs, role failover, prompt context, heartbeat changes, speech text cleanup and voice commands. Native release compilation and the macOS bundling script are checked separately. Tests use local mocks/custom commands; they do not establish live Claude/Codex authentication or OS-level microphone operation.

Interactive layout and voice checks remain pending: the sandboxed preview could not connect to macOS UI services, and Computer Use access to YesHarness was denied. No screenshot or manual-flow pass is claimed.

Before announcing Linux/Windows support:

1. Build and test natively on each OS (Linux X11 and Wayland separately). Install the platform development libraries required by eframe/rfd; the proposed Ubuntu dependency recipe is in `.github/workflows/ci.yml` and still needs a hosted run.
2. Exercise CLI discovery, login/model probes, prompt streaming, session resume and stop with a spawned child process. Windows npm command shims and paths with spaces need particular attention. Existing shell-based test fixtures also need Windows equivalents.
3. Verify folder dialogs, state persistence and heartbeat file changes.
4. Execute the prepared CI workflow and validate its development artifacts. Add Linux/Windows installers and release signing/notarization as needed. Implement notifications and choose voice backends if feature parity is required.

A native checkout can attempt `cargo build --release` and `cargo test`; `scripts/bundle.sh` is macOS-only. This is a development starting point, not a claim of supported distribution.

## Preview the interface on macOS

Quit the running app when its agents are idle, then open `dist/YesHarness.app` from this project. The running app does not hot-reload Rust UI changes. No `/Applications` install or running-session restart was performed.

Check channel switching, Team and channel menus, typing/Enter/Shift+Enter, microphone tap/hold, listening modes, read-aloud/Quiet, and Stop agents. Resize to the minimum window size to check wrapping. Use `YH_DATA_FILE` with a separate JSON file for an isolated preview; the normal state location is unchanged.

## CI preparation — 2026-11-21

`.github/workflows/ci.yml` runs on pushes, pull requests, and manual dispatch. Its independent native jobs use macOS 26 ARM64, Ubuntu 24.04 x64, and Windows 2025 x64, Rust 1.86.0 (the declared minimum), and the checked-in Cargo.lock. Each job runs `cargo test --locked --all-targets` and `cargo build --locked --release`. No live model credentials are required. The Ollama integration test remains opt-in.

The macOS job additionally builds the Swift package helper, checks the app signature, and archives the ad-hoc-signed app. Linux archives the executable with its execute permission; Windows uploads the executable. These are development artifacts, not installers or verified releases. The Linux binary still depends on the target system's desktop libraries. Jobs use read-only repository permissions and do not publish releases or deploy anything.

Five orchestration tests use Unix shell fixtures and are explicitly ignored on Windows: delegation/wrap-up, parallel handoffs, new-channel handoff, stand-in deduplication, and quota failover. A Windows-only smoke test exercises `cmd /C`, stdin, and a reply through the real custom-agent path. Windows equivalents for the five richer scenarios, child-process termination, npm shims, and path quoting remain outstanding. The Unix watchdog test is also Unix-only. Shared parsing, routing, model/state, and UI helper tests still run on Windows.

Local evidence: Rust 1.86.0 on Apple Silicon passed `cargo test --offline --locked --all-targets` (31 passed, 1 ignored) and the locked release build. Workflow YAML parsing and `bash -n scripts/bundle.sh` passed; actionlint is not installed. Only the macOS Rust target is installed here, so no Linux/Windows execution or cross-build pass is claimed.

The bundling script now propagates Swift build failures instead of hiding them and potentially copying a stale helper. The current Swift-package-based helper could not be rebuilt in this session: its default module cache is outside writable paths; relocating the module cache into target/ then reached `sandbox-exec: sandbox_apply: Operation not permitted`. Thus this run verified the Rust release binary, not a fresh full app bundle. Earlier bundle results above describe the source and helper used at that time.

Publication and hosted execution have since occurred. At review of commit `7d0f596`, [CI run 44841363709](https://github.com/ncserny/yesharness/actions/runs/44841363709) shows Linux success, Windows failure in the attachment-path assertion (25 passed, 1 failed, 6 ignored), and macOS Rust tests/release success with app bundling still in progress. Local tests now pass 32 with 1 ignored. Git fetch remains blocked in this agent session, but the connector confirms live main matches the checkout. Interactive X11, Wayland, Windows, macOS voice, and live engine checks remain separate requirements.

Workflow references: [GitHub runner labels](https://docs.github.com/en/actions/reference/runners/github-hosted-runners), [eframe Linux dependencies](https://github.com/emilk/egui/blob/main/crates/eframe/README.md), [checkout action](https://github.com/actions/checkout), and [artifact uploads](https://github.com/actions/upload-artifact).
