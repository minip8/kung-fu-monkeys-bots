-- Seasons, submissions, the active bot, the job queue, matches and ratings
-- (architecture v0.63). Created in step 3a so the schema is whole; written to
-- from step 3b (submissions, jobs) and step 4 (matches, ratings).

-- A season is a time window (v0.59): the ranking freezes at ends_at; `mode`
-- names the content row the season plays (`contest`).
CREATE TABLE seasons (
    id         uuid        PRIMARY KEY,
    name       text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 64),
    starts_at  timestamptz NOT NULL,
    ends_at    timestamptz NOT NULL,
    mode       text        NOT NULL DEFAULT 'contest',
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT seasons_name UNIQUE (name),
    CHECK (ends_at > starts_at)
);

-- An immutable submission: the source archive, the built module and its
-- hash, the build log, the validation result. Object keys are
-- sources/<id>.zip, modules/<id>.wasm and logs/<id>.txt; the keys are NULL
-- once pruned (sources and logs) or before they exist.
CREATE TABLE submissions (
    id           uuid        PRIMARY KEY,
    team_id      uuid        NOT NULL REFERENCES teams (id),
    submitted_by uuid        REFERENCES users (id) ON DELETE SET NULL,
    manifest     text        NOT NULL,
    source_key   text,
    module_key   text,
    module_hash  bytea       CHECK (module_hash IS NULL OR octet_length(module_hash) = 32),
    log_key      text,
    state        text        NOT NULL DEFAULT 'queued'
                             CHECK (state IN ('queued', 'building', 'failed', 'rejected', 'ready')),
    error        text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    -- The target of active_bots' composite key: a team activates only its own.
    CONSTRAINT submissions_id_team UNIQUE (id, team_id)
);
CREATE INDEX submissions_team_created ON submissions (team_id, created_at DESC);
CREATE INDEX submissions_state ON submissions (state) WHERE state IN ('queued', 'building');

CREATE TABLE active_bots (
    team_id       uuid        PRIMARY KEY REFERENCES teams (id),
    submission_id uuid        NOT NULL,
    activated_by  uuid        REFERENCES users (id) ON DELETE SET NULL,
    activated_at  timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (submission_id, team_id) REFERENCES submissions (id, team_id)
);

-- The queue: a worker claims the oldest runnable job of its kind with
-- UPDATE ... WHERE id = (SELECT ... FOR UPDATE SKIP LOCKED), heartbeats
-- while it runs, and a job whose heartbeat goes stale is re-queued until
-- attempts reaches max_attempts.
CREATE TABLE jobs (
    id           uuid        PRIMARY KEY,
    kind         text        NOT NULL CHECK (kind IN ('build', 'validate', 'match')),
    payload      jsonb       NOT NULL DEFAULT '{}'::jsonb,
    state        text        NOT NULL DEFAULT 'queued'
                             CHECK (state IN ('queued', 'running', 'done', 'failed')),
    attempts     integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts integer     NOT NULL DEFAULT 3 CHECK (max_attempts >= 1),
    run_after    timestamptz NOT NULL DEFAULT now(),
    claimed_by   text,
    claimed_at   timestamptz,
    heartbeat_at timestamptz,
    finished_at  timestamptz,
    error        text,
    created_at   timestamptz NOT NULL DEFAULT now()
);
-- The claim (queued jobs of a kind, oldest runnable first) and the stale scan
-- (running jobs by heartbeat) both start from (state, kind).
CREATE INDEX jobs_state_kind ON jobs (state, kind, run_after, id);
CREATE INDEX jobs_running_heartbeat ON jobs (heartbeat_at) WHERE state = 'running';

-- One two-sided kfm_arena run (--team A --team B --rotate). The seed is
-- chosen by the scheduler in [0, 2^63) so it fits a bigint exactly; the
-- release tag and content hash make a disputed result re-runnable bit for
-- bit; result is the parsed match object of the engine's docs/arena-result.md.
CREATE TABLE matches (
    id           uuid        PRIMARY KEY,
    season_id    uuid        REFERENCES seasons (id),
    job_id       uuid        REFERENCES jobs (id) ON DELETE SET NULL,
    seed         bigint      NOT NULL CHECK (seed >= 0),
    engine_tag   text        NOT NULL,
    content_hash text        NOT NULL CHECK (content_hash ~ '^[0-9a-f]{8}$'),
    team_a       uuid        NOT NULL REFERENCES teams (id),
    submission_a uuid        NOT NULL REFERENCES submissions (id),
    team_b       uuid        NOT NULL REFERENCES teams (id),
    submission_b uuid        NOT NULL REFERENCES submissions (id),
    winner_team  uuid        REFERENCES teams (id),
    replay_key   text,
    result       jsonb       NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    CHECK (team_a <> team_b)
);
CREATE INDEX matches_team_a ON matches (team_a, created_at DESC);
CREATE INDEX matches_team_b ON matches (team_b, created_at DESC);
CREATE INDEX matches_season ON matches (season_id, created_at DESC);
-- A retained module is never pruned while a match used it.
CREATE INDEX matches_submission_a ON matches (submission_a);
CREATE INDEX matches_submission_b ON matches (submission_b);

-- Glicko-2 per team and season (a new season starts afresh), in Glicko-2's
-- own scale (mu around 0, phi the deviation, sigma the volatility).
CREATE TABLE ratings (
    season_id  uuid             NOT NULL REFERENCES seasons (id),
    team_id    uuid             NOT NULL REFERENCES teams (id),
    mu         double precision NOT NULL,
    phi        double precision NOT NULL CHECK (phi > 0),
    sigma      double precision NOT NULL CHECK (sigma > 0),
    games      integer          NOT NULL DEFAULT 0 CHECK (games >= 0),
    updated_at timestamptz      NOT NULL DEFAULT now(),
    CONSTRAINT ratings_pkey PRIMARY KEY (season_id, team_id)
);
CREATE INDEX ratings_board ON ratings (season_id, mu DESC);
