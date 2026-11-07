-- The scheduler's rounds (M7 step 4): one row per round a scheduler tick
-- made in a season, numbered from 1 within the season, so a round number on
-- a match job and a match row stays meaningful across restarts. A round
-- with no new pair (every pair still busy) is recorded too: it keeps the
-- cadence.
CREATE TABLE rounds (
    season_id  uuid        NOT NULL REFERENCES seasons (id),
    number     integer     NOT NULL CHECK (number >= 1),
    teams      integer     NOT NULL CHECK (teams >= 2),
    pairs      integer     NOT NULL CHECK (pairs >= 0),
    busy       integer     NOT NULL DEFAULT 0 CHECK (busy >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT rounds_pkey PRIMARY KEY (season_id, number)
);

-- The scheduler skips a pair whose match is still queued or running.
CREATE INDEX jobs_match_live ON jobs (kind) WHERE kind = 'match' AND state IN ('queued', 'running');
