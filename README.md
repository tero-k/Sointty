# Sointty

Terminal hi-fi music player with a bit-perfect fidelity contract: decoder-produced
samples travel unchanged through the application into an OS-native direct/exclusive
endpoint. No resampling, no channel remixing, no float/int conversion, no software
volume, no ReplayGain/EQ/dither — and no shared-mode mixers anywhere in the path.

Status: **v1 code complete** (milestones M1–M6, see [`docs/PLAN.md`](docs/PLAN.md)).
Per-OS hardware acceptance is tracked in the plan; unit-tested on Windows, primary
target Linux.

## Fidelity contract

- PCM passes through at the decoder's sample values, rate, and channel mapping.
  Lossless interleaving, endian packing, and exact integer widening
  (e.g. S24 in a 32-bit slot with zero low bits) are the only permitted transforms.
- Exact rate, channels/channel mask, packing, and valid bits are confirmed with the
  device at initialization — the established configuration, not a requested nearest.
- Unsupported combinations fail typed (`UnsupportedFormat`) instead of converting.
  An f32-decoded stream on an integer-only endpoint is rejected, not narrowed.
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

Playlists: M3U/M3U8 (read+write, relocation-safe relative paths), PLS, XSPF,
single-image CUE (75 Hz INDEX 01 boundary math, gapless adjacent ranges).
Remote URLs are rejected — local files only.

## Platforms

| OS | Output backend | State |
|---|---|---|
| Linux | ALSA `hw:` direct, native DSD + DoP negotiation | code complete; hardware acceptance pending |
| Windows | WASAPI exclusive event-driven (S_OK-only format negotiation, MMCSS) | verified on hardware (iFi USB endpoint) |
| macOS | Core Audio HAL hog mode, nominal-rate + physical-format set/readback | cross-checked; live-device test pending |

## Building

MSRV: Rust 1.98.1 (edition 2024).

```sh
# Linux: ALSA headers required; libopus for the default Opus support
sudo apt install libasound2-dev libopus-dev
cargo build --release

# Windows / macOS: no system audio deps
cargo build --release
```

Feature flags (on the `sointty` app crate):

- `library-index` — opt-in SQLite library index (`rusqlite` bundled). Lazily
  opened; filesystem browsing never touches SQLite. CLI: `--index`, `--reindex DIR`.
- `ffmpeg-lgpl` — links a system FFmpeg 9 (LGPL builds only; GPL/nonfree builds
  must never be distributed). Build-time discovery via `pkg-config`/`vcpkg`, or set
  `FFMPEG_DIR` (and `LIBCLANG_PATH` for bindgen) to a prefix with `include/`+`lib/`.

```sh
cargo build --release --features library-index
cargo build --release --features ffmpeg-lgpl          # requires system FFmpeg 9
```

## Usage

```
sointty [--device DEVICE] [--period-frames N] [--buffer-frames N] [--list-devices]
        [--index] [--reindex DIR]... [FILE|PLAYLIST]...
```

- With no files, opens the TUI filesystem browser (no database involved).
- Playlists (`.m3u`/`.m3u8`/`.pls`/`.xspf`/`.cue`) are expanded into tracks.
- `--list-devices` enumerates endpoint IDs usable with `--device`.
- Gapless: tracks with an identical `StreamSpec` transition seamlessly on one open
  device; the `Playing` event fires only after the boundary frame is consumed.

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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

The optional `ffmpeg-lgpl` feature dynamically links FFmpeg, which is licensed
LGPL-2.1-or-later; only distribute binaries against audited LGPL FFmpeg builds.
