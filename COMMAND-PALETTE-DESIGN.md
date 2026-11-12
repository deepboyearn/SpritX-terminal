# Command palette redesign

## Direction

- Product: a keyboard-first action picker for the native SpritX terminal.
- Character: quiet, exact, quick to scan.
- World: a developer's command desk, using the existing workspace colors and font.
- Moments: the current command gets one restrained selection surface; the rest stays quiet.
- Signature: individually set keyboard keycaps, aligned beside their actions.

## Audit and changes

- Repeated `Run action`/`Open sub-menu` subtitles made every row twice as noisy.
  Actions now use one 40px row with one label and a consistent 20px icon column.
- One long uppercase `ACTIONS` list gave little hierarchy. Terminal, Workspace
  and Tools groups retain the existing command order and dispatch indices.
- Search looked like a static heading. It now has a separate 44px inset field,
  a clear-query control, and a compact title with a clickable Escape control.
- Shortcut strings floated at the right edge. Each modifier/key now has its
  own keycap; submenu commands use the existing Hugeicons chevron.
- Selection used an accent outline plus a competing hover fill. Selection now
  uses the existing neutral outer surface, a quiet hairline and an Enter mark.
- The 600px panel has a 16px outer radius, 8px inner rows, and a compact footer.
  The header/footer stay fixed while results scroll. Keyboard selection scrolls
  into view after accounting for the group headings; mouse scrolling is retained.

The user-selected font, light/dark colors, native Rust/GPUI rendering, command
filtering, remote connection entry, history and keyboard dispatch are preserved.
The same header and footer treatment applies to command history.

## Validation

- `cargo check -j 2 --tests`: passed.
- `cargo test -j 2 --lib -- --test-threads=1`: 76 passed.
- Native palette smoke tests in light and dark workspaces: 2 passed.
- `cargo build -j 2` and `git diff --check`: passed.
- User's running terminal was retained; isolated preview processes were closed.

Native runtime checks were run on Windows. Linux runtime was not available in
this environment; the changes use shared GPUI controls and existing theme tokens.

Design workflow: **unslop by Damir Sokolovsky (@heydamir), CC BY 4.0**.
https://heydamir.design/unslop
