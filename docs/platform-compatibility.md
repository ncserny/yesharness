# Platform compatibility audit

Audited 2026-11-20 against the local source and locked dependencies.

**macOS is the only platform built in this session. Linux and Windows are not verified releases.** Only `aarch64-apple-darwin` is installed here. The shared Rust UI, orchestration and persistence provide a plausible port, but do not prove that either other platform compiles or works end to end.

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

1. Build and test natively on each OS (Linux X11 and Wayland separately). Install the platform development libraries required by eframe/rfd; the project has no validated dependency recipe yet.
2. Exercise CLI discovery, login/model probes, prompt streaming, session resume and stop with a spawned child process. Windows npm command shims and paths with spaces need particular attention. Existing shell-based test fixtures also need Windows equivalents.
3. Verify folder dialogs, state persistence and heartbeat file changes.
4. Add packaging and CI. Implement notifications and choose voice backends if feature parity is required.

A native checkout can attempt `cargo build --release` and `cargo test`; `scripts/bundle.sh` is macOS-only. This is a development starting point, not a claim of supported distribution.

## Preview the interface on macOS

Quit the running app when its agents are idle, then open `dist/YesHarness.app` from this project. The running app does not hot-reload Rust UI changes. No `/Applications` install or running-session restart was performed.

Check channel switching, Team and channel menus, typing/Enter/Shift+Enter, microphone tap/hold, listening modes, read-aloud/Quiet, and Stop agents. Resize to the minimum window size to check wrapping. Use `YH_DATA_FILE` with a separate JSON file for an isolated preview; the normal state location is unchanged.
