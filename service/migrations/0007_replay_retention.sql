-- Replay retention (docs/ladder-api.md §10.6, API v1.4): the scheduler's sweep walks the
-- matches that still have a replay, oldest first. Once retention runs only about a day of
-- games has one, so the partial index stays small however long the ladder runs.
CREATE INDEX matches_replay_age ON matches (created_at, id) WHERE replay_key IS NOT NULL;
