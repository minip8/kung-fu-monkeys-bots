-- What a match job records (M7 step 4). One `match` job is one kfm_arena run
-- of two games (--matches 2 --rotate: the second swaps the sides), and each
-- game is one `matches` row: `game` is kfm_arena's match index k, and
-- (job_id, game) is unique, so a retried job never records a game twice.
-- Side A is the pair's higher-ranked team when scheduled (`team_a`, entrant
-- 0); `side_a` is the mode's team index it played in this game (0 or 1),
-- side B the other. The per-side figures are the teams[] entry of the
-- engine's docs/arena-result.md; `faults_*` lists the side's module slots
-- that trapped, ran over budget, returned a bad value or died (each
-- {"slot","traps","over","bad","dead"}): a fault in a ranked match is
-- recorded, never a rejection. `delta_*` is each side's rating change, on
-- Glicko's scale, from this game.
ALTER TABLE matches
    ADD COLUMN round      integer CHECK (round IS NULL OR round >= 1),
    ADD COLUMN game       smallint NOT NULL DEFAULT 0 CHECK (game >= 0),
    ADD COLUMN side_a     smallint NOT NULL DEFAULT 0 CHECK (side_a IN (0, 1)),
    ADD COLUMN ending     text NOT NULL DEFAULT 'ended' CHECK (ending IN ('ended', 'cap')),
    ADD COLUMN ticks      integer NOT NULL DEFAULT 0 CHECK (ticks >= 0),
    ADD COLUMN build_hash text CHECK (build_hash IS NULL OR build_hash ~ '^[0-9a-f]{8}$'),
    ADD COLUMN score_a    integer NOT NULL DEFAULT 0,
    ADD COLUMN kills_a    integer NOT NULL DEFAULT 0 CHECK (kills_a >= 0),
    ADD COLUMN deaths_a   integer NOT NULL DEFAULT 0 CHECK (deaths_a >= 0),
    ADD COLUMN faults_a   jsonb NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN delta_a    double precision NOT NULL DEFAULT 0,
    ADD COLUMN score_b    integer NOT NULL DEFAULT 0,
    ADD COLUMN kills_b    integer NOT NULL DEFAULT 0 CHECK (kills_b >= 0),
    ADD COLUMN deaths_b   integer NOT NULL DEFAULT 0 CHECK (deaths_b >= 0),
    ADD COLUMN faults_b   jsonb NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN delta_b    double precision NOT NULL DEFAULT 0,
    ADD CONSTRAINT matches_job_game UNIQUE (job_id, game),
    ADD CONSTRAINT matches_winner_side CHECK (winner_team IS NULL OR winner_team IN (team_a, team_b));
CREATE INDEX matches_created ON matches (created_at DESC, id DESC);

-- A team's record in a season beside its rating, and the engine release of
-- its last rated game: when the service's release differs, the deviation is
-- widened before the next game (architecture v0.59).
ALTER TABLE ratings
    ADD COLUMN wins       integer NOT NULL DEFAULT 0 CHECK (wins >= 0),
    ADD COLUMN draws      integer NOT NULL DEFAULT 0 CHECK (draws >= 0),
    ADD COLUMN losses     integer NOT NULL DEFAULT 0 CHECK (losses >= 0),
    ADD COLUMN engine_tag text;

-- Ranked games a submission played, and how many of them had a fault in one
-- of its slots (a trap, an over-budget tick, a bad return, a dead module).
ALTER TABLE submissions
    ADD COLUMN ranked_games  integer NOT NULL DEFAULT 0 CHECK (ranked_games >= 0),
    ADD COLUMN ranked_faults integer NOT NULL DEFAULT 0 CHECK (ranked_faults >= 0);
