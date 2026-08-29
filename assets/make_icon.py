"""Generate the Resonance application icon.

    python assets/make_icon.py assets

Writes `icon.ico` (embedded in the executable) and `icon.png` (loaded at
startup for the taskbar and Alt-Tab). Both are committed, so this only needs
running when the design changes.

The mark is a pulse inside a resonance ring: a symmetric waveform radiating
from the centre, enclosed by the ring it is exciting. The symmetry is the
point — an asymmetric bar cluster is the stock music-app glyph and reads as
arbitrary, whereas a shape that tapers from the middle reads as something
sounding.

Every size is drawn from scratch rather than downsampled from one master. Two
hairline rings and seven bars scaled to 16 pixels turn into a violet smudge,
so the small sizes drop the rings and lose bars until what is left is what the
eye could resolve anyway. This is why the file is a script and not a PNG.

Requires Pillow.
"""

import os
import sys

from PIL import Image, ImageChops, ImageDraw, ImageFilter

OUT = sys.argv[1] if len(sys.argv) > 1 else "."

# Everything is drawn oversized and shrunk, because PIL has no antialiased
# drawing of its own. Beyond this canvas there is cost and no visible gain.
SS = 8
MAX_CANVAS = 2048

# --- palette ---------------------------------------------------------------
# The plate is deep violet rather than near-black: a black icon dissolves into
# the Windows dark taskbar, which is where this spends most of its life.
PLATE_TOP = (36, 28, 66)
PLATE_BOTTOM = (17, 13, 32)
# The app accent (#7C5CFF) sits between these two.
MARK_TOP = (176, 150, 255)
MARK_BOTTOM = (96, 62, 236)
GLOW = (124, 92, 255)
# Hairline rim, so the silhouette stays defined against a black background.
RIM = (150, 130, 220)

SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]


def plan(size):
    """Bar heights, bar span and rings for `size`, in fractions of the icon.

    Rings are `(radius, stroke, alpha)`. Below 32 pixels there is no room for
    a ring around the bars that is not simply a grey halo, so there is none.
    """
    if size <= 20:
        return [0.21, 0.44, 0.21], 0.52, []
    if size <= 24:
        return [0.16, 0.32, 0.46, 0.32, 0.16], 0.56, []
    if size <= 48:
        return [0.13, 0.28, 0.44, 0.28, 0.13], 0.42, [(0.395, 0.038, 0.90)]
    return (
        [0.11, 0.22, 0.34, 0.47, 0.34, 0.22, 0.11],
        0.46,
        [(0.395, 0.028, 0.88), (0.468, 0.013, 0.30)],
    )


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def gradient(box, top, bottom):
    """A vertical gradient, built one pixel wide and stretched."""
    strip = Image.new("RGB", (1, box[1]))
    pixels = strip.load()
    for y in range(box[1]):
        pixels[0, y] = lerp(top, bottom, y / max(box[1] - 1, 1))
    return strip.resize(box, Image.BICUBIC)


def tint(box, colour, alpha):
    """A flat colour carrying `alpha` as its mask."""
    return Image.merge("RGBA", (*[Image.new("L", box, c) for c in colour], alpha))


def draw(size):
    """Render one icon at `size` pixels square, as RGBA."""
    scale = min(SS, max(1, MAX_CANVAS // size))
    n = size * scale
    box = (n, n)
    centre = n / 2.0

    # --- the plate ---------------------------------------------------------
    margin = n * 0.02
    corner = n * 0.205
    plate_mask = Image.new("L", box, 0)
    ImageDraw.Draw(plate_mask).rounded_rectangle(
        [margin, margin, n - 1 - margin, n - 1 - margin], radius=corner, fill=255
    )

    icon = Image.new("RGBA", box, (0, 0, 0, 0))
    icon.paste(gradient(box, PLATE_TOP, PLATE_BOTTOM).convert("RGBA"), (0, 0), plate_mask)

    # A soft bloom in the upper left, echoing the aurora visualiser.
    bloom = Image.new("L", box, 0)
    ImageDraw.Draw(bloom).ellipse([-n * 0.25, -n * 0.35, n * 0.75, n * 0.65], fill=85)
    bloom = bloom.filter(ImageFilter.GaussianBlur(n * 0.13))
    bloom = Image.composite(bloom, Image.new("L", box, 0), plate_mask)
    icon.alpha_composite(tint(box, GLOW, bloom))

    # --- the mark ----------------------------------------------------------
    heights, span_f, rings = plan(size)
    mark = Image.new("L", box, 0)
    painter = ImageDraw.Draw(mark)

    # Bars sit on a unit grid of bar-then-half-gap, so the spacing holds
    # whatever the bar count.
    span = span_f * n
    unit = span / (len(heights) * 1.5 - 0.5)
    step = unit * 1.5
    left = centre - span / 2.0

    for index, height in enumerate(heights):
        x = left + index * step
        # Never shorter than it is wide, or a short bar becomes an ellipse.
        half = max(unit / 2.0, height * 0.5 * n)
        painter.rounded_rectangle(
            [x, centre - half, x + unit, centre + half], radius=unit / 2.0, fill=255
        )

    for radius_f, stroke_f, alpha in rings:
        r = radius_f * n
        layer = Image.new("L", box, 0)
        ImageDraw.Draw(layer).ellipse(
            [centre - r, centre - r, centre + r, centre + r],
            outline=round(255 * alpha),
            width=max(1, round(stroke_f * n)),
        )
        mark = ImageChops.lighter(mark, layer)

    # A halo under the mark, which is what stops it looking like a diagram.
    halo = mark.filter(ImageFilter.GaussianBlur(n * 0.032)).point(lambda v: v * 0.58)
    halo = Image.composite(halo, Image.new("L", box, 0), plate_mask)
    icon.alpha_composite(tint(box, GLOW, halo))

    body = gradient(box, MARK_TOP, MARK_BOTTOM).convert("RGBA")
    body.putalpha(mark)
    icon.alpha_composite(body)

    # --- rim ---------------------------------------------------------------
    if size >= 32:
        rim = Image.new("L", box, 0)
        ImageDraw.Draw(rim).rounded_rectangle(
            [margin, margin, n - 1 - margin, n - 1 - margin],
            radius=corner,
            outline=64,
            width=max(1, round(n * 0.008)),
        )
        icon.alpha_composite(tint(box, RIM, rim))

    return icon.resize((size, size), Image.LANCZOS) if scale > 1 else icon


def main():
    images = {size: draw(size) for size in SIZES}

    ico = os.path.join(OUT, "icon.ico")
    images[256].save(
        ico,
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=[images[s] for s in SIZES if s != 256],
    )
    png = os.path.join(OUT, "icon.png")
    images[256].save(png, format="PNG")

    print(f"{ico}  {os.path.getsize(ico)} bytes, sizes {SIZES}")
    print(f"{png}  {os.path.getsize(png)} bytes")


if __name__ == "__main__":
    main()
