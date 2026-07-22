-- Highest-rated images first, ties broken by natural name; unrated images sink
-- to the bottom together. Shows the two-part trick that most "sort by X, then
-- Y" orders need: a single composite key that encodes both, plus an inversion
-- so a bigger rating sorts earlier.
--
-- Install by copying this file into Vitrine's scripts directory:
--   ~/.var/app/io.github.superuser_miguel.Vitrine/data/vitrine/scripts/
-- It appears in the Sort By menu under "From Scripts". See natural-sort.lua in
-- this directory for what a script can and cannot do.

local function natural_key(name)
  return (name:gsub("%d+", function(digits)
    return string.format("%08d", tonumber(digits))
  end))
end

vitrine.register_sort {
  name = "Best rated first",
  key = function(item)
    -- Ratings run 0..5, and string keys sort ascending, so map the rating to
    -- its complement (a 5-star image becomes "0", an unrated one "5"). The
    -- clamp guards against a stray out-of-range value making the key a
    -- negative number, which would not line up with the single-digit bucket.
    local rating = math.max(0, math.min(5, item.rating))
    local bucket = 5 - rating
    -- Composite: rank bucket first, then natural name to break ties. "\0"
    -- separates the two parts so a name can never reach into the next bucket.
    return tostring(bucket) .. "\0" .. natural_key(item.name:lower())
  end,
}
