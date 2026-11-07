# kfm-ladder

The Kung Fu Monkeys ladder service (engine architecture v0.63, §11 "M7 step 3 design" and §8
"The ladder"): accounts through GitHub or Google sign-in, portal sessions, API tokens and teams
over Postgres, as JSON routes and a server-rendered portal from one binary. Step 3a, this
crate's first version, serves those; submissions, the build and match workers, the object store,
ratings and replays follow (3b, 4), and their tables already exist.

The HTTP surface is normative in [`docs/ladder-api.md`](../docs/ladder-api.md); change it
there first.

| Path | What |
|---|---|
| `src/main.rs` | The CLI: `serve`, `migrate`, `worker build`, `worker match`. |
| `src/config.rs` | The environment, per role (`Config` for `serve`, `WorkerConfig`, `database_url_from`). |
| `src/oauth.rs` | GitHub (`oauth2`) and Google (`openidconnect`, discovery) sign-in, linking, logout. |
| `src/auth.rs` | Session cookies, the `Caller` extractor (session or bearer), CSRF. |
| `src/tokens.rs`, `src/teams.rs`, `src/users.rs` | The JSON routes and the functions the portal shares. |
| `src/portal.rs`, `templates/index.html` | The portal (askama, no JavaScript). |
| `src/error.rs` | `AppError`: every error code, its status and message. |
| `migrations/` | The schema (sqlx migrations, embedded in the binary, run at start). |
| `tests/` | Integration tests over a real Postgres with fake OAuth providers in process. |

## Commands

```sh
kfm-ladder serve           # migrate, then serve the API and the portal on KFM_LADDER_LISTEN
kfm-ladder migrate         # apply pending migrations and exit (needs DATABASE_URL only)
kfm-ladder worker build    # step 3b; prints "not implemented" and exits 2
kfm-ladder worker match    # step 3b; prints "not implemented" and exits 2
```

Exit codes: 0 done, 1 a runtime failure (the database unreachable for 60 s, a failed
migration, the listen address taken), 2 a configuration error (the variable is named) or a
worker that does not exist yet. `RUST_LOG` filters the log (default `info`); it is coloured
only on a terminal. Build with `--no-default-features` to leave Sentry out entirely. Caddy
terminates TLS in front of `KFM_LADDER_LISTEN`; `serve` stops cleanly on SIGTERM.

## Configuration

The environment is the only configuration (no file). An empty value counts as unset. Each role
reads only its own variables: `migrate` only `DATABASE_URL` (and `SENTRY_DSN`); the workers
never the session key, the OAuth apps, the public URL or the listen address, so a worker host
holds no portal secret; `serve` the set marked for it.

| Variable | Roles | Required | Example or default | What |
|---|---|---|---|---|
| `DATABASE_URL` | all | yes | `postgres://kfm:pw@127.0.0.1:5432/kfm` | Postgres. |
| `KFM_LADDER_LISTEN` | serve | no | `127.0.0.1:8090` (default) | Plain-HTTP listen address; `/healthz` answers here. |
| `KFM_LADDER_PUBLIC_URL` | serve | yes | `https://ladder.kfm.minip8.com` | The public origin (no path): OAuth redirect URIs, absolute links, and `Secure` cookies when https. |
| `KFM_LADDER_SESSION_KEY` | serve | yes | 64 hex characters (`openssl rand -hex 32`) | The HMAC-SHA256 key of the session and OAuth state cookies. Rotating it signs everyone out. |
| `KFM_LADDER_GITHUB_CLIENT_ID` | serve | yes | | The GitHub OAuth App. |
| `KFM_LADDER_GITHUB_CLIENT_SECRET` | serve | yes | | |
| `KFM_LADDER_GOOGLE_CLIENT_ID` | serve | yes | `….apps.googleusercontent.com` | The Google OAuth client. |
| `KFM_LADDER_GOOGLE_CLIENT_SECRET` | serve | yes | | |
| `KFM_LADDER_S3_ENDPOINT` | serve, workers | yes | `https://<account>.r2.cloudflarestorage.com` | The S3-compatible object store (R2; MinIO in development). Read and validated now, used from 3b. |
| `KFM_LADDER_S3_BUCKET` | serve, workers | yes | `kfm-ladder` | |
| `KFM_LADDER_S3_ACCESS_KEY` | serve, workers | yes | | |
| `KFM_LADDER_S3_SECRET_KEY` | serve, workers | yes | | |
| `KFM_LADDER_S3_REGION` | serve, workers | yes | `auto` (R2) | |
| `KFM_LADDER_ENGINE_TAG` | serve, workers | yes | `v0.5.1` | The engine release the season runs (`vX.Y.Z`); the image sets it from its build. |
| `KFM_LADDER_ARENA` | workers | no | `/opt/kfm/engine/kfm_arena` | The `kfm_arena` binary; used by the workers (3b). The image sets it. |
| `KFM_BUILD_IMAGE` | worker build | no | `ghcr.io/minip8/kfm-bot-build:v0.1.0` | The bot build image; used by the build worker (3b). |
| `KFM_LADDER_BUILD_DIR` | worker build | no | `/var/lib/kfm-ladder/build` | A directory at the same path on the host and in the worker's container, under which a build's source and output directories are made for the sibling `docker run`; used by the build worker (3b). |
| `SENTRY_DSN` | all | no | | Enables Sentry: error events, with info and warnings as breadcrumbs. |
| `KFM_LADDER_GITHUB_AUTH_URL` | serve | no | `https://github.com/login/oauth/authorize` | Test override. |
| `KFM_LADDER_GITHUB_TOKEN_URL` | serve | no | `https://github.com/login/oauth/access_token` | Test override. |
| `KFM_LADDER_GITHUB_API_URL` | serve | no | `https://api.github.com` | Test override (`/user`, `/user/emails` are appended). |
| `KFM_LADDER_GOOGLE_ISSUER` | serve | no | `https://accounts.google.com` | Test override: the OIDC issuer, the discovery base; must equal the id tokens' `iss` byte for byte. |

