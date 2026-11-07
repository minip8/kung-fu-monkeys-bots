# Kung Fu Monkeys ladder API v1.1

Status: normative, append-only from v1.0 (2026-11-07, M7 step 3a, engine architecture v0.63);
v1.1 (2026-11-07, M7 step 3b) serves submissions.
The design and its rationale are the engine's `docs/architecture.md` §8 "The ladder" and §11
"M7 step 3 design". This file is the HTTP surface shared by the service (`service/`, the
`kfm-ladder` binary), its portal and the `kfm` command line tool; change it here first. The
routes marked **reserved** (§10) are not served yet (they answer `not_found`); their shapes are fixed
here so the CLI can be written against the whole surface, and a later step may still refine a
reserved route before it is served.

## 1. Transport and conventions

- HTTPS in production (`https://ladder.kfm.minip8.com`, Caddy in front of the service); plain
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
| `too_large` | 413 | An upload is over its cap: 4 MiB of `source.zip` (v1.1). |
| `rate_limited` | 429 | A team submits at most once a minute (v1.1). |
| `oauth_failed` | 502 | The provider's reply could not be obtained or verified (token exchange, profile, id token). |
| `internal` | 500 | A failure on the service's side; logged, never detailed. |

The codes are append-only (§11): a client must treat an unknown code by its status.

For `bad_manifest` and `bad_source` the `message` is the code's sentence followed by the
detail, as in `The source archive is refused: "../x.c" contains a .. component.`; it is still
display text, not a contract.

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

A token is created on the portal (§6), shown once and never again, and sent as

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

A start sets the cookie `kfm_oauth` (`Path=/auth`, `HttpOnly`, `SameSite=Lax`, `Secure` under
https, 10 minutes): the provider, the `state` value, the PKCE verifier and (Google) the nonce,
signed with the session key. The callback requires that cookie, unexpired, for the same
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

Creating an account creates no team (§7). A callback always answers `303`: to `/` after a
sign-in, to `/?linked=<provider>` after a link, to `/?error=<code>` on failure (`oauth_state`,
`oauth_denied`, `oauth_failed`, `already_linked`), with the session cookie set when a new
session starts. The CLI's `login` (step 5) is this flow ending in a token; it is not part of
v1.0.

`POST /auth/logout` takes the CSRF token as the `X-CSRF-Token` header or a urlencoded `csrf`
field, deletes the session, clears the cookie and answers `303` to `/`. Without a session it
just clears the cookie.

## 6. The caller and tokens

### `GET /me` (auth: any)

```json
{"id":"…","display_name":"octomonkey","created_at":"…",
 "identities":[{"provider":"github","name":"octomonkey","email":"o@example.org","linked_at":"…"}],
 "auth":"session","csrf":"…"}
```

| Key | Type | Present | Meaning |
|---|---|---|---|
| `id` | id | always | The user. |
| `display_name` | string | always | 1 to 64 characters, chosen at account creation. Not unique. |
| `created_at` | time | always | |
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

### `DELETE /tokens/{id}` (auth: session)

Revokes the caller's token: `204`. Revoked tokens are kept (the record of use) but never listed
or accepted again; a token that is not the caller's or already revoked is `not_found`.

Tokens are managed only through the session, so a leaked token cannot mint another.

## 7. Teams

An entrant is a team; a solo player is a team of one, created explicitly. A team has members
with a role, `owner` or `member`; its creator is its owner. Submissions, the active bot and the
rating belong to the team (§9), so a member leaves without anything being rotated.

A team name is trimmed and then 3 to 32 characters of ASCII letters, digits, space, `-`, `_`
and `.`, starting and ending with a letter or digit, with no double space (`invalid_name`).
Names are unique ignoring case (`name_taken`). A team has at most 32 members (`limit_reached`).

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

| Method and path | Auth | Request | Response |
|---|---|---|---|
| `POST /teams` | session | `{"name":"Banana Squad"}` | `201`, the team. |
| `GET /teams` | any | | `200 {"teams":[{"id","name","role","created_at"}…]}`: the caller's teams, by name, `role` the caller's. |
| `GET /teams/{id}` | none | | `200`, the team (teams are public). |
| `POST /teams/{id}/members` | session | `{"user":"<id or display name>"}` | `200`, the team. |
| `DELETE /teams/{id}/members/{user}` | session | | `204`. |

