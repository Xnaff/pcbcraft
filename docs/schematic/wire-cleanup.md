# Wire cleanup

Wire cleanup redraws selected schematic wires between the endpoints they already
have. It is a command for agents and other MCP clients; the editor has no control
for it yet ([#30](https://github.com/tangency-labs/pcbcraft/issues/30)). PCB tracks
are not affected.

## Commands

| Command | Effect |
| --- | --- |
| `schematic.preview_wire_cleanup` | Read-only. Reports what a cleanup of `items` would change. |
| `schematic.clean_up_wires` | Applies that cleanup at `expected_revision` as one undo step. |

Both take `items` with one to 256 wire IDs and no symbols, labels or markers, and
`sheet` when the project has several sheets. A larger or mixed selection, an unknown
wire and a stale revision are refused without changing the project. So is a
selection in a region so dense that comparing its routes would exceed a fixed work
bound; select fewer wires.

The preview returns `wire_cleanup`:

- `before` and `after`: `bends`, `crossings` and `length_nm` totals of the selected
  wires. A crossing is a place where a selected wire crosses another wire without
  connecting to it.
- `wires`: each wire that would be redrawn, with its `id` and its `before` and
  `after` routes as point lists. Wires that keep their drawing are not listed.

Applying a cleanup that would redraw nothing succeeds and leaves the revision and
undo history unchanged.

## What stays fixed

- Wire endpoints, junctions, wire IDs and every unselected wire. The netlist is
  unchanged.
- Symbols, labels and sheet symbols never move. To keep a wire as drawn, leave it
  out of the selection; there is no separate wire lock.

## How a route is chosen

Each selected wire is compared with a straight run, the two elbows and two-bend
detours through one grid channel, up to eight grid steps beyond the box around its
current route.

A candidate is refused when it enters the box of a symbol and its pin leads, a
sheet symbol or a named-net label, passes over another pin or label anchor, touches
another wire's end or corner, or runs along another wire. A box that already holds
one of the wire's own ends is ignored for that wire.

Among the remaining candidates the lowest cost wins, where one crossing counts as
30 grid steps of length and one bend as 10. A detour around a crossing is therefore
chosen only while it stays short. The current route is kept when no candidate is
better, and equal candidates resolve in a fixed order, so the same project always
gives the same result in the preview, the command and every client.

## Limits

- Routes have at most two bends. A connection that needs more keeps its drawing.
- Label boxes use an estimated name width and ignore an authored text offset.
- Hierarchical label text and symbol field text are not treated as obstacles.
- Crossings that need a symbol to move or a wire to become a label stay; the
  preview's `after.crossings` shows how many remain.
