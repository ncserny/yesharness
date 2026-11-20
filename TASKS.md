# Tasks

Status reviewed 2026-11-21 from README.md and docs/platform-compatibility.md.

- [x] Document platform compatibility and interface refresh; notes report macOS builds, bundling, and 17 tests passing on 2026-11-20.
- [ ] Complete interactive macOS layout, global shortcut, microphone, and read-aloud checks. Previous preview was blocked by UI-service access and denied Computer Use access.
- [ ] Verify live Claude/Codex authentication and end-to-end agent flows; mock tests do not establish these.
- [ ] Validate Linux and Windows builds and runtime behavior before claiming support; add packaging and CI.
- [ ] Locate the Git checkout/history to establish recent commit chronology. This project folder has no .git repository.
- [x] Prepare Git exclusions for build outputs, dependencies, environment secrets, private-key files, and machine-specific Claude configuration; retain Cargo.lock for this application. Reviewed project file inventory and scanned text for obvious credentials.
- [ ] Initialize Git on `main` and create the initial commit. Blocked on 2026-11-21: sandbox denied `git init -b main` with `.git: Operation not permitted`. Git name/email are configured; no files staged or committed. No remote created and nothing pushed.
- [ ] Publish to corrected, user-authorized `https://github.com/ncserny/yesharness.git`. Verified 2026-11-21 through the GitHub connector: repository accessible, push permission granted, no branches present. Connector publication was blocked: `create_file` requires approval, but this session’s approval policy is `never`. No remote file or commit was created. Local Git initialization remains denied, terminal DNS fails, and CLI authentication reports invalid. Earlier `njourney` destination was a transcription error.

See docs/platform-compatibility.md for the detailed validation checklist. No new build or test run was performed during this status review.

- [x] Implement main agent display name Alex (2026-11-21): separate persisted `display_name` from the unchanged `main` handle and agent id; migrate existing state on startup, show names in team/chat UI, and include the name in agent prompts. Manage Team can edit the display name independently. Changed src/model.rs, src/ui.rs, and src/orchestrator.rs.
- [x] Verify rename: `cargo test --offline` passed (29 passed, 1 ignored); migration test checks serialization and unchanged routing/session data. One timing-sensitive parallel-handoff test failed during a concurrent build, then passed on the full rerun. `CARGO_NET_OFFLINE=true bash scripts/bundle.sh` built dist/YesHarness.app, with existing Swift Sendable warnings.
- [ ] Relaunch the rebuilt dist/YesHarness.app to apply and persist Alex in the actual app state; the currently running app was not restarted. Interactive display verification remains pending.
