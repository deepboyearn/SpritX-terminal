# SpritX application artwork

The mascot was supplied by the project owner. `icon.svg` is a path-only,
256-color vector contour trace with a transparent outer background; it does
not contain an embedded bitmap. Raster grain is simplified by vectorization.

`icon-32.png`, `icon-128.png`, `icon-256.png`, `icon-512.png` and `icon.ico`
are exported directly from the supplied artwork to retain its original
gradients and texture in the native UI, tray, taskbar and packaged apps.
GPUI's `svg()` element is a monochrome mask, so the app logo uses `img()`.

To regenerate from the original artwork:

```sh
python -m pip install pillow numpy opencv-python-headless
python tools/generate_app_icon.py /path/to/artwork.png
```

The Discord toolbar icon is Hugeicons' MIT-licensed `DiscordIcon`, from
https://github.com/hugeicons/hugeicons/blob/main/packages/core-free-icons/src/DiscordIcon.ts
and follows the existing stroke-rounded icon style. The MIT notice is in
`icons/LICENSE-Hugeicons.txt`. Clicking it opens the owner's community invite:
https://discord.gg/ecyzVxaSfV
