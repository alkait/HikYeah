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
- Releases bundle ffmpeg pinned to a major branch (BtbN `n8.1-latest`): patches flow in automatically, majors bump only when the user asks. macOS builds against Homebrew's `ffmpeg@8` formula (the plain `ffmpeg` formula is already 9.x). The app links the FFmpeg *libraries* (libavcodec/libavformat/libavutil/libswscale/libavdevice via pkg-config at build time); release archives must ship matching shared libraries next to the binary.
- Every decoded pixel is cost: measured on a hybrid laptop, the old child-process pipe (raw frames through stdout, a conversion, a re-upload) was 4–5× the CPU of in-process decode, and a hardware decode *with a download* costs more CPU than software at small sizes. The zero-copy path (`gpu.rs`: VAAPI surfaces exported as DMA-BUFs, imported as Vulkan images on the Intel iGPU) is the model for other platforms: D3D11 shared textures on Windows, CVPixelBuffer/IOSurface on macOS. Never add a copy to a path that has none.
- No tests for now.

## Building on this machine

There is no Rust toolchain and no FFmpeg headers on the dev laptop (it runs the installed release from `~/.local/share/hikyeah`; releases are built by CI). Build in a container that mirrors the Linux job in `.github/workflows/release.yml`:

- `docker.io/library/rust:latest`, `apt-get install libclang-dev pkg-config`, the BtbN `ffmpeg-n8.1-latest-linux64-gpl-shared-8.1` tarball as `FFMPEG_DIR`, and `RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN -C link-arg=-Wl,--disable-new-dtags'`.
- The cache lives in `~/.cache/hikyeah-build` (`cargo`, `target`, `ffshared`, the tarball and `build.sh`, ~2 GB); delete the directory to reset. Build with
  `podman run --rm --security-opt label=disable -v <repo>:/src -v ~/.cache/hikyeah-build:/work docker.io/library/rust:latest bash /work/build.sh`
  — SELinux refuses plain bind mounts. A warm rebuild takes seconds; a cold one ~3 min plus image pull. Extract the tarball with `--no-same-owner`, or its files end up owned by a podman subuid and only `podman unshare rm -rf` can remove them.
- Run `cargo fmt` and `cargo clippy --release --all-targets -- -D warnings` inside the container too (mount the repo read-write so fmt can write).
- To run the result natively, copy the binary into a directory with symlinks to the installed `libav*.so*` and `ffmpeg` (the `$ORIGIN` rpath resolves them). The user tests from there; a second instance is refused by the lock, so make sure none is running.
- Hybrid-laptop facts worth knowing: the Vulkan loader enumerates the NVIDIA driver at startup, which wakes the sleeping dGPU (`/sys/bus/pci/devices/0000:01:00.0/power/runtime_status`) and makes exit block ~1.5 s while the kernel wakes it again to release the handles. Measure close/launch timings against that file.

## Testing on the Mac

An M2 Pro MacBook (macOS 26, `ssh aymanalkait@192.168.1.13`, password from the user) has rustup, Xcode and Homebrew (`ffmpeg@8` force-linked, `pkg-config`). `rsync` the tree to `~/HikYeah` (exclude `target`, `.git`) and `cargo build --release` there; a warm build takes under a minute. Package it exactly like the workflow's "Bundle (macOS)" step (an `HikYeah.app` with the Info.plist, `ffmpeg` next to the executable, ad-hoc `codesign`, `ditto` zip + `.sha256`), then exercise the installer against the local zip: `HIKYEAH_ASSET_URL=file:///path/to/hikyeah-vX-macos-arm64.app.zip bash install.sh` puts it in `/Applications/HikYeah.app`, which is also what `update::installed()` checks. Launch with `open --env HIK_DEBUG=1 --stdout ~/hikyeah.log --stderr ~/hikyeah.log /Applications/HikYeah.app` so the window lands in the console session and the log is readable over SSH. Same-subnet cameras failing with "No route to host" while `ping` works means the Local Network privacy prompt is pending; only a click on the Mac clears it. Keep the Mac awake with `nohup caffeinate -dimsu &` for the session, and the config is HikViewer's own `~/Library/Application Support/hikviewer/`.
