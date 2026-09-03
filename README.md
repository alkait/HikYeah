# HikYeah

Cross-platform (Linux/Windows/macOS) port of
[HikViewer](https://github.com/alkait/HikViewer): a live grid of your
Hikvision cameras, any of them one double-click away from a full-window
main-stream view with digital zoom. Recorded playback from the NVR is not
ported yet.

Pipeline: `ffmpeg` (RTSP → decode, NVDEC when available → yuv4mpegpipe on
stdout) → I420 planes → three R8 wgpu textures → YUV→RGB in a WGSL shader
during egui's render pass. Only the latest frame is ever shown, so latency
can't accumulate; ffmpeg is respawned forever on exit or stall, like the Mac
app. `HIK_SWDEC=1` forces software decode.

## Install (Linux)

One command installs (and later updates — just re-run it). It fetches the
latest release, verifies its SHA-256, installs to `~/.local/share/hikyeah`
(with a bundled ffmpeg), symlinks `~/.local/bin/hikyeah`, and adds a desktop
entry:

```sh
/bin/bash -c "$(curl -fsSL https://github.com/alkait/HikYeah/releases/latest/download/install.sh)"
```

To uninstall (asks before touching your camera config or prefs):

```sh
/bin/bash -c "$(curl -fsSL https://github.com/alkait/HikYeah/releases/latest/download/uninstall.sh)"
```

Prefer manual? Grab your platform's archive from
[Releases](https://github.com/alkait/HikYeah/releases), extract, run —
they're self-contained.

## Run

```sh
cargo build --release
./target/release/hikyeah rtsp://user:pass@host:554/Streaming/Channels/102
./target/release/hikyeah --test    # ffmpeg synthetic test pattern, no camera
./target/release/hikyeah           # first camera from config (below)
```

Uses the `ffmpeg` sitting next to the executable if there is one (release
archives bundle a static build), else `ffmpeg` from PATH.

## First run

With no cameras configured the Settings window opens by itself: press **+**,
enter the camera's host, username, password and RTSP port, and optionally
let **Detect** read its name and codec from the camera. **Save** starts the
grid. Mixed fleets with different credentials are fine.

The config is one JSON file, the same format as the Mac app's File > Export,
so a setup moves between machines as a plain file copy (it contains the
passwords in clear — treat it as a secret):

| OS | Path |
|---|---|
| Linux | `~/.config/hikviewer/config.json` |
| macOS | `~/Library/Application Support/hikviewer/config.json` (shared with HikViewer) |
| Windows | `%APPDATA%\hikviewer\config.json` |

## Everyday use

| Action | Effect |
|---|---|
| Double-click a tile | focus it full-window (switches to the camera's main stream) |
| `Esc` | back to the grid |
| Arrow keys | move a red selection cursor between tiles; `Return` focuses it |
| Long-press + drag a tile | reorder the grid (order is saved); `Esc` cancels |
| `S` | snapshot of the focused camera (full resolution, from the camera itself) |
| `R` | start / stop recording a clip of the focused camera |
| `I` | nerd stats panel (focused camera, or the selected grid tile) |
| `?` | keyboard shortcut help |
| `F11` | toggle full screen |
| `Ctrl-,` | Settings |

**Digital zoom** (focused view): mouse wheel or pinch zooms toward the pointer
(1×–8×), double-click for a quick 2× at that spot (again to restore), drag to
pan. A `2.4× ✕` badge top-right shows the level — click it to reset — and
`Esc` zooms out first before leaving the view.

**Snapshots & clips** land on your Desktop (home if there is none) as
`Camera 2026-07-20 14.32.05.jpg/.mp4`, and a dialog then lets you type over
that name — `Return` keeps it, `Esc` or Discard deletes the capture. Nothing
is ever overwritten. `S` fires a shutter flash the instant you press it. `R` records the
main stream with no re-encode (video only, fragmented MP4 so even a hard quit
leaves a playable file); a red `● REC` badge counts up, and the clip also
stops when you leave the camera.

**Nerd stats** (`I`): a draggable panel of live diagnostics — stream and
decode device, measured fps, arrival jitter (σ + worst gap), stalls and
reconnects, the smoothing buffer's headroom, re-anchors and late frames,
app + ffmpeg CPU, and Wi-Fi signal. Hover a row's name for what the number
means; values turn amber/red past trouble thresholds; `⧉` copies a plain-text
snapshot. A stream that goes silent for 12 s is killed and reconnected (that
is a "stall"). Bitrate and GOP are not shown: ffmpeg hands the app decoded
frames, so the compressed stream never passes through.

**Settings** also holds "Always start in full screen", "Remember where I left
off" (the grid or the camera you quit from), "Smooth live video" (~0.2 s
buffer absorbing Wi-Fi jitter; untick for minimum latency), the decode device
(CPU, NVDEC, Quick Sync, VAAPI, … — only those that pass a startup probe are
listed) and the render adapter.

## Not ported yet

NVR playback (calendar, timeline, motion), bookmarks, intrusion review and
supplementary panes.
