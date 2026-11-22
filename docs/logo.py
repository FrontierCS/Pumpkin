"""Draws Pumpkin's logo, an open ring with a tile turned 45 degrees in the break at its top.

Run `python3 docs/logo.py --output docs/figures/logo.svg` from the repository's root, which is
also what it does without `--output`. It writes the same bytes each time: an SVG that reads on
GitHub's light and dark pages, as the other figures in `docs/figures/` do.

The mark's proportions are shares of its own height, set by the constants below.

`python3 docs/logo.py --wordmark <file>` writes the logo with the name beside it instead: the mark
24 px high, a 9 px gap, then "Pumpkin" in Instrument Sans Medium at 23 px. The name is drawn as
its outline, so the file needs no font; the page of `pumpkin view` holds the same shapes in its
header (`crates/pumpkin/src/view/page/index.html`), coloured by its style.
"""

import argparse
import math
from pathlib import Path

DEFAULT_OUTPUT = Path(__file__).resolve().parent / "figures" / "logo.svg"

# The ring's radius, to the middle of its stroke. The tile's centre is on the ring at its top.
RADIUS = 50.0

# The mark's measures: the height of its ink, its tile's side and corner radius, its stroke, and
# the space from the tile's corner to the arc's outer edge (the tile's centre is 45.2 units
# right of the arc's centre and as far above it).
MARK_HEIGHT = 74.7
MARK_TILE = 24.0
MARK_CORNER = 5.5
MARK_STROKE = 11.0
MARK_SPACE = math.hypot(45.2, 45.2) - (RADIUS + MARK_STROKE / 2)

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


# The logo with its name, in pixels: the mark's height, the space to the name, the name's size and
# its baseline below the mark's top (a 23 px line centred on the mark, as a browser sets it).
WORDMARK_MARK = 24.0
WORDMARK_GAP = 9.0
WORDMARK_SIZE = 23.0
WORDMARK_BASELINE = 20.0
WORDMARK_MARGIN = 4.0

# "Pumpkin" in Instrument Sans Medium (https://github.com/Instrument/instrument-sans, under the
# SIL Open Font License), as its outline in the font's units, 1000 to its size, y down from the
# baseline: each letter after the one before by its advance, the font's kerning of the pair (-18
# after P, -7 after k) and a letter-spacing of -0.01 of the size. NAME_ADVANCE is the name's width
# and NAME_DESCENT how far its p reaches below the baseline.
NAME_ADVANCE = 4118.0
NAME_DESCENT = 205.0
NAME = (
    "M74 0V-720H379Q457 -720 512 -694Q567 -668 596.5 -620Q626 -572 626 -506Q626 -439 596.5 -390.5Q567 -342 511.5 -315Q456 -288 379 -288H155V-376H374Q445 -376 482.5 -410Q520 -444 520 -505Q520 -567 483 -600Q446 -633 374 -633H180V0ZM872 10Q821 10 782 -11.5Q743 -33 721.5 -71Q700 -109 700 -158V-510H804V-182Q804 -129 830 -103Q856 -77 904 -77Q947 -77 980.5 -97Q1014 -117 1033.5 -152.5Q1053 -188 1053 -235L1066 -113Q1041 -57 989.5 -23.5Q938 10 872 10ZM1057 0V-120H1053V-510H1156V0ZM1292 0V-510H1391V-390H1396V0ZM1636 0V-333Q1636 -385 1612 -409Q1588 -433 1543 -433Q1501 -433 1467.5 -413.5Q1434 -394 1415 -359.5Q1396 -325 1396 -278L1383 -397Q1408 -454 1458.5 -487Q1509 -520 1572 -520Q1648 -520 1693.5 -476Q1739 -432 1739 -360V0ZM1980 0V-333Q1980 -385 1955.5 -409Q1931 -433 1886 -433Q1845 -433 1811.5 -413.5Q1778 -394 1758.5 -359.5Q1739 -325 1739 -278L1718 -397Q1744 -454 1796.5 -487Q1849 -520 1914 -520Q1991 -520 2037 -475.5Q2083 -431 2083 -357V0ZM2485 9Q2413 9 2365.5 -25.5Q2318 -60 2303 -122L2317 -124V205H2213V-510H2312V-393L2302 -396Q2318 -454 2370 -487Q2422 -520 2492 -520Q2561 -520 2611.5 -487Q2662 -454 2689.5 -395Q2717 -336 2717 -257Q2717 -177 2688 -117Q2659 -57 2607 -24Q2555 9 2485 9ZM2463 -77Q2531 -77 2571 -125Q2611 -173 2611 -257Q2611 -340 2571 -386.5Q2531 -433 2462 -433Q2394 -433 2353.5 -385.5Q2313 -338 2313 -254Q2313 -172 2353.5 -124.5Q2394 -77 2463 -77ZM2816 0V-733H2920V0ZM3147 0 2896 -265 3139 -510H3270L2994 -242L2999 -289L3280 0ZM3345 0V-510H3449V0ZM3340 -610V-727H3455V-610ZM3585 0V-510H3684V-390H3689V0ZM3947 0V-328Q3947 -381 3920.5 -407Q3894 -433 3842 -433Q3797 -433 3762.5 -413Q3728 -393 3708.5 -358Q3689 -323 3689 -275L3676 -397Q3701 -454 3753 -487Q3805 -520 3874 -520Q3956 -520 4003 -474Q4050 -428 4050 -352V0Z"
)

