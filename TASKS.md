# Tasks

Reviewed 2026-11-21 against local source/history, GitHub main, and hosted CI. No AGENTS.md is present in this project. This review updates notes only; outstanding implementation work has not been started.

## Completed and verified

- [x] Initialize Git and publish to https://github.com/ncserny/yesharness. Local HEAD, origin/main, and the live GitHub main branch all point to `7d0f59601f3cae10074d6318b0c9a9b62ff69209`. Working tree was clean before this review. Earlier initialization/publication blockers are historical, not unfinished project tasks.
- [x] Add Git exclusions for dependencies, build output, environment secrets, private keys, and local configuration; retain Cargo.lock.
- [x] Implement persisted Alex display name, keeping the internal main handle; support Alex/@alex and @main in addressing and handoffs. Covered by model and routing tests.
- [x] Add optional Parakeet Ultra recognition through FluidAudio 0.18.4, with Apple fallback during setup (`7cb390a`). Prior notes report a five-synthetic-clip comparison of 2.2% vs 4.5% WER; that benchmark was not rerun in this review.
- [x] Add image drag-and-drop, project-local attachment copies, previews, and Codex image arguments; fuller read-aloud, more concise Alex instructions, and default 1.25x speech (`7bd07e7`). Implementation inspected; interactive behavior remains to verify.
- [x] Implement Enter to send and Shift+Enter for multiline input, with a composer growing up to six lines (`7d0f596`). Implementation inspected; interactive behavior remains to verify.
- [x] Publish native macOS/Linux/Windows CI with locked tests, release builds, and development artifacts. Bundling now propagates Swift failures instead of copying a potentially stale helper.
- [x] Verify current checkout locally: `cargo test --offline --locked --all-targets` passed on macOS (32 passed, 1 ignored). No new release build or app restart was performed during this review.
- [x] Verify Linux hosted tests, release build, archive, and artifact upload: ubuntu-24.04 job succeeded in [CI run 44841363709](https://github.com/ncserny/yesharness/actions/runs/44841363709) for `7d0f596`. This does not verify X11/Wayland interactive behavior.
- [x] Verify hosted macOS Rust tests and release build in the same run. Full Swift/app bundling was still in progress when checked.

## Remaining work

- [ ] Fix the Windows attachment-path test failure at src/ui.rs:2774. Hosted Windows compiled and ran tests: 25 passed, 1 failed, 6 ignored; `windows_custom_command_replies` passed. The failing assertion compares a string prefix containing `.yesharness/attachments` against a returned Windows path with backslashes; use a platform-aware path comparison and rerun. Windows release build/artifact steps were skipped after this failure. No fix made during this review.
- [ ] Check completion of the macOS bundle/signature/artifact steps in run 44841363709. This session previously hit SwiftPM cache/sandbox restrictions; no new local bundle attempt was made.
- [ ] Complete interactive macOS checks: layout, global shortcut, microphone/listening modes, read-aloud/Quiet, Alex display/persistence, image attachments, and Enter/Shift+Enter. Prior preview access was restricted; current app version/relaunch state was not independently verified.
- [ ] Verify live Claude/Codex authentication, image understanding, streaming, handoffs, and session resume end to end; mock tests alone do not establish these.
- [ ] Compare Apple vs Parakeet using the user's own microphone/voice; evaluate custom vocabulary for agent/project names.
- [ ] Validate Linux X11/Wayland and Windows desktop behavior, folder dialogs, CLI discovery, npm shims/paths with spaces, and child-process termination. Implement Windows equivalents for the five ignored Unix process fixtures; the watchdog test is Unix-only.
- [ ] Add Linux/Windows installers and release signing/notarization as needed. CI development artifacts are not supported releases; Linux/Windows voice and notifications remain absent.

## Synchronization limits

`git fetch origin` was attempted but denied writing `.git/FETCH_HEAD` (`Operation not permitted`). The GitHub connector independently confirmed that the existing checkout already matches live main at `7d0f596`; no merge, reset, stash, or checkout update was needed. This notes revision is local and uncommitted. Do not describe the fetch as successful or the repository as unpublished.

See docs/platform-compatibility.md for platform details and historical validation evidence.
