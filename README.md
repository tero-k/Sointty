# Sointty

A terminal music player for people who care what actually reaches their DAC.
What the decoder produces is what the device gets: same samples, same rate,
same channels. No resampling, no volume processing, no EQ, no mixer in between.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![MSRV: 1.98.1](https://img.shields.io/badge/rustc-1.98.1%2B-orange.svg)](https://blog.rust-lang.org/releases/)
[![Platform: Linux | Windows | macOS](https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey.svg)](#platforms)

Sointty is v1 feature complete (see [`docs/PLAN.md`](docs/PLAN.md)). It has been
played through a real iFi USB DAC on Windows; Linux and macOS support is written
and unit tested, hardware testing still pending.

## Why another player

Most players quietly "help" your audio on the way out: resample it to whatever
the OS mixer wants, convert formats, apply fade and gain. Sointty refuses to do
any of that. If your DAC can't take the stream exactly as decoded, you get a
clear error instead of silent degradation. If you *want* that conversion anyway
(say, MP3 on an integer-only DAC), you can opt in explicitly, and the UI tells
you plainly when the output is no longer bit-perfect.

## What you get

- Bit-perfect playback with the negotiated format shown on screen, so you can
  see what was actually agreed with the device, not what was requested.
- Gapless albums: same-format tracks flow through one open stream without a
  click or a reopen.
- A keyboard-driven TUI with a filesystem browser, queue, seek, and live tags.
- Playlist support: M3U/M3U8, PLS, XSPF, and single-image CUE sheets.
- Playback that survives slow disks and network mounts: reads happen ahead of
  time, and a stall is reported honestly instead of papered over.

## Quick start

```sh
# Linux: ALSA headers required; libopus for the default Opus support
sudo apt install libasound2-dev libopus-dev
cargo build --release

# Windows / macOS: no system audio dependencies
cargo build --release
```

```sh
sointty --list-devices                 # find your DAC
sointty /path/to/album/                # or just open files and folders
sointty playlist.m3u
```

Run it with no arguments and you get a file browser. Press `d` to choose your
output device; the choice is remembered for next time.

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
`~/.config/sointty/config.toml` on Linux). Command-line flags win over the file.

## The fidelity promise

This is the part Sointty is picky about:

- Your PCM goes through untouched. The only allowed transforms are ones that
  provably change nothing: interleaving channels, byte-order packing, and
  widening into a bigger container with zeroed low bits (like 24-bit audio in a
  32-bit slot).
- The device is asked for exactly what the file needs, and the answer is
  checked, not assumed.
- Lossy decoders that produce floats (MP3, AAC, Vorbis, Opus) don't play on
  integer-only DACs unless you press `c` or pass `--allow-float-to-int`. When
  you do, the conversion is honest arithmetic (round to nearest, clip, no
  dither) and the screen says `NOT bit-perfect` so there's no pretending.
- DSD stays DSD: native or DoP on ALSA, never dressed up as PCM.

## Formats

| Source | Formats | Notes |
|---|---|---|
| Symphonia (built in) | FLAC, ALAC, WAV/AIFF, MP3, AAC-LC, Vorbis | always available |
| Ogg Opus | `.opus` | default on Linux, `--features opus` elsewhere |
| FFmpeg (opt-in) | APE, WavPack, TAK, Musepack, DSF/DFF | `--features ffmpeg-lgpl`, needs system FFmpeg 9 |

## Platforms

| OS | Backend | Status |
|---|---|---|
| Linux | ALSA `hw:` direct, native DSD and DoP | written, awaiting hardware test |
| Windows | WASAPI exclusive, with a push-mode fallback for quirky USB drivers | tested on an iFi DAC |
| macOS | Core Audio HAL hog mode | written, awaiting hardware test |

## Building details

MSRV is Rust 1.98.1 (edition 2024). Two optional features on the app crate:

- `library-index`: an opt-in SQLite index of your collection. Off by default;
  browsing folders never touches a database.
- `ffmpeg-lgpl`: extra formats via a system FFmpeg 9. LGPL builds only; set
  `FFMPEG_DIR` (and `LIBCLANG_PATH` for bindgen) if it isn't found.

```sh
cargo build --release --features library-index
cargo build --release --features ffmpeg-lgpl
cargo test --workspace
```

## Repository layout

```
crates/
  core              shared types and the exact-packing rules
  source            read-ahead file access with stall recovery
  decode            Symphonia, Opus, and FFmpeg decoders; tag reading
  playlist          M3U/M3U8/PLS/XSPF/CUE
  library           the optional SQLite index
  output-alsa       Linux ALSA backend
  output-wasapi     Windows WASAPI backend
  output-coreaudio  macOS Core Audio backend
  tui               terminal UI and file browser
  app               player engine and command line
docs/PLAN.md        design notes, milestones, acceptance criteria
```

## Contributing

Issues and pull requests are welcome. One house rule: anything that changes
your audio needs to be explicit, opt-in, and labeled. Quiet "improvements" to
the signal are bugs here.

## License

MIT or Apache-2.0, your choice: [LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE).

The optional `ffmpeg-lgpl` feature dynamically links FFmpeg (LGPL-2.1-or-later);
only distribute binaries built against LGPL FFmpeg builds.
