-- The CLI's login (docs/ladder-api.md §5.1, API v1.3, M7 step 5): a one-time code that the
-- browser carries to `kfm login`'s local listener and the CLI exchanges for the token the
-- confirmation created, so the token's secret never rides in a browser URL.
--
-- The code is 128 random bits; the row is keyed by its SHA-256, so a dump holds no usable
-- code. The token's secret is kept only sealed under the code (XOR with a SHA-512 of it,
-- domain separated from the key's hash), so the row alone unlocks nothing either; the
-- exchange checks the unsealed secret against the token's stored hash. A row lives 5
-- minutes and is deleted by its exchange; an expired one is deleted and its token revoked.
CREATE TABLE cli_codes (
    hash       bytea       PRIMARY KEY CHECK (octet_length(hash) = 32),
    token_id   uuid        NOT NULL REFERENCES tokens (id) ON DELETE CASCADE,
    sealed     bytea       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL
);
CREATE INDEX cli_codes_expires ON cli_codes (expires_at);
