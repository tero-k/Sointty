# Agent conventions for Sointty

- **No em dash (`—`, U+2014) in `README.md` or `docs/`.** Use a comma, colon,
  semicolon, or parentheses instead. This is a standing user rule; apply it in
  every session, including when editing existing text.
- Fidelity contract: never convert, resample, or remix audio without an
  explicit, labeled user opt-in (see `docs/PLAN.md`).
- The RT output path (render threads) must not allocate, lock, or do I/O other
  than device writes.
- Run `cargo test --workspace` before committing; verify playback changes on
  real hardware when a device is available.
