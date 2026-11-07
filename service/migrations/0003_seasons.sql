-- Seasons are time windows that never overlap (M7 step 4; architecture v0.59:
-- a season is a time window, not a release). A season holds the instants
-- [starts_at, ends_at); with no two windows overlapping, at most one season is
-- current at any instant, which is what the scheduler and GET /ladder rely on.
-- A GiST exclusion over the range needs no extension (btree_gist is only for
-- scalar columns beside the range).
ALTER TABLE seasons ADD CONSTRAINT seasons_no_overlap
    EXCLUDE USING gist (tstzrange(starts_at, ends_at, '[)') WITH &&);

-- The mode a season plays is a content row's name (`contest`).
ALTER TABLE seasons ADD CONSTRAINT seasons_mode CHECK (mode ~ '^[a-z0-9_]{1,32}$');

CREATE INDEX seasons_starts ON seasons (starts_at DESC);