## Run locally

```sh
docker run --rm -d --name kfm-ladder-pg -p 5432:5432 -e POSTGRES_PASSWORD=dev postgres:17
cd service
export DATABASE_URL=postgres://postgres:dev@127.0.0.1:5432/postgres
export KFM_LADDER_PUBLIC_URL=http://127.0.0.1:8090
export KFM_LADDER_SESSION_KEY=$(openssl rand -hex 32)
export KFM_LADDER_GITHUB_CLIENT_ID=... KFM_LADDER_GITHUB_CLIENT_SECRET=...
export KFM_LADDER_GOOGLE_CLIENT_ID=... KFM_LADDER_GOOGLE_CLIENT_SECRET=...
export KFM_LADDER_S3_ENDPOINT=http://127.0.0.1:9000 KFM_LADDER_S3_BUCKET=kfm-ladder
export KFM_LADDER_S3_ACCESS_KEY=minio KFM_LADDER_S3_SECRET_KEY=minio-secret KFM_LADDER_S3_REGION=auto
export KFM_LADDER_ENGINE_TAG=v0.5.1
cargo run -- serve
curl http://127.0.0.1:8090/healthz      # {"db":true,"ok":true}
```

Then open `http://127.0.0.1:8090/`. Any non-empty values start the service; signing in needs
real OAuth apps whose callback is on `KFM_LADDER_PUBLIC_URL` (below; both providers accept a
`http://127.0.0.1` or `http://localhost` callback for development). Cookies are `Secure` only
when the public URL is https, so a plain-http development origin works.

## Registering the OAuth apps

**GitHub.** Settings > Developer settings > OAuth Apps > New OAuth App (under the organisation
for production). Homepage URL: `<PUBLIC_URL>`; Authorization callback URL:
`<PUBLIC_URL>/auth/github/callback`. Generate a client secret; the client id and secret are
`KFM_LADDER_GITHUB_CLIENT_ID` and `KFM_LADDER_GITHUB_CLIENT_SECRET`. The service asks for
`read:user user:email` and keys the account on the numeric GitHub user id.

**Google.** Google Cloud Console > APIs & Services. OAuth consent screen: user type External,
the app name and support email, the scopes `openid`, `email` and `profile` (non-sensitive, so no
verification review), then publish it (in Testing only the listed test users can sign in).
Credentials > Create credentials > OAuth client ID > Web application; Authorised redirect URI:
`<PUBLIC_URL>/auth/google/callback`. The client id and secret are
`KFM_LADDER_GOOGLE_CLIENT_ID` and `KFM_LADDER_GOOGLE_CLIENT_SECRET`. The service discovers
Google's endpoints and keys from `https://accounts.google.com/.well-known/openid-configuration`
and keys the account on the id token's `sub`.

## Test

```sh
docker run --rm -d --name kfm-ladder-pg -p 5432:5432 -e POSTGRES_PASSWORD=dev postgres:17
export DATABASE_URL=postgres://postgres:dev@127.0.0.1:5432/postgres
cargo fmt --check --manifest-path service/Cargo.toml
cargo clippy --manifest-path service/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path service/Cargo.toml --locked
```

Every integration test is a `#[sqlx::test]`: sqlx creates a fresh database per test on the
server `DATABASE_URL` names (the user must be allowed to create databases), applies
`migrations/`, and drops it afterwards, so the tests run in parallel. `tests/common/mod.rs`
starts the service in process on an ephemeral port and a fake GitHub (authorize, token, `/user`,
`/user/emails`) and fake Google (discovery, JWKS, authorize, token with an id token signed by
`tests/fixtures/test_rsa_key.pem`, a key for tests only) on another, checks PKCE and the client
credentials as the real providers do, and drives the flows with a cookie-keeping client that
follows no redirect. No compile-time-checked query macros are used, so building needs no
database and no offline metadata. The `service` job of `.github/workflows/ci.yml` runs the same
three commands against a `postgres:17` service container.
