# Sointty

**Terminal hi-fi music player with a bit-perfect fidelity contract.** Decoder-produced
samples travel unchanged through the application into an OS-native direct/exclusive
endpoint. No resampling, no channel remixing, no float/int conversion, no software
volume, no ReplayGain/EQ/dither, and no shared-mode mixers anywhere in the path.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![MSRV: 1.98.1](https://img.shields.io/badge/rustc-1.98.1%2B-orange.svg)](https://blog.rust-lang.org/releases/)
[![Platform: Linux | Windows | macOS](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey.svg)](#platforms)

Status: **v1 code complete** (milestones M1–M6, see [`docs/PLAN.md`](docs/PLAN.md)).
Verified on Windows hardware (iFi USB DAC); Linux and macOS acceptance tracked in the plan.

## Features

- **Bit-perfect playback**: exact rate, channel mask, packing, and valid bits confirmed
  with the device at initialization; unsupported combinations fail with a typed error
  instead of silently converting.
- **Gapless transitions**: tracks with an identical stream spec play seamlessly on one
  open device; preopened next track, frame-count boundary events.
- **Direct OS backends only**: ALSA `hw:` (Linux), WASAPI exclusive (Windows),
  Core Audio HAL hog mode (macOS).
- **Terminal UI**: filesystem browser, queue, seek, live tags, device picker, and a
  persistent on/off label for bit-perfect fidelity.
- **Playlists**: M3U/M3U8 (read+write), PLS, XSPF, single-image CUE with gapless
  adjacent ranges. Local files only; remote URLs are rejected.
- **Stall resilience**: bounded read-ahead worker keeps Stop/Skip/Seek responsive on
  slow or mounted drives; underruns are surfaced, never hidden.

## Quick start

```sh
# Linux: ALSA headers required; libopus for the default Opus support
sudo apt install libasound2-dev libopus-dev
cargo build --release

# Windows / macOS: no system audio dependencies
cargo build --release
```

```sh
sointty --list-devices                 # find your endpoint
sointty /path/to/album/                # browse or play files/directories
sointty playlist.m3u                   # playlists expand into tracks
```

With no arguments, sointty opens the filesystem browser. Press `d` in the TUI to pick
the output device; the choice is saved.

### Keys

| Key | Action |
|---|---|
| `space` | play / pause |
| `s` / `n` | stop / next track |
| `←` / `→` | seek −/+10 s |
| `b` | toggle filesystem browser (`Enter` enqueues and plays, `Backspace` goes up) |
| `d` | pick the output device; the choice is saved to the config file |
| `c` | toggle F32→integer compatibility for f32 decoders on integer-only devices; the choice is saved |
| `q` / `Esc` | quit (`Esc` closes an open pane first) |

Settings persist in `<config dir>/sointty/config.toml`
(e.g. `%APPDATA%\sointty\sointty\config\config.toml` on Windows,
`~/.config/sointty/config.toml` on Linux): `device`, `period_frames`, `buffer_frames`,
`float_to_int`. CLI flags override the file for that run.

## Fidelity contract

- PCM passes through at the decoder's sample values, rate, and channel mapping.
  Lossless interleaving, endian packing, and exact integer widening
  (e.g. S24 in a 32-bit slot with zero low bits) are the only permitted transforms.
- Exact rate, channels/channel mask, packing, and valid bits are confirmed with the
  device at initialization: the established configuration, not a requested nearest.
- Unsupported combinations fail typed (`UnsupportedFormat`) instead of converting.
  MP3, AAC, Vorbis and Opus decode to f32; on an integer-only endpoint
  (including the tested iFi USB DAC), they are rejected, not narrowed to
  integers, unless the user explicitly opts in to F32→integer conversion
  (`--allow-float-to-int`, `float_to_int = true` in the config, or `c` in the
  TUI; all persisted). Conversion is round-to-nearest-even with clipping, no
  dither, and is never silent: the TUI labels converted output `NOT bit-perfect`
  while playing and after the track ends. FLAC and integer PCM are negotiated
  from their actual decoded sample format, not assumed to be 16-bit.
- No `cpal`, no ALSA `plug`/`plughw`/`default`, no WASAPI shared mode, no Core Audio
  system mixer or aggregate devices.
- DSD is a first-class `DsdSpec`, never disguised as PCM: native DSD or DoP
  (per-frame 0x05/0xFA marker alternation per the DoP 1.1 spec) on ALSA.

## Formats

| Path | Formats | Notes |
|---|---|---|
| Symphonia (default) | FLAC, ALAC, WAV/AIFF PCM, MP3, AAC-LC, Vorbis | always built |
| Ogg Opus (libopus) | `.opus` | RFC 7845 pre-skip/EOS trim, nonzero header gain rejected; default on Linux, feature `opus` elsewhere |
| FFmpeg LGPL (opt-in) | APE, WavPack, TAK, Musepack, DSF/DFF (DSD) | feature `ffmpeg-lgpl`; demuxer+decoder availability probed at runtime, typed failure when absent |

## Platforms

| OS | Output backend | State |
|---|---|---|
| Linux | ALSA `hw:` direct, native DSD + DoP negotiation | code complete; hardware acceptance pending |
| Windows | WASAPI exclusive, event-driven with timer-driven push fallback for drivers that force whole-buffer periods (S_OK-only format negotiation, MMCSS) | verified on hardware (iFi USB endpoint) |
| macOS | Core Audio HAL hog mode, nominal-rate + physical-format set/readback | cross-checked; live-device test pending |

## Building

MSRV: Rust 1.98.1 (edition 2024).

Feature flags (on the `sointty` app crate):

- `library-index`: opt-in SQLite library index (`rusqlite` bundled). Lazily
  opened; filesystem browsing never touches SQLite. CLI: `--index`, `--reindex DIR`.
- `ffmpeg-lgpl`: links a system FFmpeg 9 (LGPL builds only; GPL/nonfree builds
  must never be distributed). Build-time discovery via `pkg-config`/`vcpkg`, or set
  `FFMPEG_DIR` (and `LIBCLANG_PATH` for bindgen) to a prefix with `include/`+`lib/`.

```sh
cargo build --release --features library-index
cargo build --release --features ffmpeg-lgpl          # requires system FFmpeg 9
```

Run the test suite:

```sh
cargo test --workspace
```

## Repository layout

```
crates/
  core              fidelity types, exact packing, DoP packer, contracts
  source            read-ahead source with stall detection/recovery
  decode            Symphonia + Ogg Opus + FFmpeg decoders, lofty tags
  playlist          M3U/M3U8/PLS/XSPF/CUE
  library           opt-in SQLite index (rusqlite bundled)
  output-alsa       ALSA hw: backend, native DSD/DoP negotiation
  output-wasapi     WASAPI exclusive backend
  output-coreaudio  Core Audio HAL hog backend
  tui               ratatui/crossterm UI + filesystem browser
  app               player engine (coordinator + decoder worker), CLI
docs/PLAN.md        design contract, milestones, acceptance criteria
```

## Contributing

Issues and pull requests are welcome. The fidelity contract in
[`docs/PLAN.md`](docs/PLAN.md) is the design boundary: changes that convert,
resample, or remix audio without an explicit, labeled user opt-in will not be merged.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

The optional `ffmpeg-lgpl` feature dynamically links FFmpeg, which is licensed
LGPL-2.1-or-later; only distribute binaries against audited LGPL FFmpeg builds.
