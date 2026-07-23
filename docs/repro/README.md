# Reproducers

Minimal, standalone scripts that isolate a behaviour discussed in the write-ups.

## `gridview-bind.py` — GtkGridView's fixed realize buffer

Companion to [*The viewport that lied*](../the-viewport-that-lied.html).

**Finding:** for a large model, `GtkGridView` binds a **fixed 32 rows** of cell
widgets at rest — `32 × columns + 1` — regardless of how few rows are actually
visible, and invariant to window height, cell size, and model size. It scales
only with the column count:

| columns | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|---|
| cells bound | 33 | 65 | 97 | 129 | 161 | 193 | **225** |

`total_bind_events == live_bound`, so these are simultaneous live widgets, not
bind/unbind churn. For a thumbnail grid showing ~3 rows that's ~8–10×
over-realization. Vitrine hit exactly **225** in production because its grid caps
at `MAX_COLUMNS = 7`; the midpoint of that 0–224 band (112) is what a naive
"viewport centre = midpoint of bound cells" heuristic computed — the bug the post
is about.

```sh
python3 gridview-bind.py            # 4 cols -> 129
COLS=7 python3 gridview-bind.py     # 7 cols -> 225 (Vitrine's number)
```

Requires PyGObject + GTK4 and a display; auto-quits after ~1.5 s. Measured on
host **GTK 4.22**. Whether 32 is a floor or a hard cap above 32 *visible* rows is
untested (a normal display can't show that many rows); for any ordinary grid it's
a fixed 32-row floor.
