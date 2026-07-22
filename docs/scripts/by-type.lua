-- Group by file type, then natural name within each group — all the PNGs
-- together, then all the JPEGs, and so on, each group in "img_2 before img_10"
-- order rather than plain alphabetical.
--
-- Install by copying this file into Vitrine's scripts directory:
--   ~/.var/app/io.github.superuser_miguel.Vitrine/data/vitrine/scripts/
-- It appears in the Sort By menu under "From Scripts". See natural-sort.lua in
-- this directory for what a script can and cannot do; the same rules apply.

local function natural_key(name)
  -- Rewrite each run of digits as a fixed-width, zero-padded number, so a
  -- plain text comparison then orders numbers the way a human would.
  return (name:gsub("%d+", function(digits)
    return string.format("%08d", tonumber(digits))
  end))
end

vitrine.register_sort {
  name = "Type, then name",
  key = function(item)
    -- A single composite string key: the content type, then the natural name.
    -- The "\0" separator sorts below every real character, so a short type
    -- ("image/gif") can never bleed into the next group's names.
    return item.content_type .. "\0" .. natural_key(item.name:lower())
  end,
}
