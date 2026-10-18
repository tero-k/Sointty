# Changelog

These are retrospectively named development milestones based on dated commits.
They are not tagged or published releases, and the 0.1.x numbers were not the
manifest versions at those dates; they exist to summarize how the project
evolved toward the current build. `0.2.0` and the `Unreleased` section
describe the code as it exists today.

## Unreleased

- Fix search freezing the whole app on broad queries: the search worker
  indexes whole drives, so short queries matched tens of thousands of
  entries and the UI spent all its time draining and re-sorting results.
  The UI now drains worker events in bounded ticks and re-sorts once per
  pass, results are capped to the best 2048 ranks, keystroke queries are
  coalesced so only the newest runs, and the index is kept across search
  opens instead of being rebuilt every time.

## 0.2.0 (2026-10-18)

- Rebuilt the terminal UI as three screens switched with `1`/`2`/`3`: Library
  (file browser, queue, and Now playing panel), Playlists, and Settings.
- Context-sensitive key guide at the bottom of every screen, with `?` for the
  full key list.
- Search across library scopes with play, queue, playlist, and open-folder
  actions.
- Responsive layouts that adapt to terminal width, plus display titles and
  presentation metadata for tidier track rows.
- Pending-queue control commands in the player control plane.
- Fix pause: `space` pauses and resumes the current track in place instead
  of stopping it and advancing the queue to the next track.
- Fix shifted shortcuts: `Shift+A` (queue album), `Shift+Q` (queue playlist
  selection), `?` (full key list), and `Y`/`N` quit answers now register,
  where previously only Caps Lock produced them.
- Show the playback keys (`space`, `s`, `n`, `←`/`→`, `r`) on a dedicated
  footer row below the context key guide.

## 0.1.4 (2026-10-17)

- Redraw the TUI on terminal resize, following the October screenshot and
  documentation refresh.

## 0.1.3 (2026-10-03)

- macOS Core Audio Integer Mode negotiation: non-mixable integer stream
  formats are preferred for the hardware and IOProc sides under hog mode.
- Verified bit-perfect playback on an iFi ZEN DAC V2, including the float
  IOProc fallback path via the integer-to-F32 opt-in.

## 0.1.2 (2026-09-28)

- Recover the exact source encoding of FLAC streams widened into S32
  containers.
- Read Core Audio physical formats as ranged descriptions and re-verify the
  format after buffer-size changes; match S24 streams into 32-bit slots.
- Add the opt-in integer-to-F32 conversion for float-only endpoints.

## 0.1.1 (2026-09-27)

- Windows: fix F32 compatibility opt-in and WASAPI push-mode clicks on
  drivers that force whole-buffer periods.
- Add persistent named playlists, the locations view for drives and network
  shares, and queue controls.

## 0.1.0 (2026-09-26)

- Initial player: terminal UI, Symphonia-based decoding, and direct ALSA,
  WASAPI, and Core Audio backends.
- Bit-perfect fidelity contract: no resampling or mixing, failures typed
  instead of silent conversions.
