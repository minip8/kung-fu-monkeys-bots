# Kung Fu Monkeys ladder API v1.7

Status: normative, append-only from v1.0 (2026-11-07, M7 step 3a, engine architecture v0.63);
v1.1 (2026-11-07, M7 step 3b) serves submissions; v1.2 (2026-11-07, M7 step 4) serves seasons,
the scheduler's matches, the ratings and the ladder (§10); v1.3 (2026-11-07, M7 step 5) serves
the `kfm` CLI's login (§5.1); v1.4 (2026-11-07) keeps replays for 24 hours (§10.6), limits
the routes that presign per client (§2) and serves the privacy page (§8); v1.5 (2026-11-11,
engine architecture v0.78) holds an account to one team, an operator's excepted (§7); v1.6
(2026-11-13, engine architecture v0.83) plays a pair as one game on an arena and sides drawn
from its seed, and names a game's arena (§10.3, §10.6); v1.7 (2026-11-14) is the site: a
landing page, the contest guide and the games as a page (§8), with no JSON route changed.
The design and its rationale are the engine's `docs/architecture.md` §8 "The ladder" and §11
"M7 step 3 design". This file is the HTTP surface shared by the service (`service/`, the
`kfm-ladder` binary), its portal and the `kfm` command line tool; change it here first. Every
route below is served as of v1.7.

## 1. Transport and conventions

- HTTPS in production (`https://bots.kfm.minip8.com`, Caddy in front of the service); plain
  HTTP on `127.0.0.1` in development. Every path below is relative to that origin.
