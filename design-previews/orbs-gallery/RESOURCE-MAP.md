# Thinking Orbs — original resources

Repository: https://github.com/Jakubantalik/Libraries.dev

Pinned revision: `387b049ca8fe97b5d72999b5bf61a19ab1ccff81`

All paths below are inside `upstream/`. Original files are unmodified.
`resources.json` lists all 123 downloaded files, their byte sizes and Git blob hashes.
Build caches and generated iOS compiler artifacts are omitted.

## Component and animation source

| Resource | Original path |
| --- | --- |
| React component, frame loop, visibility/pause handling | `packages/thinking-orbs/src/ThinkingOrb.tsx` |
| All states, sizes and options | `packages/thinking-orbs/src/types.ts` |
| State-to-engine mapping and 20/32/64px presets | `packages/thinking-orbs/src/presets.ts` |
| Camera, depth sorting and canvas painter | `packages/thinking-orbs/src/engine/core.ts` |
| Base profiles and density/radius scaling | `packages/thinking-orbs/src/engine/profiles.ts` |
| Engine registry | `packages/thinking-orbs/src/engine/registry.ts` |
| Light/dark and reduced-motion detection | `packages/thinking-orbs/src/theme.ts` |
| Optional pointer interaction | `packages/thinking-orbs/src/gravity.ts` |
| Reference geometry and preset specification | `packages/thinking-orbs/spec/` |
| Native iOS and React Native reference ports | `packages/thinking-orbs/ports/` |

## All nine states

| Gallery number | State | Engine | Geometry source |
| --- | --- | --- | --- |
| 01 | Working | `orbits` | `engine/orbits.ts` |
| 02 | Searching | `globe` | `engine/lattice.ts` |
| 03 | Solving | `rubik` | `engine/lattice.ts` |
| 04 | Listening | `wave` | `engine/lattice.ts` |
| 05 | Connecting | `web` | `engine/web.ts` |
| 06 | Weaving | `braid` | `engine/braid.ts` |
| 07 | **Composing — screenshot's “Thinking….” pill** | `ribbon` | `engine/ribbon.ts` |
| 08 | Breathing | `ring` | `engine/ribbon.ts` |
| 09 | Shaping | `morph` | `engine/morph.ts` |

Engine paths in this table are relative to `packages/thinking-orbs/src/`.

The 64px and 20px presets are separately tuned; the 32px version is interpolated.
The official hero UI displays the 64px preset inside a 56px canvas box.

## Original UI components and CSS classes

| UI | Resource / classes |
| --- | --- |
| Screenshot label mapping | `sites/home/src/orbs.tsx` → `HERO_PILLS`: `composing` → `Thinking….` |
| Large pills and small chips | `sites/orbs/components/Examples.tsx` → `listeningPillClass`, `chipClass`, `HERO_PILLS`, `CHIP_STATES` |
| Original example wrappers | `sites/home/src/orbs.tsx` → `.detail-examples`, `.ex-orb-heroes`, `.ex-orb-grid`, `.ex-orb-cell`, `.ex-orb-cell--hero`, `.ex-orb-cell--lg`, `.ex-pill`, `.ex-chip` |
| Pill fill, border/shadow, text shimmer | `sites/orbs/styles.css` → `--pill-fill`, `--pill-stroke`, `.t-shimmer`, `@keyframes t-shimmer` |
| Shared website example styles | `sites/home/public/assets/examples.css` |
| Shared playground and page styles | `sites/home/public/assets/playground.css`, `site.css`, `nav-products.css` |
| Theme tokens and Tailwind base styles | `sites/orbs/tailwind.css` |
| State/size/speed playground | `sites/orbs/components/Playground.tsx` |
| State/size buttons | `sites/orbs/components/TabBtn.tsx` |
| Play/pause control | `sites/orbs/components/PlayPauseToggle.tsx` |
| Copy control | `sites/orbs/components/CopyButton.tsx` |
| Copy/play icon transitions | `sites/orbs/styles.css` → `.t-icon`, `[data-state]`, `[data-icon]` |

## Review gallery

`gallery.tsx` is a local selection interface around the original components.
It mounts the original `Examples` and `Playground` components unchanged.
`preview-orb.tsx` forwards a global pause control to the original `ThinkingOrb`.
`review.css` imports the original theme and shimmer CSS, and adds gallery layout.
`gallery.js` / `gallery.css` are prebuilt local browser assets; no CDN is required.

This browser preview uses the repository's original React/TypeScript source.
It is isolated from the Rust application. A selected design can be ported to
Rust/GPUI after approval; this resource collection does not change terminal behavior.

## License

MIT, copyright Jakub Antalik. Retained in
`packages/thinking-orbs/LICENSE` and the downloaded repository `LICENSE`.
