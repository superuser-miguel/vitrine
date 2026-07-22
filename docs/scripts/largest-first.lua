-- Biggest files on top — handy for finding what is eating disk, or the
-- full-resolution original among a pile of downscaled copies.
--
-- Install by copying this file into Vitrine's scripts directory:
--   ~/.var/app/io.github.superuser_miguel.Vitrine/data/vitrine/scripts/
-- It appears in the Sort By menu under "From Scripts". See natural-sort.lua in
-- this directory for what a script can and cannot do.

vitrine.register_sort {
  name = "Largest first",
  key = function(item)
    -- Sort keys always order ascending. `item.size` is a number, so negate it
    -- to put the largest file at the top. Files of equal size keep the view's
    -- existing stable order rather than being reshuffled arbitrarily.
    return -item.size
  end,
}