WORDMARK_TEMPLATE = """\
<!-- python3 docs/logo.py --wordmark writes this file: edit docs/logo.py, not this -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x:.2f} {y:.2f} {width:.2f} {height:.2f}" \
width="{width:.2f}" height="{height:.2f}" role="img" aria-labelledby="title desc">
<title id="title">Pumpkin's logo, with its name</title>
<desc id="desc">An open ring with an orange tile, turned 45 degrees, in the break at its top, \
and the name Pumpkin at its right.</desc>
<style>
:root{{--bg:#ffffff;--ink:#1f2328}}
@media (prefers-color-scheme:dark){{:root{{--bg:#0d1117;--ink:#e6edf3}}}}
.bg{{fill:var(--bg)}}
.ring{{fill:none;stroke:var(--ink);stroke-width:{stroke:.2f};stroke-linecap:round}}
.tile{{fill:#e8710a}}
.name{{fill:var(--ink)}}
</style>
<rect class="bg" x="{x:.2f}" y="{y:.2f}" width="{width:.2f}" height="{height:.2f}"/>
{shapes}
</svg>
"""

# The mark and the name, for a style to colour by their classes `ring`, `tile` and `name`.
SHAPES = """\
<g transform="translate({mark_x:.3f} {mark_y:.3f}) scale({mark_scale:.5f})">\
<path class="ring" d="M{end_x:.2f} {end_y:.2f} A{radius:g} {radius:g} 0 1 1 {mirror_x:.2f} \
{end_y:.2f}"/>\
<rect class="tile" x="{tile_x:.2f}" y="{tile_y:.2f}" width="{tile:.2f}" height="{tile:.2f}" \
rx="{corner:.2f}" transform="rotate(45 0 {top:g})"/></g>\
<path class="name" transform="translate({name_x:.3f} {baseline:.3f}) scale({name_scale:g})" \
d="{name}"/>"""


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


def mark():
    """The mark's measures: its ink's width and height, its stroke, and its shapes' numbers."""
    # A turned tile's tip is this far above its centre, as a share of its side.
    tip = 1 / math.sqrt(2) - (MARK_CORNER / MARK_TILE) * (math.sqrt(2) - 1)
    tile_share = MARK_TILE / MARK_HEIGHT
    stroke_share = MARK_STROKE / MARK_HEIGHT
    space_share = MARK_SPACE / MARK_HEIGHT
    # The ink runs from the tile's tip to the ring's bottom edge, 2R + tip * tile + stroke / 2,
    # and the tile and the stroke are shares of it, so it is solved for first.
    ink_height = 2 * RADIUS / (1 - tile_share * tip - stroke_share / 2)
    tile = tile_share * ink_height
    stroke = stroke_share * ink_height
    space = space_share * ink_height
    corner = MARK_CORNER / MARK_TILE * tile
    end_x, end_y = ring_end(tile, corner, space + stroke / 2)
    top = -RADIUS
    return {
        "ink_width": 2 * (RADIUS + stroke / 2),
        "ink_height": ink_height,
        "ink_top": top - tip * tile,
        "stroke": stroke,
        "shapes": {
            "radius": RADIUS,
            "end_x": end_x,
            "end_y": end_y,
            "mirror_x": -end_x,
            "tile": tile,
            "tile_x": -tile / 2,
            "tile_y": top - tile / 2,
            "corner": corner,
            "top": top,
        },
    }


def logo():
    """The logo's SVG."""
    drawn = mark()
    width, height = drawn["ink_width"] + 2 * MARGIN, drawn["ink_height"] + 2 * MARGIN
    return TEMPLATE.format(
        x=-drawn["ink_width"] / 2 - MARGIN,
        y=drawn["ink_top"] - MARGIN,
        width=width,
        height=height,
        pixel_width=width * SCALE,
        pixel_height=height * SCALE,
        stroke=drawn["stroke"],
        **drawn["shapes"],
    )


def wordmark():
    """The SVG of the logo with its name."""
    drawn = mark()
    scale = WORDMARK_MARK / drawn["ink_height"]
    mark_width = drawn["ink_width"] * scale
    name_scale = WORDMARK_SIZE / 1000
    shapes = SHAPES.format(
        mark_x=drawn["ink_width"] / 2 * scale,
        mark_y=-drawn["ink_top"] * scale,
        mark_scale=scale,
        name_x=mark_width + WORDMARK_GAP,
        baseline=WORDMARK_BASELINE,
        name_scale=name_scale,
        name=NAME,
        **drawn["shapes"],
    )
    return WORDMARK_TEMPLATE.format(
        x=-WORDMARK_MARGIN,
        y=-WORDMARK_MARGIN,
        width=mark_width + WORDMARK_GAP + NAME_ADVANCE * name_scale + 2 * WORDMARK_MARGIN,
        height=WORDMARK_BASELINE + NAME_DESCENT * name_scale + 2 * WORDMARK_MARGIN,
        stroke=drawn["stroke"],
        shapes=shapes,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--wordmark", type=Path, help="write the logo with its name here instead")
    arguments = parser.parse_args()
    if arguments.wordmark:
        arguments.wordmark.write_text(wordmark())
    else:
        arguments.output.write_text(logo())


if __name__ == "__main__":
    main()
