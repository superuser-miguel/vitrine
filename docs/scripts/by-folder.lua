-- Sort by folder, then by name within the folder. Useful once a view is
-- flattened across subfolders and you want images from the same directory to
-- stay together instead of interleaving by filename alone.
--
-- Install by copying this file into Vitrine's scripts directory:
--   ~/.var/app/io.github.superuser_miguel.Vitrine/data/vitrine/scripts/
-- It appears in the Sort By menu under "From Scripts". See natural-sort.lua in
-- this directory for what a script can and cannot do.

local function natural_key(s)
  return (s:gsub("%d+", function(digits)
    return string.format("%08d", tonumber(digits))
  end))
end

vitrine.register_sort {
  name = "Folder, then name",
  key = function(item)
    -- The full path already reads "directory then filename" left to right, so
    -- sorting it naturally groups each folder and orders files within it. Note
    -- this uses `item.path`, not `item.name` — the folder is the whole point.
    return natural_key(item.path:lower())
  end,
}