Adding a member: the caller must be an owner (`forbidden`; `not_found` if the team does not
exist). `user` is a user id, or a display name compared ignoring case (`unknown_user` if none,
`ambiguous_user` if several share it). The user joins at once as a `member` (`already_member`
if they are one). There is no invitation to accept in v1.0.

Removing a member: the caller removes themself (leaves) or is an owner (`forbidden`
otherwise); `{user}` must be a member (`not_found`). An owner may leave, or be removed, only
while another owner remains (`last_owner`). v1.0 has no route that promotes a member or deletes
a team.

## 8. The portal

The portal is the same service rendered as HTML (askama templates, no JavaScript):

| Method and path | What |
|---|---|
| `GET /` | Signed out: sign in with GitHub or Google. Signed in: the account and its identities (a link button for the provider not yet linked), tokens (create, revoke), teams (create, add member, leave or remove) and each team's submissions (upload, the list with state, module hash, error and a link to the log, an Activate button on ready ones, the active one marked; v1.1). Shows `?error=<code>` as the code's message (with the `&detail=` text for `bad_manifest` and `bad_source`, v1.1) and `?linked=<provider>` as a notice. |
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
`/portal/...` routes are for the portal's own forms; programs use the JSON routes.

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
| `log_url` | string or null | always | A presigned URL of the build and validation log on the object store, valid for 15 minutes from the response; `null` before a log exists and once retention pruned it (§9.7). |
| `error` | string or null | always | Why it is `failed` or `rejected`, one line for a person; `null` otherwise. |
| `active` | bool | always | Whether it is its team's active submission. |

### 9.5 Routes

| Method and path | Auth | Request | Response |
|---|---|---|---|
| `POST /teams/{t}/submissions` | submit, a member of `t` | `multipart/form-data` with the parts `manifest.toml` (§9.1) and `source.zip` (§9.2) | `202`, the submission (`queued`). |
| `GET /teams/{t}/submissions` | any, a member of `t` | | `200 {"submissions":[<submission>…]}`, newest first. |
| `GET /submissions/{s}` | any, a member of its team | | `200`, the submission. |
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
  https://ladder.kfm.minip8.com/teams/$TEAM/submissions
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
2. plays one smoke match with the season's released `kfm_arena`, `kfm_arena --mode contest
   --team <module> --team ref:hard --matches 1 --ticks 3600 --json --result <file> --quiet`,
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

## 10. Reserved routes

Not served in v1.1; the shapes below are the design's (architecture v0.63) and become
normative when a later version of this document serves them.

| Method and path | Auth | Step | What |
|---|---|---|---|
| `GET /ladder` | none | 4 | The season's ratings, paged: `{"season","ratings":[{"team_id","name","rating","deviation","games"}…],"next"}`. |
| `GET /matches?team=<t>` | none | 4 | A team's matches, newest first, paged. |
| `GET /matches/{m}` | none | 4 | A match: `seed`, `engine_tag`, `content_hash`, both sides (team and submission ids), the parsed result (the engine's `docs/arena-result.md` match object), `replay_url` and `viewer_url` (`https://kfm.minip8.com/<tag>/?replay=<url>`, step 6). |

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

## 12. Configuration of the binary

`kfm-ladder serve` (API and portal; runs pending migrations first), `kfm-ladder migrate` (apply
migrations and exit), `kfm-ladder worker build` (build and validate jobs, §9.6) and
`kfm-ladder worker match` (match jobs; until step 4 each fails as "not implemented (M7 step
4)"). Workers never migrate, and stop on SIGTERM once the job in hand is done. Configuration is
the environment only, per role: `migrate` reads only `DATABASE_URL` (and `SENTRY_DSN`); a worker
never reads the session key, the OAuth apps, the public URL or the listen address, and
`worker build` also needs `KFM_BUILD_IMAGE`, `KFM_LADDER_BUILD_DIR` (an absolute path) and
`KFM_LADDER_ARENA`; `serve` needs the full set. A missing or
malformed required variable exits 2 with a line naming it. The variables, their defaults and
the roles that read them are listed in `service/README.md`.
