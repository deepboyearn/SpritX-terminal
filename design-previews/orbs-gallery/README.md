# Original Thinking Orbs review gallery

Open `index.html` in a browser. The prebuilt gallery works locally without
installing packages or downloading assets from a CDN.

The default selection is **07 Composing**, which the original website uses
for the reference **Thinking….** pill. Click any of the nine component cards
to preview it at 64px, 32px and 20px, including a terminal-tab mockup. Light/dark,
speed, global pause, the original example UI and the original playground are
included. Copy selection provides the state, size, speed and theme for approval.

## Preview server (optional)

From this directory:

```text
python -m http.server 43821 --bind 127.0.0.1
```

Open http://127.0.0.1:43821/ in a browser. A server also enables normal source
links and clipboard behavior. The prebuilt HTML still works by double-click.

## Rebuild (only when editing this browser preview)

```text
npm ci --ignore-scripts
npm run build
```

Dependencies are scoped to this preview directory. The Cargo project and
Rust application's dependencies are not changed.

See `RESOURCE-MAP.md` for all states, original classes and file locations.
`resources.json` pins 123 original resource files to revision
`387b049ca8fe97b5d72999b5bf61a19ab1ccff81` of
https://github.com/Jakubantalik/Libraries.dev.

No new terminal integration was made in this preview step. Existing Rust
changes from earlier work are separate and have not been reverted.
