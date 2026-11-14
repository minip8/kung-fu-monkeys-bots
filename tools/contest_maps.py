#!/usr/bin/env python3
"""Draws the contest's arenas for the ladder's site (service/templates/maps/<arena>.svg).

    tools/contest_maps.py <engine checkout>     # at engine.lock's tag

Reads the engine's content (content/modes.toml for the `contest` row's pool,
content/arenas/*.toml for each member) and writes one inline SVG per arena, which the
portal's templates include (the contest guide, the landing page). The drawings are
generated, never edited: run this again after an engine bump that touches the pool, and
commit what it writes. One unit is one cell; the classes (m-floor, m-wall, m-pit, m-home0,
m-home1, m-tree, m-power, m-spawn0, m-spawn1) are styled by templates/base.html, so the
maps follow the page's light and dark colours.

A tilemap's glyphs (the engine's arenas/06-hilltop.toml): `.` floor, `#` wall, `~` pit, a
digit a spawn point (0-4 the first spawn group, team 0; 5-9 the second), a letter a zone
(`a` zone 0, `b` zone 1). A position (x, y) has the origin at the map's centre, +x east and
+y north; row 0 is the north edge.
"""
import sys
import tomllib
from pathlib import Path


def runs(rows, glyph):
    """The cells holding `glyph` as rectangles (x, y, w, h): horizontal runs, then runs of
    equal span in consecutive rows merged."""
    spans = []
    for y, row in enumerate(rows):
        x = 0
        while x < len(row):
            if row[x] == glyph:
                start = x
                while x < len(row) and row[x] == glyph:
                    x += 1
                spans.append([start, y, x - start, 1])
            else:
                x += 1
    merged = []
    for span in spans:
        for m in merged:
            if m[0] == span[0] and m[2] == span[2] and m[1] + m[3] == span[1]:
                m[3] += 1
                break
        else:
            merged.append(span)
    return merged


def num(value):
    """A coordinate without a trailing .0."""
    text = f"{value:.2f}".rstrip("0").rstrip(".")
    return text or "0"


def describe(name, width, height, trees, pits):
    kinds = {}
    for tree in trees:
        kinds[tree["interval_ticks"]] = kinds.get(tree["interval_ticks"], 0) + 1
    parts = [f"{count} bearing every {beat} turns" for beat, count in sorted(kinds.items())]
    text = f"The {name} arena, {width} by {height} cells: a home zone for each team on the west and east sides, {len(trees)} trees ({', '.join(parts)})"
    if pits:
        text += ", and pits"
    return text + "."


def draw(arena):
    rows = arena["map"]
    height, width = len(rows), len(rows[0])
    if any(len(row) != width for row in rows):
        raise SystemExit(f"{arena['name']}: ragged map")
    cell = arena.get("cell", 1.0)

    def at(pos):
        return pos[0] / cell + width / 2, height / 2 - pos[1] / cell

    trees = [s for s in arena["spawners"] if s.get("tree")]
    powers = [s for s in arena["spawners"] if not s.get("tree")]
    pits = runs(rows, "~")
    out = [
        f'<svg class="map" viewBox="0 0 {width} {height}" role="img" '
        f'aria-label="{describe(arena["name"], width, height, trees, pits)}">',
        f'<rect class="m-floor" width="{width}" height="{height}" rx=".4"/>',
    ]
    grid = "".join(f"M{x} 0V{height}" for x in range(1, width))
    grid += "".join(f"M0 {y}H{width}" for y in range(1, height))
    out.append(f'<path class="m-grid" d="{grid}"/>')
    for x, y, w, h in pits:
        out.append(f'<rect class="m-pit" x="{x}" y="{y}" width="{w}" height="{h}"/>')
    for zone, glyph in enumerate("ab"):
        for x, y, w, h in runs(rows, glyph):
            out.append(f'<rect class="m-home{zone}" x="{x}" y="{y}" width="{w}" height="{h}" rx=".2"/>')
            out.append(
                f'<text class="m-label" x="{num(x + w / 2)}" y="{num(y + h / 2 + 0.25)}">home {zone}</text>'
            )
    for x, y, w, h in runs(rows, "#"):
        out.append(f'<rect class="m-wall" x="{x}" y="{y}" width="{w}" height="{h}"/>')
    for y, row in enumerate(rows):
        for x, glyph in enumerate(row):
            if glyph.isdigit():
                team = 0 if int(glyph) < 5 else 1
                out.append(f'<circle class="m-spawn{team}" cx="{x + 0.5}" cy="{y + 0.5}" r=".17"/>')
    for power in powers:
        x, y = at(power["pos"])
        out.append(
            f'<path class="m-power" d="M{num(x)} {num(y - 0.38)}l.38 .38l-.38 .38l-.38 -.38z"/>'
        )
    for tree in trees:
        x, y = at(tree["pos"])
        radius = {1: 0.38, 2: 0.43, 3: 0.48}.get(tree["max_alive"], 0.4)
        out.append(f'<circle class="m-tree" cx="{num(x)}" cy="{num(y)}" r="{radius}"/>')
        out.append(
            f'<text class="m-beat" x="{num(x)}" y="{num(y + 0.18)}">{tree["interval_ticks"]}</text>'
        )
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    content = Path(sys.argv[1]) / "content"
    modes = tomllib.loads((content / "modes.toml").read_text())["mode"]
    contest = next(m for m in modes if m["name"] == "contest")
    pool = contest.get("arenas") or [contest["arena"]]
    arenas = {}
    for path in sorted((content / "arenas").glob("*.toml")):
        for arena in tomllib.loads(path.read_text()).get("arena", []):
            arenas[arena["name"]] = arena
    out_dir = Path(__file__).resolve().parent.parent / "service" / "templates" / "maps"
    out_dir.mkdir(parents=True, exist_ok=True)
    for name in pool:
        (out_dir / f"{name}.svg").write_text(draw(arenas[name]))
        print(f"wrote {out_dir / (name + '.svg')}")


if __name__ == "__main__":
    main()
