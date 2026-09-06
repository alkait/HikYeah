# HikYeah

Cross-platform (Linux/macOS/Windows) rewrite of the macOS HikViewer app (`../HikViewer`, Swift — reference for features and behavior). Rust + egui/wgpu; the FFmpeg libraries (ffmpeg-next) decode in-process — libavformat for RTSP, libavcodec with hardware decoders where chosen — and frames go straight to wgpu textures. The ffmpeg binary is only spawned for captures (stream-copy clips, playback snapshots) and the decoder probe.

## Rules

- **Smoothness and performance come first, in every decision.** Video must never stutter, the UI must never jump or resize under the user, and nothing may burn CPU, GPU or wake a sleeping GPU without a measured reason. Measure before and after (CPU per thread, iGPU busy/clock, package temperature, redraw rate) and keep the numbers in the commit message. A feature that costs smoothness or heat is not done.
- Run `cargo fmt` and `cargo clippy` before every commit; keep the tree warning-free so new warnings stand out.
- Minimal, clean, maintainable code. No speculative abstractions, no empty scaffolding — split modules only when a feature makes them grow.
- Flat `src/`, one module per concern: `main.rs` is app state + the frame loop; views live in `grid.rs`, `focused.rs`, `settings.rs`, `overlay.rs`, `timeline.rs` (playback bar); video is `stream.rs` (pacing, publish) + `decode.rs` (libav) + `gpu.rs` (device, DMA-BUF import) + `render.rs`; playback is `nvr.rs` (ISAPI) + `rtsp.rs` (native RTSP → the in-process decoder) + `playback.rs` (transport). Single crate; no workspace.
- Port logic from the Mac sources as written — the quirks (fake-`Z` NVR-local timestamps, 2000-entry log cap, `searchResultPostion`, no-qop RTSP digest, pause = kill the pipe and keep the frame) are intentional, measured findings; don't re-derive or "fix" them.
- Threads + channels + `Mutex` — no async runtime.
- Prefer serde enums over stringly-typed state (e.g. session location, pref ids) when touching those files.
- Don't add global statics; pass state through `Shared` or a settings struct.
- Fail fast on bugs (panic on violated invariants — don't catch). For environmental failures (camera offline, network, ffmpeg exit), degrade visibly and retry: show status on the affected tile, never take down the app, never swallow an error without a status or log signal.
- Comments explain constraints and Mac-app ports (why), not what the code does.
- Guard platform-specific code with `cfg(target_os)`; only Linux is exercised today, so double-check macOS/Windows paths compile.
- Releases bundle ffmpeg pinned to a major branch (BtbN `n8.1-latest`): patches flow in automatically, majors bump only when the user asks. The macOS source is knowingly unpinned until there are Mac users. The app links the FFmpeg *libraries* (libavcodec/libavformat/libavutil/libswscale/libavdevice via pkg-config at build time); release archives must ship matching shared libraries next to the binary.
- Every decoded pixel is cost: measured on a hybrid laptop, the old child-process pipe (raw frames through stdout, a conversion, a re-upload) was 4–5× the CPU of in-process decode, and a hardware decode *with a download* costs more CPU than software at small sizes. The zero-copy path (`gpu.rs`: VAAPI surfaces exported as DMA-BUFs, imported as Vulkan images on the Intel iGPU) is the model for other platforms: D3D11 shared textures on Windows, CVPixelBuffer/IOSurface on macOS. Never add a copy to a path that has none.
- No tests for now.
