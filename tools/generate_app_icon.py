"""Export a supplied SpritX artwork as path-only SVG, PNG sizes and Windows ICO.

Run with Python + Pillow, numpy and opencv-python-headless:
  python tools/generate_app_icon.py /path/to/artwork.png
Only the connected outer white background is removed; the mascot stays intact.
PNG/ICO exports preserve the source's gradients and texture. The vector master
uses a 256-color contour trace, rather than embedding a bitmap inside an SVG.
"""
import argparse
from pathlib import Path

import cv2
import numpy as np
from PIL import Image, ImageDraw, ImageFilter


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1] / "assets"
    source = Image.open(args.source).convert("RGBA")
    ImageDraw.floodfill(source, (0, 0), (0, 0, 0, 0), thresh=32)
    # Keep the artwork's aspect ratio in a square transparent canvas.
    side = max(source.size)
    canvas = Image.new("RGBA", (side, side))
    canvas.alpha_composite(source, ((side - source.width) // 2, (side - source.height) // 2))
    for size in (32, 128, 256, 512):
        canvas.resize((size, size), Image.Resampling.LANCZOS).save(root / f"icon-{size}.png")
    canvas.save(root / "icon.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])

    size = 768
    trace = canvas.resize((size, size), Image.Resampling.LANCZOS)
    colors = trace.convert("RGB").filter(ImageFilter.MedianFilter(3)).quantize(colors=256)
    pixels, alpha, palette = np.array(colors), np.array(trace.getchannel("A")), colors.getpalette()
    svg = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" width="{size}" height="{size}">',
           '<title>SpritX Terminal</title>',
           '<!-- Vector contour trace of owner-supplied artwork; no embedded raster. -->']
    for index in np.unique(pixels):
        mask = np.uint8((pixels == index) & (alpha >= 128)) * 255
        contours, _ = cv2.findContours(mask, cv2.RETR_LIST, cv2.CHAIN_APPROX_SIMPLE)
        paths = []
        for contour in contours:
            points = cv2.approxPolyDP(contour, 0.3, True).reshape(-1, 2)
            if len(points) < 3:
                for x, y in points:
                    paths.append(f"M{x-.5} {y-.5}h1v1h-1Z")
                continue
            paths.append("M" + "L".join(f"{x} {y}" for x, y in points) + "Z")
        if paths:
            rgb = palette[int(index) * 3:int(index) * 3 + 3]
            color = "#" + "".join(f"{c:02x}" for c in rgb)
            # Contours run through pixel centers. A matching half-pixel edge
            # prevents transparent seams between neighboring color regions.
            svg.append(f'<path fill="{color}" stroke="{color}" stroke-width="1.5" stroke-linejoin="round" fill-rule="evenodd" d="{"".join(paths)}"/>')
    svg.append("</svg>")
    (root / "icon.svg").write_text("\n".join(svg) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
