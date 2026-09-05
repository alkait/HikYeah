# HikYeah

Cross-platform (Linux/macOS/Windows) rewrite of the macOS HikViewer app (`../HikViewer`, Swift — reference for features and behavior). Rust + egui/wgpu; ffmpeg child processes decode RTSP to y4m pipes.

## Rules

- Run `cargo fmt` and `cargo clippy` before every commit; keep the tree warning-free so new warnings stand out.
- Minimal, clean, maintainable code. No speculative abstractions, no empty scaffolding — split modules only when a feature makes them grow.
- Flat `src/`, one module per concern: `main.rs` is app state + the frame loop; views live in `grid.rs`, `focused.rs`, `settings.rs`, `overlay.rs`, `timeline.rs` (playback bar); playback is `nvr.rs` (ISAPI) + `rtsp.rs` (native RTSP → ffmpeg stdin) + `playback.rs` (transport). Single crate; no workspace.
- Port logic from the Mac sources as written — the quirks (fake-`Z` NVR-local timestamps, 2000-entry log cap, `searchResultPostion`, no-qop RTSP digest, pause = kill the pipe and keep the frame) are intentional, measured findings; don't re-derive or "fix" them.
- Threads + channels + `Mutex` — no async runtime.
- Prefer serde enums over stringly-typed state (e.g. session location, pref ids) when touching those files.
- Don't add global statics; pass state through `Shared` or a settings struct.
- Fail fast on bugs (panic on violated invariants — don't catch). For environmental failures (camera offline, network, ffmpeg exit), degrade visibly and retry: show status on the affected tile, never take down the app, never swallow an error without a status or log signal.
- Comments explain constraints and Mac-app ports (why), not what the code does.
- Guard platform-specific code with `cfg(target_os)`; only Linux is exercised today, so double-check macOS/Windows paths compile.
- Releases bundle a static ffmpeg pinned to a major branch (BtbN `n8.1-latest`): patches flow in automatically, majors bump only when the user asks. The macOS source is knowingly unpinned until there are Mac users.
- No tests for now.
