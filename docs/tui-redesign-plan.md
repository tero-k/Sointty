# TUI redesign plan

Replace the pane-and-debug-block TUI with four terminal-cell surfaces:
Library + Queue with Now playing, Search overlay, Playlists with bulk
editing, and Settings. Decoding, PCM/DSD packing, output negotiation,
render threads, and bit-perfect rules stay unchanged; the only player
change is control-plane queue commands and presentation fields on the
existing `Playing` event.

## Current wiring

- `crates/tui/src/lib.rs`: `TuiState`, `App`, `Pane`, key handling,
  `event_loop`, `draw`, and TestBackend tests.
- `crates/tui/src/browser.rs`: `Browser`, `list_dir`,
  `collect_audio_files`.
- `crates/app/src/main.rs::run` builds `RunOptions`/`Hooks` and calls
  `sointty_tui::run`.
- Workspace UI deps: `ratatui 0.30.2`, `crossterm 0.29.0`.

## New modules

- `crates/tui/src/theme.rs`: color tokens, truecolor detection,
  `NO_COLOR` fallback, optional accent parsing.
- `crates/tui/src/layout.rs`: responsive layout helpers (wide, medium,
  narrow).
- `crates/tui/src/widgets.rs`: small render helpers (panels, keycap
  footer, progress bar, track row, signal path line).
- `crates/tui/src/display.rs`: display title/tag derivation from paths
  and tags, CUE range labels.
- `crates/tui/src/keymap.rs`: single typed binding table driving
  dispatch, footer, and help.
- `crates/tui/src/search.rs`: background metadata/index worker with a
  path-keyed cache, priority requests, and generation-tagged queries.
- `crates/tui/src/screens/{library,queue,player,playlists,settings}.rs`:
  per-surface rendering and input.
- `crates/decode/src/tags.rs`: presentation-only
  `DisplayMetadata`/`read_display_metadata` using the existing lofty
  dependency.

## Execution order

1. Protect the mockup (`.gitignore`), this doc, theme/layout/widgets,
   runnable visual foundation.
2. Library, locations, and Queue screens; display titles; junk-file
   scan filter; control-plane queue commands (`PlayNow`,
   `RemoveQueueEntry`, `MoveQueueEntry`, `JumpToQueueEntry`) with
   `QueueEditRejected`.
3. Now playing and source signal path (`Playing` gains read-only
   `source`/`buffers`).
4. Search overlay on the background worker, non-blocking.
5. Playlist editing with per-list dirty flags and explicit `Ctrl+S`
   save, dirty-aware quit prompt.
6. Settings screen: device picker/reset, timing, compatibility
   toggles, accent, config path/error display, keys reference.
7. Narrow-terminal layouts, progress coalescing, removal of retired UI
   code, README update, snapshots.

Each slice is verified with `cargo build`, `cargo test --workspace`,
and `cargo clippy -- -D warnings` before its focused commit. The root
HTML mockup stays untracked and ignored.
