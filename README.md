# HikYeah

Cross-platform (Linux/Windows/macOS) port of
[HikViewer](https://github.com/alkait/HikViewer) — currently a **walking
skeleton**: one window, one live camera tile.

Pipeline: `ffmpeg` (RTSP → decode, NVDEC when available → yuv4mpegpipe on
stdout) → I420 planes → three R8 wgpu textures → YUV→RGB in a WGSL shader
during egui's render pass. Only the latest frame is ever shown, so latency
can't accumulate; ffmpeg is respawned forever on exit or stall, like the Mac
app. `HIK_SWDEC=1` forces software decode.

## Run

```sh
cargo build --release
./target/release/hikyeah rtsp://user:pass@host:554/Streaming/Channels/102
./target/release/hikyeah --test    # ffmpeg synthetic test pattern, no camera
./target/release/hikyeah           # first camera from config (below)
```

Requires `ffmpeg` on PATH. With no arguments it reads
`~/.config/hikviewer/config.json` — the same JSON the Mac app's
File > Export produces, so an exported config can be dropped there unchanged.

## Next steps (per the porting plan)

- Camera grid, focus view, then the rest of the UI in dependency order
- Port `NVRClient` / `PlaybackStream` / ISAPI from the Swift reference