- Requests with a body send `Content-Type: application/json` (the portal's forms excepted, §8);
  a body that is not JSON, or JSON without a required key or with a key of the wrong type, is
  `bad_request`. Unknown keys in a request are ignored.
- Responses are JSON objects (`Content-Type: application/json`), UTF-8, except the portal's
  HTML pages, the redirects of §5 and §8, and `204 No Content`.
- *id*: a UUID as a lower-case hyphenated string (`"0190f0f0-1c2d-7a00-8000-0123456789ab"`).
  Ids are UUID v7: unguessable enough to be public, time ordered, never reused. A path segment
  that is not a UUID names nothing: `not_found`.
- *time*: an RFC 3339 timestamp in UTC (`"2026-11-07T09:28:09.266849Z"`).
- *scope*: `"submit"` or `"read"` (§3.3).
- "Present" says when a key appears. A key that is not present is absent, never `null`; a key
  marked *or null* is always present and may be `null`.

## 2. Errors

Every error is its HTTP status with the body

```json
{"error":"<code>","message":"<a sentence for a person>"}
```

and exactly those two keys. `error` is the contract; `message` is for display and may change.
The service never puts internals (SQL, a provider's reply, a path) in either. Codes:

| Code | Status | Meaning |
|---|---|---|
| `bad_request` | 400 | The body or query is malformed (not JSON, a missing or mistyped key; an upload that is not `multipart/form-data`). |
| `bad_manifest` | 400 | A submission's `manifest.toml` is refused (§9.1); the message names what is wrong (v1.1). |
| `bad_source` | 400 | A submission's `source.zip` is refused (§9.2), or a part is missing; the message names the first violation (v1.1). |
| `invalid_name` | 400 | A token or team name breaks its rule (§6, §7). |
| `invalid_scope` | 400 | A scope other than `submit` or `read`. |
| `oauth_state` | 400 | A sign-in callback without a valid state: expired, started elsewhere, or tampered with (§5). |
| `unauthenticated` | 401 | The route needs a caller and the request has no session cookie and no bearer, or the session expired. |
| `invalid_token` | 401 | The bearer is malformed, unknown or revoked. |
| `session_required` | 403 | The route takes a portal session only, and the request came with a token. |
| `insufficient_scope` | 403 | The token's scope does not cover the route. |
| `bad_csrf` | 403 | A session-authenticated write without the session's CSRF token, or with another (§3.2). |
| `forbidden` | 403 | The caller may not do this (not a team owner, not the member being removed). |
| `oauth_denied` | 403 | The provider did not grant the sign-in (the person declined). |
| `not_found` | 404 | No such route, or nothing by that id visible to the caller. |
| `unknown_user` | 404 | No user has the id or name given. |
| `method_not_allowed` | 405 | The route exists, the method does not. |
| `name_taken` | 409 | A team of that name exists (names compare ignoring case). |
| `already_member` | 409 | The user is already a member of the team. |
| `last_owner` | 409 | The team's last owner cannot leave or be removed. |
| `ambiguous_user` | 409 | Several users have that display name; name the user by id. |
| `already_linked` | 409 | The signed-in account already has another identity of this provider. |
| `limit_reached` | 409 | A cap was reached: 25 live tokens per user, 32 members per team. |
| `not_ready` | 409 | Only a `ready` submission can be made active (v1.1). |
| `invalid_code` | 400 | A CLI login code that is malformed, unknown, already exchanged or expired (v1.3, §5.1). |
| `already_in_team` | 409 | The account creating a team, or the user being added to one, is in a team already (v1.5, §7). |
| `too_large` | 413 | An upload is over its cap: 4 MiB of `source.zip` (v1.1). |
| `rate_limited` | 429 | A team submits at most once a minute (v1.1); a client asked for presigned links too often (v1.4, below). |
| `oauth_failed` | 502 | The provider's reply could not be obtained or verified (token exchange, profile, id token). |
| `internal` | 500 | A failure on the service's side; logged, never detailed. |

The codes are append-only (§11): a client must treat an unknown code by its status.

For `bad_manifest` and `bad_source` the `message` is the code's sentence followed by the
detail, as in `The source archive is refused: "../x.c" contains a .. component.`; it is still
display text, not a contract.

**The presigning limit (v1.4).** The routes that hand out presigned object store URLs,
`GET /matches/{m}` (§10.6, its JSON and its HTML page alike) and `GET /submissions/{s}`
(§9.5), share one token bucket per client IP: `KFM_LADDER_PRESIGN_PER_MINUTE` requests a minute
(default 60; 0 turns the limit off), refilled continuously at that rate, so a burst of 60 is
allowed and then one a second. A request without a token answers `429` with the error shape
(`{"error":"rate_limited","message":…}`, JSON even where the route would answer HTML) and a
`Retry-After` header: the whole seconds until the next token. The limit is checked before
authentication, so a refused request was not authenticated either. The client IP is the TCP
peer, or the first `X-Forwarded-For` entry when the service runs behind a proxy it trusts
(`KFM_LADDER_TRUST_PROXY=1`; Caddy sets the header in production); IPv6 clients share a bucket
per /64. The buckets are in the API process's memory: a restart refills them. A client
polling a submission every 3 seconds, as the CLI does, stays well under the limit.

## 3. Authentication

A caller is a user, authenticated one of two ways.

### 3.1 Portal sessions

Signing in (§5) sets the cookie `kfm_session`: 128 random bits, signed with HMAC-SHA256 under
the service's session key, `Path=/; HttpOnly; SameSite=Lax`, plus `Secure` whenever the
service's public URL is https. The service keeps the session's row keyed by the SHA-256 of
those bits, so its database holds no live session. A session ends after **30 days without a
request** (each authenticated request records itself), at sign-out, or when the same browser
signs in as another account. The cookie's own `Max-Age` is longer (400 days) and carries no
meaning: the service decides expiry.

### 3.2 CSRF

Every session has a CSRF token (`GET /me` returns it as `csrf`; the portal puts it in every
form). A write (any `POST` or `DELETE`) authenticated by the session cookie must carry it, as the
`X-CSRF-Token` header of a JSON request or the `csrf` field of a portal form; otherwise it is
`bad_csrf` and changes nothing. Bearer-authenticated requests need none (a browser never sends
a bearer by itself). `GET` routes change nothing (the sign-in routes of §5 are protected by
their state cookie instead).

### 3.3 API tokens

A token is created on the portal (§6) or by the CLI's login (§5.1), shown once and never
again, and sent as

```
Authorization: Bearer kfm_<43 characters>
```

The secret is `kfm_` followed by 256 random bits in unpadded base64url (43 characters; 47 in
all). The service stores only the SHA-256 of the whole string and records each use. Scopes:

- `read`: read the caller's own data and everything public.
- `submit`: everything `read` allows, plus uploading submissions and setting the active one
  (§9).

A request with an `Authorization` header is judged by the token alone: a bad one is
`invalid_token` even if a valid session cookie came with it. Without the header, the session
cookie decides; without either, a route that needs a caller answers `unauthenticated`.

### 3.4 Who may call what

| Auth column | Means |
|---|---|
| none | No credentials needed (sent ones are still checked). |
| session | A portal session only; a token is `session_required`. Writes need the CSRF token. |
| any | A session (writes with the CSRF token) or a token of either scope. |
| submit | A session (writes with the CSRF token) or a `submit` token; a `read` token is `insufficient_scope`. |

## 4. Health

`GET /healthz` (auth: none). `200 {"ok":true,"db":true}` once a `SELECT 1` answers within 2
seconds; otherwise `503 {"ok":false,"db":false}`. Deployment checks poll it.

## 5. Identity

Identity is a hosted OAuth provider, never a password: GitHub (OAuth 2) or Google (OpenID
Connect). Both use the authorization code flow with PKCE (S256). An account is keyed by the
provider's **subject**: GitHub's numeric user id, Google's `sub`. The email and the name a
provider reports are display data on the identity, refreshed at every sign-in, never a key (an
email changes, and two providers may report the same one: those are two accounts until one
links the other).

| Method and path | Auth | What |
|---|---|---|
| `GET /auth/github` | none | Starts a GitHub sign-in: `303` to GitHub's authorize URL (scopes `read:user user:email`). |
| `GET /auth/google` | none | Starts a Google sign-in: `303` to Google's authorize URL (scopes `openid email profile`). |
| `GET /auth/github/callback` | none (state cookie) | The provider's redirect target. |
| `GET /auth/google/callback` | none (state cookie) | The provider's redirect target. |
| `POST /auth/logout` | session (CSRF) | Ends the session. |
| `GET /auth/cli` | none (a session to confirm) | The CLI login's page (v1.3, §5.1). |
| `POST /auth/cli` | session (CSRF, form) | Confirms or cancels a CLI login (v1.3, §5.1). |
| `POST /auth/cli/exchange` | none (the code) | A CLI login's code for its token (v1.3, §5.1). |

A start sets the cookie `kfm_oauth` (`Path=/auth`, `HttpOnly`, `SameSite=Lax`, `Secure` under
https, 10 minutes): the provider, the `state` value, the PKCE verifier and (Google) the nonce,
signed with the session key, and the page to come back to (v1.3: a start may take `?next=`,
kept only when it is the CLI login's page, a path starting `/auth/cli?` of printable ASCII
without `//` or a backslash, at most 512 bytes; any other `next` is ignored). The callback
requires that cookie, unexpired, for the same
provider, with the same `state` as the query; it always clears it. Then the code is exchanged;
GitHub's profile comes from `GET /user` (and `GET /user/emails` when the email is hidden: the
primary verified one), Google's from the id token, verified against Google's published keys,
issuer, audience, expiry and nonce. The account rule:

1. The identity (provider, subject) exists: sign in as its user (from another account's
   session too: the browser switches accounts).
2. Otherwise, a session is signed in: link the identity to that user (`already_linked` if the
   user has another identity of the same provider). The session is kept.
3. Otherwise: create a user named after the identity (GitHub's login, Google's name; at most
   64 characters, `player` if empty) and sign in.

Creating an account creates no team (§7). A callback always answers `303`: to the start's
`next` after a sign-in or a link when it had one (v1.3), else to `/` after a sign-in and to
`/?linked=<provider>` after a link, to `/?error=<code>` on failure (`oauth_state`,
`oauth_denied`, `oauth_failed`, `already_linked`), with the session cookie set when a new
session starts.

`POST /auth/logout` takes the CSRF token as the `X-CSRF-Token` header or a urlencoded `csrf`
field, deletes the session, clears the cookie and answers `303` to `/`. Without a session it
just clears the cookie.

### 5.1 The CLI's login (v1.3)

`kfm login` gets a token through the browser with a local redirect, the flow of `gh auth login`
and of OAuth for native apps (RFC 8252 §7.3), with the ladder's own sign-in in front:

1. The CLI listens on `127.0.0.1` on an ephemeral port, makes a `state` of at least 128 random
   bits, and opens (and prints, for a machine without a browser)
   `<origin>/auth/cli?port=<port>&state=<state>&host=<host name>`.
2. `GET /auth/cli` checks the query: `port` a number from 1024 to 65535, `state` 8 to 128
   characters of `A-Z a-z 0-9 - _ . ~`, `host` optional (control characters dropped, whitespace
   collapsed). A bad query is a `400` page saying so. Signed out, the page links to both
   sign-ins with `?next=` set to itself (§5), so the browser comes back after signing in.
   Signed in, it asks **"Authorize the kfm CLI on this computer?"**: it creates a `submit` token
   named `kfm on <host>` (`kfm CLI` without a host; cut to 64 characters), with a form of the
   session's CSRF token, the three parameters and the buttons `decision=allow` and `deny`.
3. `POST /auth/cli` (a portal form, §8). Without a session it answers `303` to
   `/?error=unauthenticated`, with a missing or wrong `csrf` to `/?error=bad_csrf`, with a bad
   port or state to `/?error=bad_request`: nothing is created and the browser never reaches the
   listener. Otherwise it answers `303` to the listener,
   `http://127.0.0.1:<port>/callback?state=<state>` followed by `&code=<code>` on `allow`
   (the token is created now, visible in `GET /tokens` and on the portal),
   `&error=access_denied` on a cancel, or `&error=<code>` when the token could not be made
   (`limit_reached`, `internal`). The page's `Content-Security-Policy` adds
   `http://127.0.0.1:*` to `form-action` so the browser follows that redirect.
4. The listener checks that `state` is its own and answers the browser with a page of its
   own; the CLI then sends `POST /auth/cli/exchange` with `{"code":"<code>"}` (auth: none; the
   code is the credential) and gets `200`:

```json
{"token":"kfm_…","token_id":"…","name":"kfm on laptop","scope":"submit"}
```

The code is 128 random bits in unpadded base64url (22 characters), **single use** (the
exchange deletes it whatever the outcome) and valid for **5 minutes**. The service stores the
SHA-256 of the code, never the code, bound to the token it unlocks: the row holds the token's
id and its secret sealed under the code (XORed with a SHA-512 of a purpose string and the
code), so neither a database dump nor a log of the browser's URLs yields a usable token, and
the exchange checks the unsealed secret against the token's stored hash. A code that is
malformed, unknown, already exchanged or expired is `invalid_code`; an expired code's token,
never handed out, is revoked (at its exchange, or when anyone next confirms a login).
`kfm logout` revokes the CLI's token with `DELETE /tokens/{token_id}` sent with that token as
its bearer (§6, v1.3), then forgets it.

## 6. The caller and tokens

### `GET /me` (auth: any)

```json
{"id":"…","display_name":"octomonkey","created_at":"…","operator":false,
 "identities":[{"provider":"github","name":"octomonkey","email":"o@example.org","linked_at":"…"}],
 "auth":"session","csrf":"…"}
```

| Key | Type | Present | Meaning |
|---|---|---|---|
| `id` | id | always | The user. |
| `display_name` | string | always | 1 to 64 characters, chosen at account creation. Not unique. |
| `created_at` | time | always | |
| `operator` | bool | always | Whether the account is an operator's (v1.5, §7): not held to one team. |
| `identities` | array | always | One entry per linked provider, by provider name. |
| `identities[].provider` | string | always | `"github"` or `"google"`. |
| `identities[].name` | string or null | always | The provider's login (GitHub) or name (Google), as last reported. |
| `identities[].email` | string or null | always | The email the provider last reported; display only. |
| `identities[].linked_at` | time | always | |
| `auth` | string | always | `"session"` or `"token"`. |
| `csrf` | string | session | The session's CSRF token (§3.2). |
| `scope` | scope | token | The token's scope. |

### `POST /tokens` (auth: session)

Request `{"name":"laptop","scope":"submit"}`. `name` is trimmed and must be 1 to 64 characters
without control characters (`invalid_name`); `scope` as §3.3 (`invalid_scope`). A user has at
most 25 live tokens (`limit_reached`). Response `201`, a token object (below) plus `secret`, the
only time the secret is sent:

```json
{"id":"…","name":"laptop","scope":"submit","created_at":"…","last_used_at":null,"secret":"kfm_…"}
```

### `GET /tokens` (auth: session)

`200 {"tokens":[<token>…]}`: the caller's live tokens, oldest first. A token object:

| Key | Type | Present | Meaning |
|---|---|---|---|
| `id` | id | always | |
| `name` | string | always | |
| `scope` | scope | always | |
| `created_at` | time | always | |
| `last_used_at` | time or null | always | The last request it authenticated. |

### `DELETE /tokens/{id}` (auth: session, or that token itself)

Revokes the caller's token: `204`. Revoked tokens are kept (the record of use) but never listed
or accepted again; a token that is not the caller's or already revoked is `not_found`. From
v1.3 a token may revoke itself (the bearer is the token `{id}` names, either scope, so `kfm
logout` needs no session); a bearer naming another token is `session_required`, as before.

Tokens are managed only through the session (beyond a token revoking itself), so a leaked
token cannot mint or revoke another.

## 7. Teams

An entrant is a team; a solo player is a team of one, created explicitly. A team has members
with a role, `owner` or `member`; its creator is its owner. Submissions, the active bot and the
rating belong to the team (§9), so a member leaves without anything being rotated.

A team name is trimmed and then 3 to 32 characters of ASCII letters, digits, space, `-`, `_`
and `.`, starting and ending with a letter or digit, with no double space (`invalid_name`).
Names are unique ignoring case (`name_taken`). A team has at most 32 members (`limit_reached`).

**One team per account (v1.5).** An account is a member of at most one team, as owner or
member: an entrant is a team and the rating is the team's, so an account in two teams would be
two entrants, holding two places on the board and paired against itself. Creating a team while
in one, and adding a user who is in one, are refused with `already_in_team` and change nothing;
an account that left its team (below) may join or create another. The rule is judged when a
membership is made: memberships from before v1.5 stay.

The exception is an **operator**: the account of whoever runs the ladder, which owns the
**house teams** (the example bots every entrant is paired against, so that a first submission
has opponents). An operator creates and joins any number of teams; the flag lifts this limit
and nothing else (it is no admin role: an operator has no route another account lacks), it
belongs to the account and not to the teams it owns, and it is set on the service's VM, never
over HTTP:

```sh
kfm-ladder operator add <user id or display name>     # lift the limit for the account
kfm-ladder operator remove <user id or display name>  # hold it to one team again; its memberships stay
kfm-ladder operator list                              # id, display name
```

`GET /me` shows it as `operator` (§6).

A team object:

| Key | Type | Present | Meaning |
|---|---|---|---|
| `id` | id | always | |
| `name` | string | always | |
| `created_at` | time | always | |
| `members` | array | always | Owners first, then by join time. |
| `members[].user_id` | id | always | |
| `members[].display_name` | string | always | |
| `members[].role` | string | always | `"owner"` or `"member"`. |
| `members[].joined_at` | time | always | |
| `active` | id or null | always | The team's active submission (§9.5), `null` before one is set (v1.1). |
| `rating` | object or null | always | The team's standing in the current season (v1.2, §10.5): `{"season","rank","rating","rd","games","wins","draws","losses"}` as a board row has them; `null` outside a season and before the team's first game in it. |

| Method and path | Auth | Request | Response |
|---|---|---|---|
| `POST /teams` | session | `{"name":"Banana Squad"}` | `201`, the team. `already_in_team` if the caller is in a team and no operator (v1.5). |
| `GET /teams` | any | | `200 {"teams":[{"id","name","role","created_at"}…]}`: the caller's teams, by name, `role` the caller's. |
| `GET /teams/{id}` | none | | `200`, the team (teams are public). |
| `POST /teams/{id}/members` | session | `{"user":"<id or display name>"}` | `200`, the team. |
| `DELETE /teams/{id}/members/{user}` | session | | `204`. |

Adding a member: the caller must be an owner (`forbidden`; `not_found` if the team does not
exist). `user` is a user id, or a display name compared ignoring case (`unknown_user` if none,
`ambiguous_user` if several share it). The user joins at once as a `member` (`already_member`
if they are one; `already_in_team` if they are in another team and no operator, v1.5). There is
no invitation to accept in v1.0.

Removing a member: the caller removes themself (leaves) or is an owner (`forbidden`
otherwise); `{user}` must be a member (`not_found`). An owner may leave, or be removed, only
while another owner remains (`last_owner`). v1.0 has no route that promotes a member or deletes
a team, so under the one-team rule the only owner of a team stays in it: an account that made
a team of one cannot move to another team (a route to dissolve a team is a later addition).

## 8. The portal

The portal is the same service rendered as HTML (askama templates, no JavaScript):

| Method and path | What |
|---|---|
| `GET /` | Signed out: the landing page (v1.7): what the contest is, the quick start, sign in with GitHub or Google, and from the current season (else the one that started last) the top five of the board and the five latest games. Signed in: the account and its identities (a link button for the provider not yet linked), tokens (create, revoke), teams (create, shown to an account without a team and to an operator, v1.5; add member, leave or remove) and each team's submissions (upload, the list with state, module hash, error, ranked games and faults and a link to the log, an Activate button on ready ones, the active one marked; v1.1), its standing in the current season and its last ten games with links to their pages (v1.2). Shows `?error=<code>` as the code's message (with the `&detail=` text for `bad_manifest` and `bad_source`, v1.1) and `?linked=<provider>` as a notice. |
| `GET /privacy` | The privacy page (v1.4): who runs the service (`KFM_LADDER_CONTACT`), what it stores and why, what it does not, IP addresses, the third parties, how to have data deleted. Public; every portal page's footer links to it. |
| `GET /docs/contest` | The contest guide (v1.7): the rules of the `contest` mode as an entrant needs them, its arenas drawn, what a bot sees and where each of these is in the observation, and how the ladder plays it. Public. It names the engine release it was written for and says so when the service plays another (`KFM_LADDER_ENGINE_TAG`); the engine's content and its `docs/bot-abi.md` stay normative. |
| `GET /ladder` | With `Accept: text/html` (a browser): the board of §10.5 as a page, paged, with links to every season (v1.2), each team's name linking to its games, and the season's eight latest games (v1.7). |
| `GET /matches` | With `Accept: text/html` (v1.7): the list of §10.6 as a page, 25 games a page, with the same `team`, `season` and `page` parameters and the same errors. |
| `GET /matches/{m}` | With `Accept: text/html`: the game's page: both sides with score, kills, deaths, rating change and faults, the replay's download link, the viewer link, the seed and hashes to re-run it, and the raw result (v1.2). |
| `POST /portal/tokens` | Form `csrf`, `name`, `scope`: creates a token and renders `/` with its secret, once. |
| `POST /portal/tokens/{id}/revoke` | Form `csrf`. |
| `POST /portal/teams` | Form `csrf`, `name`. |
| `POST /portal/teams/{id}/members` | Form `csrf`, `user`. |
| `POST /portal/teams/{id}/members/{user}/remove` | Form `csrf` (leave when `{user}` is the caller). |
| `POST /portal/teams/{id}/submissions` | `multipart/form-data`: `csrf`, `language` (`cpp` or `c`), `entry` (empty for `.`) and the file `source.zip`; the manifest is made from the two fields (v1.1). |
| `POST /portal/teams/{id}/active` | Form `csrf`, `submission` (v1.1). |

Forms are `application/x-www-form-urlencoded` (the upload: `multipart/form-data`),
authenticated by the session cookie only, and carry the session's CSRF token in `csrf`. Each
answers `303` to `/` on success and to `/?error=<code>` on failure (the codes of §2;
`/?error=<code>&detail=<text>` for a refused upload), except a created token, which renders the
page (`200`). Portal pages are sent with `Cache-Control: no-store`, a `Content-Security-Policy` that
allows no script, `X-Content-Type-Options: nosniff` and `Referrer-Policy: same-origin`. The
`/portal/...` routes are for the portal's own forms; programs use the JSON routes. `GET /ladder`,
`GET /matches` (v1.7) and `GET /matches/{m}` answer HTML only to a request whose `Accept` header lists `text/html` (what a
browser's navigation sends); without it, or with `*/*` or `application/json`, they answer the
JSON of §10, so a link to any of them works for a person and a program alike (v1.2).

## 9. Submissions

A submission is a team's bot as source: a zip of source files plus a small manifest, built by
the service with the pinned toolchain (never by the author), checked as the engine's host
checks a module, and played once against the reference bot before it may be made active
(architecture §8 "Source in, module out", "The build sandbox", "Submissions and retention").
A submission is immutable once uploaded; a new version of a bot is a new submission.

### 9.1 The manifest

`manifest.toml`, UTF-8 TOML, at most 4096 bytes:

```toml
language = "cpp"      # required: the ladder language
entry = "bots/mine"   # optional: the directory inside source.zip that holds the sources; default "."
```

| Key | Type | Required | Meaning |
|---|---|---|---|
| `language` | string | yes | `"cpp"`, the only ladder language. `"c"` is accepted as an alias and builds the same way (the recipe compiles a directory of only `.c` files as C11 and one with any `.cc` or `.cpp` file as C++20, every file in one command). |
| `entry` | string | no | A relative path with `/` separators: no leading `/`, no `\`, no `:`, no `..` component, no control character, at most 255 bytes. `.` and empty components are dropped (`"./src/"` is `src`). |

Any other key is `bad_manifest`, so a typo never silently means the default. The entry must hold
at least one `.c`, `.cc` or `.cpp` file directly (`bad_source` otherwise): the recipe
(`tools/build.sh`) compiles the files directly in the entry directory, and the files below it
are there for `#include`.

### 9.2 The archive

`source.zip`, at most 4 MiB (4 194 304 bytes) compressed (`too_large` above). The upload is
walked entry by entry before anything is stored, and the first violation is `bad_source`
naming it:

- every name is a relative path: no leading `/`, no drive letter, no `\`, no `..` component,
  no control character, at most 255 bytes;
- nothing but regular files and directories: no symbolic link, device or fifo; nothing
  encrypted;
- at most 256 files (1024 entries of any kind) and at most 16 MiB (16 777 216 bytes)
  unpacked, counted while decompressing (a header's sizes are not trusted);
- no name twice (after dropping `.` components), and no file where another entry needs a
  directory.

The build worker unpacks only the entry directory's subtree, after checking the archive by
the same rules again.

### 9.3 States

| State | Meaning | Next |
|---|---|---|
| `queued` | Uploaded and stored; a `build` job waits for a build worker. | `building`; `failed` if the build cannot be run. |
| `building` | A build worker claimed the build, or the module is built and its validation is queued or running. | `failed`, `rejected`, `ready`. |
| `failed` | The build produced no module: a compile error, the 120 s limit, the 2 GiB memory cap, no `bot.wasm`, or the build could not be run after three attempts. `error` says which; the log has the compiler's output. | final |
| `rejected` | The module was built but fails validation (§9.6): the host's load rules, or a fault in the smoke match. `error` says which. | final |
| `ready` | Built and validated: it may be made active. | final |

A failure on the service's side (the object store, the Docker daemon, the runner) retries the
job, up to three attempts; a job whose worker stops heartbeating for five minutes is re-queued
the same way. Only after the last attempt does the submission turn `failed`, with an `error`
starting `the build could not be completed:` or `the validate could not be completed:`.

### 9.4 The submission object

| Key | Type | Present | Meaning |
|---|---|---|---|
| `id` | id | always | |
| `team_id` | id | always | |
| `submitted_by` | id or null | always | The uploading user; `null` once that account is deleted. |
| `created_at` | time | always | The upload. |
| `updated_at` | time | always | The last change of state or of the stored objects. |
| `state` | string | always | §9.3. A client treats an unknown state as "not ready". |
| `language` | string | always | The manifest's language, normalised (`"cpp"`). |
| `entry` | string | always | The manifest's entry, normalised (`"."` for the root). |
| `module_hash` | string or null | always | The SHA-256 of the built module, 64 lower-case hex digits, once built (a `rejected` submission has one too). |
| `log_url` | string or null | always | A presigned URL of the build and validation log on the object store, valid for 15 minutes from the response; `null` before a log exists and once retention pruned it (§9.7). The route that shows it is under the presigning limit (§2, v1.4). |
| `error` | string or null | always | Why it is `failed` or `rejected`, one line for a person; `null` otherwise. |
| `active` | bool | always | Whether it is its team's active submission. |
| `ranked_games` | number | always | Ranked games it played (v1.2, §10.3). |
| `ranked_faults` | number | always | Of those, the games in which one of its slots faulted (v1.2, §10.3). |

### 9.5 Routes

| Method and path | Auth | Request | Response |
|---|---|---|---|
| `POST /teams/{t}/submissions` | submit, a member of `t` | `multipart/form-data` with the parts `manifest.toml` (§9.1) and `source.zip` (§9.2) | `202`, the submission (`queued`). |
| `GET /teams/{t}/submissions` | any, a member of `t` | | `200 {"submissions":[<submission>…]}`, newest first. |
| `GET /submissions/{s}` | any, a member of its team | | `200`, the submission. Under the presigning limit (§2, v1.4): `429 rate_limited` with `Retry-After`. |
| `GET /submissions/{s}/log` | any, a member of its team | | `200`, the log as `text/plain; charset=utf-8`. |
| `POST /teams/{t}/active` | submit, a member of `t` | `{"submission":"<id>"}` | `200`, the submission, now `active`. |

Uploading: `not_found` if the team does not exist, `forbidden` for a non-member; then the
manifest (`bad_manifest`) and the archive (`too_large`, `bad_source`) are checked; then the
team's rate, one submission per team per minute counted from the last accepted upload
(`rate_limited`; a refused upload does not count). The request body may be at most 4 MiB plus
64 KiB of manifest and framing. A part named neither `manifest.toml` nor `source.zip` is
ignored (it may be at most 1 KiB); a part sent twice is `bad_request`. Success stores the zip,
inserts the submission and its `build` job in one transaction and answers `202 Accepted`:
nothing is built yet; poll `GET /submissions/{s}` until `state` is final.

Listing: the team's submissions that retention keeps, newest first: every one whose source is
still stored (those queued or building, and the five most recent finished ones) and the
active one. A pruned submission is not listed, and `GET /submissions/{s}` still shows it.

Reading: a submission of a team the caller is not a member of is `not_found` (submissions are
not public); without credentials, `unauthenticated`. The log route is `not_found` while there
is no log yet and after pruning.

Activating: `not_found` if the team does not exist or the submission is not the team's,
`forbidden` for a non-member, `not_ready` unless it is `ready`. Any member may activate (an
owner is not required). Activating replaces the previous active submission; the rating is the
team's and stays. v1.1 has no route that clears the active submission.

An upload with `curl`, as the CLI's `submit` sends it:

```sh
curl -H "Authorization: Bearer $KFM_TOKEN" -F manifest.toml=@manifest.toml -F source.zip=@bot.zip \
  https://bots.kfm.minip8.com/teams/$TEAM/submissions
```

### 9.6 Build and validation

The build worker (`kfm-ladder worker build`) claims the `build` job, unpacks the entry
directory into a fresh directory and runs one throwaway container of the pinned bot build
image (`kfm-bot-build`, this repository's `Dockerfile`) with the flags of architecture §8:

```
docker run --rm --name kfm-build-<job> --network none --memory 2g --cpus 1 --pids-limit 256
  --read-only --tmpfs /tmp --user builder -v SRC:/src:ro -v OUT:/out IMAGE timeout 120 kfm-build
```

(`--name` is the service's addition: a build that outlives the worker's own 180 s clock is
removed by name). The only result that counts is `OUT/bot.wasm`: the module is stored at
`modules/<s>.wasm`, its SHA-256 recorded, and a `validate` job queued. Validation then:

1. checks the module as the engine's host does at load (the engine's `docs/bot-abi.md` §1): at
   most 4 MiB; no import but the function `env.kfm_log : (i32, i32) -> ()`, so no imported
   memory, table or global; one 32-bit memory of its own with an initial size of at most 256
   pages (16 MiB); the exports `memory`, `kfm_abi_version : () -> i32`,
   `kfm_obs_buffer : () -> i32`, `kfm_init : (i64, i32) -> ()` and `kfm_act : (i32) -> i64`;
2. plays one smoke match with the season's released `kfm_arena` in the ranked shape (5v5,
   v1.2: the module fills its five slots against five `ref:hard`; before, 1v1, which costs
   less fuel per tick than a ranked game), `kfm_arena --mode contest --team <module>,<module>,
   <module>,<module>,<module> --team ref:hard,ref:hard,ref:hard,ref:hard,ref:hard --matches 1
   --ticks 3600 --json --result <file> --quiet`,
   and reads its match objects (the engine's `docs/arena-result.md`): a module slot with
   `traps`, `over` or `bad` above 0, or `dead` not null, rejects the submission, and so does
   `kfm_arena` refusing the module at load (exit 3).

The log (`logs/<s>.txt`) holds the build command, the compiler's output and exit status, the
module's size and hash, the checks, the smoke match's command, its stderr and JSON lines, and
the verdict (`READY`, `FAILED: …` or `REJECTED: …`). Each part is capped at 1 MiB. It is
stored whatever the outcome.

### 9.7 Retention

When a submission turns `ready`, its team is pruned: the five most recent finished
submissions (`ready`, `failed`, `rejected`, by upload time) keep their source archive and log,
and so does the active one; every older finished submission loses both objects (and its
`log_url`). A module is never deleted, so every recorded match stays reproducible from its
seed and modules; queued and building submissions are never pruned; the row stays, and
`GET /submissions/{s}` keeps showing its state and hash.

## 10. Seasons, matches and the ladder

Served from v1.2 (architecture §8 "Matches and ratings", §11 "Scheduling and ratings"; v0.59: a
season is a time window, every game records its engine release, ratings persist across a
release with the deviation widened). Every route in this section is public (auth: none): the
ratings, the results and the replays are the contest's record.

### 10.1 Seasons

A season is a time window `[starts_at, ends_at)` and the content mode its games play
(`contest`). Windows never overlap (the database refuses an overlapping one), so at most one
season is **current**: the one whose window holds now. Nothing is scheduled outside a season;
a season's board freezes at its end (a match job that starts after the end records nothing,
§10.3), and a new season starts every rating afresh. Seasons are made by an operator on the
service's VM, never over HTTP (there is no admin role):

```sh
kfm-ladder season create 2026-12 --start 2026-12-01T00:00:00Z --end 2027-01-01T00:00:00Z [--mode contest]
kfm-ladder season list      # name, start, end, mode, id; "current" on the current one
```

A name is 1 to 64 characters of ASCII letters, digits, space, `-`, `_` and `.`, starting and
ending with a letter or digit, unique; a mode is 1 to 32 characters of `a-z`, `0-9` and `_`.

`GET /seasons`: `200 {"seasons":[<season>…],"engine_tag":"v0.5.1"}`, the latest start first.
`engine_tag` (v1.3) is the engine release the service plays new games with
(`KFM_LADDER_ENGINE_TAG`, §10.4), the one `kfm test` fetches by default. A season object:

| Key | Type | Present | Meaning |
|---|---|---|---|
| `id` | id | always | |
| `name` | string | always | |
| `mode` | string | always | The content mode its games play. |
| `starts_at` | time | always | The window's first instant. |
| `ends_at` | time | always | The window's end, exclusive: the board freezes here. |
| `current` | bool | always | Whether the window holds now. |

### 10.2 Scheduling

`kfm-ladder serve` runs the scheduler in the background. Of several API instances one
schedules: it holds a Postgres session advisory lock (`pg_try_advisory_lock`) on a connection
of its own and makes rounds over that connection only; the others stay idle until it goes. A
**round** is due once the current season's last round is `KFM_LADDER_ROUND_SECONDS` old
(default 1800, judged on the database's clock, so a restart neither skips nor doubles one;
v1.4, from 300: every game writes a replay to the object store, and a round every 5 minutes
with 20 teams writes 17 k replays a day).
Rounds are numbered from 1 within a season. A round:

1. takes every team whose active submission is `ready` (§9.3) and sorts them by their rating
   in the season, highest first (1500 for a team not yet rated), ties by team id. Fewer than
   two such teams: no round. A team that changed its active submission plays the new one; the
   rating stays the team's;
2. pairs them in k passes, k = `KFM_LADDER_ROUND_OPPONENTS` (default 3). In each pass a team
   is in at most one new pair; the pass walks the teams from the top in rank order, except that
   teams with fewer pairs so far walk first (the team an odd pass left out leads the next), and
   each team without a pair in this pass takes the team nearest to it by rating that is also
   free in this pass, has fewer than k pairs and has not met it in this round (ties go to the
   higher ranked). So a team is in at most k pairs and a round of n teams has at most n·k/2;
3. counts a pair whose match job is still queued or running as made, but queues nothing for it
   (a backlog of workers never grows the queue);
4. queues one `match` job per new pair, side A the higher ranked, with the seed: the first 8
   bytes of SHA-256 over `kfm-ladder match seed v1\0`, the season id's 16 bytes, the round
   number (big-endian i32) and the two submission ids' bytes (A's first), read big-endian and
   shifted right by 2, so it is below 2^62. The game's arena and sides are drawn from it too
   (§10.3, v1.6).

### 10.3 The match job

A match worker (`kfm-ladder worker match`) claims the job, fetches both modules from the object
store (checked against their recorded SHA-256), and runs the season's released `kfm_arena`
once, in a fresh directory holding the modules as `<submission>.wasm`:

```
kfm_arena --mode <season's mode> --arena <arena> --team X,X,X,X,X --team Y,Y,Y,Y,Y --matches 1
  --seed <seed> --ticks <KFM_LADDER_MATCH_TICKS> --json --result result.jsonl --replay-dir replays --quiet
```

Each side's module fills all five slots of its team (the contest is 5v5; `--team` takes one
spec per slot). A pair plays one **game**, recorded as one match (v1.6; before, two games from
both sides through `--matches 2 --rotate`, which on a mode with an arena pool never left the
pool's first arena). The arena and the sides are **drawn from the seed**: with D the SHA-256
over `kfm-ladder match draw v1\0` and the seed's 8 bytes (big-endian),

- the lowest bit of D's byte 0 is the team index side A plays: 0, and X is A's module (the
  first `--team` plays the mode's team 0); 1, and X is B's;
- D's bytes 8 to 15, read big-endian, modulo the number of arenas in the mode's pool, index the
  arena in the pool's content order.

The pool is the engine's content, not the service's configuration: a worker asks its runner
once per mode, by playing 32 one-tick matches of reference bots without `--arena` and reading
the `arena` key of their match objects (`kfm_arena` cycles a pool, two matches a member, and
plays a mode without a pool on its one arena). A runner whose match objects carry no `arena`
(an engine release before v0.10.0) is given no `--arena` and plays the arena it would have. So
over the games of a season a team meets every arena from either side about equally often, a
single game is as random as a coin and a die, and the draw is the same on every attempt of a
job. The worker reads the match object of the engine's `docs/arena-result.md`, stores its
replay at `replays/<match>.kfmr`, and in one transaction inserts the match and updates both
teams' ratings (§10.4) and both submissions' `ranked_games` and `ranked_faults`.

A **fault in a ranked game** (a module slot with `traps`, `over` or `bad` above 0, or `dead`
not null) is recorded on the match (the side's `faults`) and counted on the submission
(`ranked_faults`: the games with at least one faulty slot); it never rejects the submission,
which passed validation (§9.6), and the game counts as played: the engine substituted "repeat
last input" for the faulty ticks, which is the bot's loss to bear.

`kfm_arena` exiting non-zero (the pool's probe included), or leaving no result, no summary
line, not exactly game 0, side A on another team than the drawn one or a replay missing, fails
the attempt; the job is retried like any other (three attempts in all). A game already recorded
by an earlier attempt is never recorded again (one row per job and game), and the draw and the
run are deterministic, so a retry records what the first attempt would have.
A job that starts after its season's end records nothing. A pair whose job failed for good is
simply paired again in a later round.

### 10.4 Ratings

The rating is Glicko-2 (Glickman, "Example of the Glicko-2 system") per team and season, with
τ = 0.5, a new team at rating 1500, deviation (RD) 350 and volatility 0.06, the Glicko-2 scale
μ = (r − 1500) / 173.7178, φ = RD / 173.7178, and the volatility found by the Illinois
iteration to 10^-6. Each game is its own rating period of one game: both teams are updated
against each other's rating from before the game, a win scoring 1, a loss 0, and a draw (a game
that ended without a winner, or that the tick cap ended) 0.5. A period batched per round is a
later refinement.

At an engine release a team's deviation widens before its first game under the new release:
when the release the service runs (`KFM_LADDER_ENGINE_TAG`) differs from the release of the
team's last rated game, RD' = min(350, sqrt(RD² + 100²)) first; the rating itself is kept (a
release is the same shock to every entrant, never a reset).

Ratings are kept unrounded; the API shows `rating` and `rd` on Glicko's scale rounded to whole
numbers, and a game's `rating_change` to a tenth.

### 10.5 The board: `GET /ladder?season=<name>&page=<n>`

`season` names a season (`not_found` if none has that name); without it, the current season,
else the season that started last (frozen at its end). `page` is 1-based (default 1;
`bad_request` below 1 or not a number), 50 teams a page. `200`:

```json
{"season":<season>,"page":1,"ratings":[{"rank":1,"team_id":"…","name":"Banana Squad","rating":1712,
 "rd":64,"games":40,"wins":28,"draws":3,"losses":9,"active":"…","module_hash":"…"}…],"next":2}
```

Before any season has begun: `{"season":null,"page":1,"ratings":[],"next":null}`. The board
holds the teams with at least one game in the season, by rating (unrounded), then the smaller
deviation, then the team id; `rank` counts from 1 across pages. `next` is the next page's
number, or `null` on the last.

| Key | Type | Present | Meaning |
|---|---|---|---|
| `rank` | number | always | The place on the board. |
| `team_id`, `name` | id, string | always | |
| `rating`, `rd` | number | always | The rating and its deviation, rounded (§10.4). |
| `games`, `wins`, `draws`, `losses` | number | always | The team's games in the season. |
| `active` | id or null | always | The team's active submission now (not necessarily the one that played every game). |
| `module_hash` | string or null | always | The active submission's module SHA-256 (64 hex digits). |

### 10.6 Matches: `GET /matches?team=<t>&season=<name>&page=<n>` and `GET /matches/{m}`

A **match** is one recorded game. `GET /matches` lists them newest first, 25 a page, of every
team and season unless `team` (an id, either side; `bad_request` if not a UUID) or `season` (a
name; `not_found` if unknown) narrows it: `200 {"page":1,"matches":[<match>…],"next":2}`. A
match object:

| Key | Type | Present | Meaning |
|---|---|---|---|
| `id` | id | always | |
| `season`, `season_id` | string, id | always | Its season. |
| `round` | number | always | The scheduler's round number in the season. |
| `game` | number | always | The game of its pair's run: 0 since v1.6 (a pair is one game); before, 0 or 1 (1 had the sides swapped). |
| `created_at` | time | always | When it was recorded. |
| `engine_tag` | string | always | The engine release it ran under (`v0.5.1`): re-run it with that release's `kfm_arena`. |
| `seed` | number | always | The game's world seed (`--seed`). |
| `ticks` | number | always | Sim ticks it ran. |
| `end` | string | always | `ended`, or `cap` (the tick cap ended it: a draw). |
| `winner` | id or null | always | The winning team; `null` for a draw. |
| `sides` | array | always | Two sides, side A (the higher ranked when scheduled) first. |
| `sides[].team_id`, `sides[].name` | id, string | always | |
| `sides[].submission_id` | id | always | The submission that played. |
| `sides[].team` | number | always | The mode's team index the side played (0 or 1). |
| `sides[].score`, `kills`, `deaths` | number | always | The side's `teams[]` entry in the result. |
| `sides[].faults` | array | always | One `{"slot","traps","over","bad","dead"}` per faulty module slot of the side (§10.3); empty when clean. |
| `sides[].rating_change` | number | always | The side's rating change from this game, to a tenth. |
| `arena` | string or null | always | The arena it was played on (v1.6), as its result names it: re-run it with `--arena`. `null` for a game played with an engine release before v0.10.0, whose result names none. |

`GET /matches/{m}`: `200`, the match object plus:

| Key | Type | Present | Meaning |
|---|---|---|---|
| `content_hash`, `build_hash` | string, string or null | always | The result's two hashes (8 hex digits). |
| `result` | object | always | The match object `kfm_arena` printed, whole (the engine's `docs/arena-result.md`; its specs are `<submission>.wasm`). |
| `replay_url` | string or null | always | A presigned URL of the `.kfmr` on the object store, valid for one hour from the response; `null` once the replay is pruned, 24 hours after the game by default (below, v1.4). |
| `viewer_url` | string or null | always | `<KFM_LADDER_VIEWER_URL>/<engine_tag>/?replay=<replay_url, URL-encoded>`: the replay in the client of the game's own release (`https://kfm.minip8.com/v0.5.1/?replay=…`); `null` when `replay_url` is. |

`GET /matches/{m}` is under the presigning limit (§2, v1.4): `429 rate_limited` with
`Retry-After`, for its HTML page too.

**Replay retention (v1.4).** A replay is kept for `KFM_LADDER_REPLAY_HOURS` after its game
was recorded (default 24; 0 keeps replays forever). The match itself is kept forever: its
result, seed, engine release, both submissions (whose modules are never deleted, §9.7) and
the ratings it moved stay, so a pruned game can still be re-run with that release's
`kfm_arena`; only the `.kfmr` object goes, and with it `replay_url` and `viewer_url`, which
answer `null` from then on (the portal's page drops its replay links). The scheduling instance
(§10.2) sweeps every 10 minutes under its advisory lock, so one process prunes: the games
older than the retention that still have a replay, oldest first, 200 at a time, each object
deleted from the store (a missing object counts as deleted) and then the batch's keys cleared
in one update. A delete that fails keeps its key, and the next sweep tries again, so a
replay may outlive its 24 hours by a sweep or more, never the other way. Download a replay you
want to keep (`kfm replay get`, or the link) within the day.

The viewer itself is the browser client's replay-by-URL feature (the engine's M7 step 6,
architecture v0.68): the page of an engine release after v0.5.1 plays the replay. v0.5.1's page
predates the viewer, so a link for a game played under v0.5.1 opens a page whose client stops
on the unknown `--replay` argument. The client fetches the replay from the object store's
presigned URL, so the bucket must allow that origin's GET (CORS, `deploy/README.md`).

## 11. Versioning

- The promise is append-only: a route, a request or response key, an error code or a status of
  a route listed as served is never renamed, removed or retyped, and a key's meaning never
  changes. New routes, new optional request keys, new response keys and new error codes may
  appear in any minor version. A client ignores response keys it does not know and treats an
  unknown error code by its HTTP status.
- A change that cannot be append-only gets a new route (or a `/v2/` prefix) beside the old one.
- The version below is this document's; the service's `/healthz` does not report it.

| Version | Change |
|---|---|
| v1.0 | Health, GitHub and Google sign-in with linking, sessions and CSRF, `/me`, tokens and scopes, teams, the portal; submissions, the ladder and matches reserved. |
| v1.1 | Submissions served (§9): the upload, list, show, log and activate routes, the manifest and archive rules, the states, build and validation, retention; the team object's `active`; the codes `bad_manifest`, `bad_source`, `too_large`, `rate_limited`, `not_ready`; the portal's upload and activate forms. Refined from the reserved shapes: the upload answers `202` with the whole submission (not `201` with three keys: nothing is built yet), the submission object adds `updated_at`, `language` and `entry`, and the list is what retention keeps (in-flight ones included, pruned ones left out). |
| v1.2 | Seasons, the scheduler, the match job, ratings and the board served (§10): `GET /seasons`, `GET /ladder`, `GET /matches`, `GET /matches/{m}` and their portal pages; the team object's `rating`; the submission object's `ranked_games` and `ranked_faults`; `kfm-ladder season create|list`. Refined from the reserved shapes: the board's rows are `rank`, `rd` (not `deviation`), the record and the active module's hash, with `page` beside `next`; a match is one game of a pair's two-game run, with its sides, faults and rating changes; `GET /ladder` and `GET /matches/{m}` answer HTML to a browser. |
| v1.3 | The `kfm` CLI's login (§5.1): `GET /auth/cli`, `POST /auth/cli`, `POST /auth/cli/exchange`; a sign-in start's `?next=` (the CLI page only); the code `invalid_code`; `DELETE /tokens/{id}` with that token as the bearer (a token revokes itself); `GET /seasons`' `engine_tag`. |
| v1.4 | Replay retention (§10.6): a game's replay is deleted 24 hours after it was recorded (`KFM_LADDER_REPLAY_HOURS`), after which its `replay_url` and `viewer_url` are `null`; the match row stays. The presigning limit (§2): `GET /matches/{m}` and `GET /submissions/{s}` answer `429 rate_limited` with `Retry-After` past 60 requests a minute per client IP (`KFM_LADDER_PRESIGN_PER_MINUTE`, `KFM_LADDER_TRUST_PROXY`). The round interval's default becomes 1800 seconds (§10.2). The portal's `GET /privacy` page (§8). The production origin is `https://bots.kfm.minip8.com`. |
| v1.5 | One team per account (§7): `POST /teams` and `POST /teams/{id}/members` refuse a second membership with the new code `already_in_team` (a rule tightened, no route or key changed; memberships from before stay); operators, set by `kfm-ladder operator add|remove|list` on the VM, are excepted and own the house teams; `GET /me` gains `operator`; the portal shows the create-team form only to an account that may use it. |
| v1.6 | A pair is one game (§10.3): the match job runs `--matches 1` on an arena of the mode's pool and with the sides drawn from the job's seed, where it ran `--matches 2 --rotate` (both sides, always the pool's first arena); the worker asks the runner for the pool. A match object gains `arena` (§10.6); its `game` is 0 from here on. No route changed; games recorded before keep their `game` 0 or 1. |
| v1.7 | The site (§8): `GET /` signed out is a landing page; `GET /docs/contest`, the contest guide; `GET /matches` answers HTML to a browser as `GET /ladder` and `GET /matches/{m}` do, and the board's page lists the season's latest games. No JSON route, key or code changed. |

## 12. Configuration of the binary

`kfm-ladder serve` (API and portal, and the scheduler, §10.2; runs pending migrations first),
`kfm-ladder migrate` (apply migrations and exit), `kfm-ladder season create|list` (§10.1; applies
migrations first), `kfm-ladder operator add|remove|list` (§7, v1.5; applies migrations first),
`kfm-ladder worker build` (build and validate jobs, §9.6) and `kfm-ladder worker match` (match
jobs, §10.3). Workers never migrate, and stop on SIGTERM once the job in
hand is done. Configuration is the environment only, per role: `migrate`, `season` and `operator` read
only `DATABASE_URL` (and `SENTRY_DSN`); a worker never reads the session key, the OAuth apps, the
public URL or the listen address; `worker build` also needs `KFM_BUILD_IMAGE`,
`KFM_LADDER_BUILD_DIR` (an absolute path) and `KFM_LADDER_ARENA`, and `worker match` needs
`KFM_LADDER_ARENA` (and may set `KFM_LADDER_MATCH_TICKS`); `serve` needs the full set and may set
`KFM_LADDER_ROUND_SECONDS`, `KFM_LADDER_ROUND_OPPONENTS`, `KFM_LADDER_VIEWER_URL`,
`KFM_LADDER_REPLAY_HOURS`, `KFM_LADDER_PRESIGN_PER_MINUTE`, `KFM_LADDER_TRUST_PROXY` and
`KFM_LADDER_CONTACT` (v1.4). A missing or
malformed required variable exits 2 with a line naming it. The variables, their defaults and
the roles that read them are listed in `service/README.md`.
