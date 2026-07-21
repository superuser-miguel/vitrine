-- Natural sort — "img_2" before "img_10", which plain alphabetical gets
-- backwards because it compares "1" against "2" one character at a time.
--
-- Install by copying this file into Vitrine's scripts directory:
--   ~/.var/app/io.github.superuser_miguel.Vitrine/data/vitrine/scripts/
-- It appears in the Sort By menu under "From Scripts".
--
-- ---------------------------------------------------------------------------
-- WHAT A SCRIPT CAN AND CANNOT DO
--
-- A script runs *inside* Vitrine with all of Vitrine's authority. Install
-- scripts you trust, exactly as you would a shell alias or a .bashrc line.
-- The restrictions below are guardrails against mistakes, not a security
-- boundary, and Vitrine does not pretend otherwise:
--
--   * There is no `os`, `io`, `require`, `load` or `dofile`. A script cannot
--     open files, spawn processes or reach the network. File-producing work
--     goes through batch operations, which the host runs.
--   * A key function gets an instruction budget. An accidental
--     `while true do end` errors out and names this file in a toast; it does
--     not hang the app.
--   * Errors are survivable. A syntax error here costs this script only —
--     the others still load.
--
-- The API is versioned: `vitrine.api_version` is 1.
-- ---------------------------------------------------------------------------

-- `key` is called once per image, and its result is cached — so it is fine for
-- it to do real work, but it must be a pure function of the facts it is given.
-- The comparison itself is done natively; you supply a sort *key*, not a
-- comparator.
--
-- The trick: rewrite every run of digits as a fixed-width, zero-padded number,
-- so ordinary text comparison then does the right thing.
--   "img_2.jpg"  ->  "img_00000002.jpg"
--   "img_10.jpg" ->  "img_00000010.jpg"
-- and "00000002" < "00000010" the way you would expect.

local function natural_key(name)
  -- The extra parentheses matter: gsub returns two values (the string and a
  -- count), and a bare `return name:gsub(...)` would return both. The key
  -- function must return exactly one value.
  return (name:gsub("%d+", function(digits)
    return string.format("%08d", tonumber(digits))
  end))
end

vitrine.register_sort {
  name = "Name (natural)",
  key = function(item)
    -- Case-fold so "IMG_2" and "img_2" land together rather than in separate
    -- uppercase/lowercase blocks.
    return natural_key(item.name:lower())
  end,
}

-- A second one, to show that a script may register more than one order and
-- that keys may be numbers as well as strings.
--
-- `item.date_taken` is nil until the background index has enriched that image,
-- so this falls back to the file's modification time. Returning nil would be
-- an error — a sort key has to be a number or a string.
vitrine.register_sort {
  name = "Oldest first",
  key = function(item)
    return item.date_taken or item.mtime
  end,
}

-- Facts available on `item`:
--   name, path, size, mtime, content_type, content_hash,
--   rating, orientation, date_taken (may be nil)
