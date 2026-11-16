"""Draws Pumpkin's logo, an open ring with a tile turned 45 degrees in the break at its top.

Run `python3 docs/logo.py --output docs/figures/logo.svg` from the repository's root, which is
also what it does without `--output`. It writes the same bytes each time: an SVG that reads on
GitHub's light and dark pages, as the other figures in `docs/figures/` do.

The proportions are those of FrontierCS's mark, as a share of each mark's own height.
"""

import argparse
import math
from pathlib import Path

DEFAULT_OUTPUT = Path(__file__).resolve().parent / "figures" / "logo.svg"

# The ring's radius, to the middle of its stroke. The tile's centre is on the ring at its top.
RADIUS = 50.0

# FrontierCS's mark: the height of its ink, its tile's side and corner radius, its stroke, and
# the space from the tile's corner to the arc's outer edge (the tile's centre is 45.2 units
# right of the arc's centre and as far above it).
FRONTIER_HEIGHT = 74.7
FRONTIER_TILE = 24.0
FRONTIER_CORNER = 5.5
FRONTIER_STROKE = 11.0
FRONTIER_SPACE = math.hypot(45.2, 45.2) - (RADIUS + FRONTIER_STROKE / 2)

MARGIN = 16.0
SCALE = 1.5  # pixels to a unit of the drawing

TEMPLATE = """\
<!-- python3 docs/logo.py writes this file: edit docs/logo.py, not this -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x:.2f} {y:.2f} {width:.2f} {height:.2f}" \
width="{pixel_width:.0f}" height="{pixel_height:.0f}" role="img" \
aria-labelledby="title desc">
<title id="title">Pumpkin's logo</title>
<desc id="desc">An open ring with an orange tile, turned 45 degrees, in the break at its top.\
</desc>
<style>
:root{{--bg:#ffffff;--ink:#1f2328}}
@media (prefers-color-scheme:dark){{:root{{--bg:#0d1117;--ink:#e6edf3}}}}
.bg{{fill:var(--bg)}}
.ring{{fill:none;stroke:var(--ink);stroke-width:{stroke:.2f};stroke-linecap:round}}
.tile{{fill:#e8710a}}
</style>
<rect class="bg" x="{x:.2f}" y="{y:.2f}" width="{width:.2f}" height="{height:.2f}"/>
<path class="ring" d="M{end_x:.2f} {end_y:.2f} A{radius:g} {radius:g} 0 1 1 \
{mirror_x:.2f} {end_y:.2f}"/>
<rect class="tile" x="{tile_x:.2f}" y="{tile_y:.2f}" width="{tile:.2f}" height="{tile:.2f}" \
rx="{corner:.2f}" transform="rotate(45 0 {top:g})"/>
</svg>
"""


def distance_to_tile(x, y, side, corner):
    """The distance from the point (x, y) to the tile, a square of `side` with rounded corners."""
    # The point in the tile's own axes: the tile is turned 45 degrees about its centre, (0, -R).
    u = (x + (y + RADIUS)) / math.sqrt(2)
    v = (x - (y + RADIUS)) / math.sqrt(2)
    inner = side / 2 - corner
    return math.hypot(max(abs(u) - inner, 0.0), max(abs(v) - inner, 0.0)) - corner


def ring_end(side, corner, reach):
    """The right end of the ring's centre line: the point of the ring from which the round end of
    the stroke stops `space` short of the tile, found by halving the angle from the top."""
    near, far = 0.0, math.pi / 2
    for _ in range(50):
        angle = (near + far) / 2
        x, y = RADIUS * math.sin(angle), -RADIUS * math.cos(angle)
        if distance_to_tile(x, y, side, corner) < reach:
            near = angle
        else:
            far = angle
    return RADIUS * math.sin(near), -RADIUS * math.cos(near)


def logo():
    """The logo's SVG."""
    # A turned tile's tip is this far above its centre, as a share of its side.
    tip = 1 / math.sqrt(2) - (FRONTIER_CORNER / FRONTIER_TILE) * (math.sqrt(2) - 1)
    tile_share = FRONTIER_TILE / FRONTIER_HEIGHT
    stroke_share = FRONTIER_STROKE / FRONTIER_HEIGHT
    space_share = FRONTIER_SPACE / FRONTIER_HEIGHT
    # The ink runs from the tile's tip to the ring's bottom edge, 2R + tip * tile + stroke / 2,
    # and the tile and the stroke are shares of it, so it is solved for first.
    ink_height = 2 * RADIUS / (1 - tile_share * tip - stroke_share / 2)
    tile = tile_share * ink_height
    stroke = stroke_share * ink_height
    space = space_share * ink_height
    corner = FRONTIER_CORNER / FRONTIER_TILE * tile
    end_x, end_y = ring_end(tile, corner, space + stroke / 2)
    top = -RADIUS
    ink_width = 2 * (RADIUS + stroke / 2)
    width, height = ink_width + 2 * MARGIN, ink_height + 2 * MARGIN
    return TEMPLATE.format(
        x=-ink_width / 2 - MARGIN,
        y=top - tip * tile - MARGIN,
        width=width,
        height=height,
        pixel_width=width * SCALE,
        pixel_height=height * SCALE,
        stroke=stroke,
        radius=RADIUS,
        end_x=end_x,
        end_y=end_y,
        mirror_x=-end_x,
        tile=tile,
        tile_x=-tile / 2,
        tile_y=top - tile / 2,
        corner=corner,
        top=top,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    arguments = parser.parse_args()
    arguments.output.write_text(logo())


if __name__ == "__main__":
    main()
