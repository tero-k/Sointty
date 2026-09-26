# Sointty

A terminal music player with a bit-perfect output path. What the decoder
produces is what the device gets: same samples, same rate, same channels.
No resampling, no volume processing, no EQ, no mixer in between.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![MSRV: 1.98.1](https://img.shields.io/badge/rustc-1.98.1%2B-orange.svg)](https://blog.rust-lang.org/releases/)
[![Platform: Linux | Windows | macOS](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey.svg)](#platforms)

v1 feature complete (see [`docs/PLAN.md`](docs/PLAN.md)). Verified on Windows
with an iFi USB DAC; Linux and macOS backends are implemented and unit tested,
hardware testing pending.

## Behavior

- If the output device can't take the stream exactly as decoded, playback fails
  with a typed error instead of a silent conversion. An explicit opt-in
  (`--allow-float-to-int`, or `c` in the TUI) enables F32→integer conversion
  for float-decoding formats on integer-only devices; converted output is
  labeled `NOT bit-perfect` in the UI.
- The negotiated format (rate, channels, packing, valid bits) is shown on
  screen as established with the device, not as requested.
- Tracks with an identical stream specification play gaplessly through one
  open device.
- File reads go through a bounded read-ahead worker, so slow or mounted drives
  stall gracefully and Stop/Skip/Seek stay responsive. Stalls and underruns
  are reported, not hidden.
- Local files only; remote URLs are rejected.

## Quick start

```sh
# Linux: ALSA headers required; libopus for the default Opus support
sudo apt install libasound2-dev libopus-dev
cargo build --release

# Windows / macOS: no system audio dependencies
cargo build --release
```

```sh
sointty --list-devices                 # list output endpoints
sointty /path/to/album/                # play files or folders
sointty playlist.m3u                   # playlists expand into tracks
```

With no arguments, sointty opens a filesystem browser. Press `d` to pick the
output device; the choice is saved.

### Keys

| Key | Action |
|---|---|
| `space` | play / pause |
| `s` / `n` | stop / next track |
| `←` / `→` | seek back / forward 10 s |
| `b` | file browser (`Enter` plays, `Backspace` goes up) |
| `d` | choose output device |
| `c` | allow F32→integer conversion (off by default) |
| `q` / `Esc` | quit (`Esc` closes an open pane first) |

Settings live in `<config dir>/sointty/config.toml`
(`%APPDATA%\sointty\sointty\config\config.toml` on Windows,
`~/.config/sointty/config.toml` on Linux). Command-line flags override the file.

## Fidelity rules

- PCM passes through unchanged. The only permitted transforms are lossless:
  channel interleaving, byte-order packing, and widening into a larger
  container with zeroed low bits (e.g. 24-bit samples in a 32-bit slot).
- Rate, channel mask, packing, and valid bits are confirmed with the device at
  initialization; mismatches raise `UnsupportedFormat`.
- F32→integer conversion, when enabled, is round-to-nearest-even with clipping
  and no dither, and runs on the decoder thread, not the render thread.
- DSD is a separate stream type: native DSD or DoP on ALSA, never converted to
  PCM.
- No `cpal`, no ALSA `plug`/`plughw`/`default`, no WASAPI shared mode, no
  Core Audio system mixer or aggregate devices.

## Formats

| Source | Formats | Notes |
|---|---|---|
| Symphonia (built in) | FLAC, ALAC, WAV/AIFF, MP3, AAC-LC, Vorbis | always available |
| Ogg Opus | `.opus` | default on Linux, `--features opus` elsewhere |
| FFmpeg (opt-in) | APE, WavPack, TAK, Musepack, DSF/DFF | `--features ffmpeg-lgpl`, needs system FFmpeg 9 |

Playlists: M3U/M3U8 (read+write), PLS, XSPF, single-image CUE with gapless
adjacent ranges.

## Platforms

| OS | Backend | Status |
|---|---|---|
| Linux | ALSA `hw:` direct, native DSD and DoP | implemented, awaiting hardware test |
| Windows | WASAPI exclusive, push-mode fallback for drivers that force whole-buffer periods | tested on an iFi DAC |
| macOS | Core Audio HAL hog mode | implemented, awaiting hardware test |

## Building

MSRV is Rust 1.98.1 (edition 2024). Optional features on the app crate:

- `library-index`: opt-in SQLite index of the music collection. Off by default;
  filesystem browsing never touches a database.
- `ffmpeg-lgpl`: extra formats via a system FFmpeg 9 (LGPL builds only). Set
  `FFMPEG_DIR` (and `LIBCLANG_PATH` for bindgen) if discovery fails.

```sh
cargo build --release --features library-index
cargo build --release --features ffmpeg-lgpl
cargo test --workspace
```

## Repository layout

```
crates/
  core              shared types and exact-packing rules
  source            read-ahead file access with stall recovery
  decode            Symphonia, Opus, and FFmpeg decoders; tag reading
  playlist          M3U/M3U8/PLS/XSPF/CUE
  library           optional SQLite index
  output-alsa       Linux ALSA backend
  output-wasapi     Windows WASAPI backend
  output-coreaudio  macOS Core Audio backend
  tui               terminal UI and file browser
  app               player engine and command line
docs/PLAN.md        design notes, milestones, acceptance criteria
```

## Contributing

Issues and pull requests are welcome. Any change to the audio signal must be
explicit, opt-in, and labeled in the UI.

## License

MIT or Apache-2.0, your choice: [LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE).

The optional `ffmpeg-lgpl` feature dynamically links FFmpeg (LGPL-2.1-or-later);
only distribute binaries built against LGPL FFmpeg builds.
