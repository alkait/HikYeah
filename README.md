# HikYeah

A fast, simple viewer for Hikvision cameras and NVRs on Linux, macOS and
Windows.

Hikvision's own software is slow, unintuitive and a chore to use every day.
HikYeah exists so that looking at your cameras is instant and obvious: open
it and the grid is there, double-click a camera to see it big, press one key
to scrub through what the NVR recorded. No plugins, no browser, no menus
three levels deep. Under the hood it is light too, so it can stay open all
day on a laptop.

## Highlights

- **Live grid** of all your cameras, reorderable by drag, with a selection
  cursor for keyboard-only use.
- **Focused view**: double-click a tile for a full-window main-stream view
  with digital zoom (wheel or pinch, up to 8×) and pan.
- **Recorded playback** straight from the NVR: a calendar of days with
  footage, a zoomable 24-hour timeline, seek by click, 1× / 2× / 4× speed,
  and audio where the NVR recorded it.
- **Snapshots and clips** at full resolution, live or from the playback
  position, with no re-encoding.
- **Audio** for live streams that carry it.
- **Hardware decoding** where it helps: VAAPI, NVDEC, Quick Sync,
  VideoToolbox or Direct3D 11, chosen in Settings after a startup probe.
- **Always current**: only the latest frame is ever shown, so latency never
  accumulates, and a dropped stream reconnects on its own.
- **Diagnostics panel** with per-stream frame rate, jitter, stalls, decoder
  and CPU cost, for when something looks off.
- **Self-updating**: one command installs, and the app offers new releases
  from Settings.

## Install

**Linux (x86_64) and macOS (Apple Silicon):** one command installs the
latest release, verifies its checksum, and can be re-run at any time to
update.

```sh
/bin/bash -c "$(curl -fsSL https://github.com/alkait/HikYeah/releases/latest/download/install.sh)"
```

On Linux this puts the app, with FFmpeg bundled, in `~/.local/share/hikyeah`,
links `~/.local/bin/hikyeah`, and adds a desktop entry. On macOS it puts
`HikYeah.app` in `/Applications`; the app uses Homebrew's FFmpeg 8 libraries,
so run `brew install ffmpeg@8` first.

**Windows (x86_64):** download the zip from
[Releases](https://github.com/alkait/HikYeah/releases), extract it anywhere,
and run `hikyeah.exe`. Everything it needs is in the folder.

To uninstall on Linux:

```sh
/bin/bash -c "$(curl -fsSL https://github.com/alkait/HikYeah/releases/latest/download/uninstall.sh)"
```

On macOS delete `/Applications/HikYeah.app`; on Windows delete the folder.

## First run

With no cameras configured, Settings opens by itself. Press **+**, enter a
camera's host, user, password and RTSP port, and let **Detect** fill in its
name and codec. **Save** starts the grid. To enable recorded playback, fill in
the NVR row with its host, user and password; the app works out which channel
each camera is on.

Press **?** in the app for the full list of keyboard shortcuts.

Your camera list is one JSON file you can copy between machines. It holds
the passwords in clear, so treat it as a secret.

| OS | Path |
|---|---|
| Linux | `~/.config/hikviewer/config.json` |
| macOS | `~/Library/Application Support/hikviewer/config.json` |
| Windows | `%APPDATA%\hikviewer\config.json` |

## Build from source

Rust stable, the FFmpeg development libraries (any of 6 through 9) and
libclang for the bindings. Debian/Ubuntu: `apt install libavcodec-dev
libavformat-dev libavutil-dev libswscale-dev libavdevice-dev pkg-config
libclang-dev`. Arch: `pacman -S ffmpeg clang`. macOS: `brew install ffmpeg@8
pkg-config` and `PKG_CONFIG_PATH=$(brew --prefix ffmpeg@8)/lib/pkgconfig`.

```sh
cargo build --release
./target/release/hikyeah                       # cameras from the config
./target/release/hikyeah rtsp://user:pass@host:554/Streaming/Channels/102
./target/release/hikyeah --test                # synthetic test pattern
```

Snapshots, clips and the decoder probe call the `ffmpeg` binary: the one
next to the executable if present (releases bundle it), otherwise the one
on PATH.

## License

[MIT](LICENSE). The release archives bundle FFmpeg, which is GPL; its
notice files sit next to the binary.
