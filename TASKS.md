# Tasks

Status reviewed 2026-11-21 from README.md and docs/platform-compatibility.md.

- [x] Document platform compatibility and interface refresh; notes report macOS builds, bundling, and 17 tests passing on 2026-11-20.
- [ ] Complete interactive macOS layout, global shortcut, microphone, and read-aloud checks. Previous preview was blocked by UI-service access and denied Computer Use access.
- [ ] Verify live Claude/Codex authentication and end-to-end agent flows; mock tests do not establish these.
- [ ] Validate Linux and Windows builds and runtime behavior before claiming support; add packaging and CI.
- [x] Git history now lives in this folder and on GitHub (ncserny/yesharness).
- [x] Prepare Git exclusions for build outputs, dependencies, environment secrets, private-key files, and machine-specific Claude configuration; retain Cargo.lock for this application. Reviewed project file inventory and scanned text for obvious credentials.
- [x] Initialize Git on `main` and create the initial commit. Done 2026-11-21 from the Claude Code session (the agents' sandbox can't run git). Blocked on 2026-11-21: sandbox denied `git init -b main` with `.git: Operation not permitted`. Git name/email are configured; no files staged or committed. No remote created and nothing pushed.
- [x] Publish to `https://github.com/ncserny/yesharness` (public). Done 2026-11-21; every change is committed and pushed there, with secrets and machine-specific files excluded. Verified 2026-11-21 through the GitHub connector: repository accessible, push permission granted, no branches present. Connector publication was blocked: `create_file` requires approval, but this session’s approval policy is `never`. No remote file or commit was created. Local Git initialization remains denied, terminal DNS fails, and CLI authentication reports invalid. Earlier `njourney` destination was a transcription error.

See docs/platform-compatibility.md for the detailed validation checklist. No new build or test run was performed during this status review.

- [x] Implement main agent display name Alex (2026-11-21): separate persisted `display_name` from the unchanged `main` handle and agent id; migrate existing state on startup, show names in team/chat UI, and include the name in agent prompts. Manage Team can edit the display name independently. Changed src/model.rs, src/ui.rs, and src/orchestrator.rs.
- [x] Verify rename: `cargo test --offline` passed (29 passed, 1 ignored); migration test checks serialization and unchanged routing/session data. One timing-sensitive parallel-handoff test failed during a concurrent build, then passed on the full rerun. `CARGO_NET_OFFLINE=true bash scripts/bundle.sh` built dist/YesHarness.app, with existing Swift Sendable warnings.
- [ ] Relaunch the rebuilt dist/YesHarness.app to apply and persist Alex in the actual app state; the currently running app was not restarted. Interactive display verification remains pending.

- [x] Prepare cross-platform CI (2026-11-21): added .github/workflows/ci.yml with native macOS/Linux/Windows locked tests, release builds, and development artifacts; documented Linux dependencies and platform test gaps. Five Unix process fixtures explicitly ignore Windows; added a native Windows command smoke test (not executed locally).
- [x] Local CI preparation checks: Rust 1.86.0, 31 tests passed / 1 ignored, locked macOS release binary built, workflow YAML parsed, and bundle shell syntax passed. Fixed scripts/bundle.sh to propagate Swift failures and use Cargo.lock.
- [ ] Execute hosted CI after publication; no Linux/Windows pass claimed. Only macOS target available locally. Full current macOS bundle blocked by SwiftPM sandbox_apply denial even with module caches in target/. Interactive platform tests, Windows equivalents of Unix process fixtures, installers, and release signing remain pending. See docs/platform-compatibility.md.
- [x] Add Parakeet Ultra as an optional speech engine (Settings → Voice → Recognition) via FluidAudio 0.18.4; helper is now a Swift package. Compared on 5 synthetic clips: WER 2.2% vs Apple 4.5%, ~0.5 s per sentence for both.
- [ ] Compare Apple vs Parakeet on the user's own voice and microphone; consider Parakeet custom vocabulary (CTC word spotting) for agent and project names.
