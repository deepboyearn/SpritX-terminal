# CLI brand artwork

These are full-color source assets and 128px transparent PNG exports used by
the launcher. Source URLs and SHA-256 hashes are in `sources.json`.

| Launcher | Source |
| --- | --- |
| Codex | LobeHub's color variant, including the original blue/violet gradient and white plate |
| Claude Code | LobeHub's color variant, `#D97757` |
| OpenCode | Existing MIT LobeHub silhouette, resolved to its published black ink |
| Pi | `https://pi.dev/logo-auto.svg`, including all three original fills |
| Oh My Pi | The project's own `assets/icon.svg`, including its white mark and orange connector |

SVG sources are preserved. PNGs were rendered from them, with transparent
outer padding trimmed and their aspect ratio preserved in a 128×128 frame.
They are embedded by `build.rs` and loaded through GPUI's image renderer.
GPUI's SVG element is a monochrome mask, so using it would lose the Codex
gradient and Pi/Oh My Pi's multiple fills.

Image opacity remains 1, including unavailable/disabled launcher entries.
Fixed white plates (dark for Oh My Pi) preserve contrast and the same artwork
appearance in light and dark workspaces. Availability is described by the
existing tooltip and actions, not by fading or recoloring the logo.

Original MIT notices are retained in the three `LICENSE-*.txt` files.
