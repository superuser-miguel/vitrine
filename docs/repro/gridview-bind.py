#!/usr/bin/env python3
"""
GtkGridView bind probe — how many cells does it realize for a large model?

Companion to the blog post "The viewport that lied" (docs/the-viewport-that-lied.html).

Question: with a 10,000-item model but only ~10 cells on screen, how many cell
widgets does GtkGridView actually bind at rest (no scrolling)?

Answer (host GTK 4.22): a fixed 32 rows — `32 * columns + 1` cells — held
simultaneously, invariant to window height, cell size, and model size. It scales
only with the column count:

    cols   1   2   3   4   5   6   7
    bound 33  65  97 129 161 193 225        (= 32*cols + 1)

So GtkGridView realizes a fixed ~32-row block regardless of how few rows are
visible — ~8-10x over-realization for a thumbnail grid. (7 columns -> 225 is the
number Vitrine hit in production, because its grid caps at MAX_COLUMNS = 7.)

Metrics printed:
  live_bound         cells currently bound  (bind count - unbind count)
  total_bind_events  cumulative binds — equal to live_bound here, so these are
                     simultaneous live widgets, not bind/unbind churn
  live_range         the position span of the currently-bound cells

Run (requires PyGObject + GTK4 and a display; auto-quits after ~1.5s):
    python3 gridview-bind.py
    COLS=7 python3 gridview-bind.py            # -> 225, Vitrine's number
    WIN_H=1000 CELL=140 python3 gridview-bind.py

Caveat: on a normal display you can't get a viewport taller than 32 rows, so
whether 32 is a floor or a hard cap above 32 visible rows is untested. For any
ordinary thumbnail grid (a few visible rows) it is a fixed 32-row floor.
"""
import os
import gi
gi.require_version("Gtk", "4.0")
from gi.repository import Gtk, Gio, GLib

N_ITEMS = int(os.environ.get("N_ITEMS", 10000))
CELL = int(os.environ.get("CELL", 220))   # px, ~a default thumbnail
COLS = int(os.environ.get("COLS", 4))
WIN_W = int(os.environ.get("WIN_W", 1100))
WIN_H = int(os.environ.get("WIN_H", 720))

live = 0            # currently bound (bind - unbind)
total_binds = 0     # cumulative bind events
live_pos = set()    # positions currently bound

model = Gtk.StringList()
for i in range(N_ITEMS):
    model.append(str(i))

factory = Gtk.SignalListItemFactory()

def on_setup(_f, li):
    frame = Gtk.Frame()
    frame.set_size_request(CELL, CELL)
    frame.set_child(Gtk.Label())
    li.set_child(frame)

def on_bind(_f, li):
    global live, total_binds
    live += 1
    total_binds += 1
    live_pos.add(li.get_position())
    li.get_child().get_child().set_text(li.get_item().get_string())

def on_unbind(_f, li):
    global live
    live -= 1
    live_pos.discard(li.get_position())

factory.connect("setup", on_setup)
factory.connect("bind", on_bind)
factory.connect("unbind", on_unbind)

grid = Gtk.GridView(model=Gtk.NoSelection(model=model), factory=factory)
grid.set_min_columns(COLS)
grid.set_max_columns(COLS)

scroller = Gtk.ScrolledWindow()
scroller.set_child(grid)

def rng(s):
    return f"[{min(s)}..{max(s)}]" if s else "[]"

def report(app, label):
    print(f"REPRO {label:8s}  viewport={scroller.get_width()}x{scroller.get_height()}  "
          f"live_bound={live:<4d} live_range={rng(live_pos):12s}  "
          f"total_bind_events={total_binds}")

def on_activate(app):
    win = Gtk.ApplicationWindow(application=app)
    win.set_default_size(WIN_W, WIN_H)
    win.set_child(scroller)
    win.present()
    # first reading, then a second to confirm it has settled (no growth at rest)
    GLib.timeout_add(500, lambda: (report(app, "@500ms"), False)[1])
    GLib.timeout_add(1500, lambda: (report(app, "@1500ms"), app.quit(), False)[2])

visible_rows = max(1, WIN_H // CELL)
print(f"REPRO config  N_ITEMS={N_ITEMS} cell={CELL}px cols={COLS} window={WIN_W}x{WIN_H} "
      f"(~{visible_rows * COLS} cells expected on screen; predicting {32 * COLS + 1} bound)")

app = Gtk.Application(application_id="org.test.gridbind",
                     flags=Gio.ApplicationFlags.NON_UNIQUE)
app.connect("activate", on_activate)
app.run(None)
