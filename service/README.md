# kfm-ladder

The Kung Fu Monkeys ladder service (engine architecture v0.63, §11 "M7 step 3 design" and §8
"The ladder"): accounts through GitHub or Google sign-in, portal sessions, API tokens and teams
over Postgres, as JSON routes and a server-rendered portal from one binary (step 3a); then
submissions, the object store, the jobs table and the build worker that turns a submission's
source into a validated module (step 3b). Matches, ratings and replays follow (step 4); their
tables already exist, and `worker match` is a stub until then.

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
| `src/submissions.rs`, `src/manifest.rs`, `src/archive.rs` | Uploads (the manifest, the zip's checks and unpacking), listing, activation. |
| `src/store.rs` | The object store (the S3 API through `rust-s3`, path-style): put, get, delete, presigned GET. |
| `src/jobs.rs`, `src/worker.rs` | The job queue (claim, heartbeat, retry, sweep) and the worker loop. |
| `src/builder.rs`, `src/wasm.rs`, `src/arena.rs` | The build worker: the sandbox run, the module's load checks, the smoke match. |
| `src/retention.rs` | Pruning sources and logs past five finished submissions. |
| `src/error.rs` | `AppError`: every error code, its status and message. |
| `migrations/` | The schema (sqlx migrations, embedded in the binary, run at start). |
| `tests/` | Integration tests over a real Postgres with fake OAuth providers in process. |

## Commands

```sh
kfm-ladder serve           # migrate, then serve the API and the portal on KFM_LADDER_LISTEN
kfm-ladder migrate         # apply pending migrations and exit (needs DATABASE_URL only)
kfm-ladder worker build    # claim build and validate jobs until SIGTERM
kfm-ladder worker match    # claim match jobs until SIGTERM (step 4; each fails as not implemented)
```

Exit codes: 0 done (a worker stopped by SIGTERM), 1 a runtime failure (the database
unreachable for 60 s, a failed migration, the listen address taken), 2 a configuration error
(the variable is named). Workers never migrate; on SIGTERM they stop claiming and finish the
job in hand (`kfm-ladder-build.service` waits 200 s for it). `RUST_LOG` filters the log (default `info`); it is coloured
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
| `KFM_LADDER_S3_ENDPOINT` | serve, workers | yes | `https://<account>.r2.cloudflarestorage.com` | The S3-compatible object store (R2; MinIO in development), addressed path-style. Presigned log URLs point here, so browsers must reach it. |
| `KFM_LADDER_S3_BUCKET` | serve, workers | yes | `kfm-ladder` | |
| `KFM_LADDER_S3_ACCESS_KEY` | serve, workers | yes | | |
| `KFM_LADDER_S3_SECRET_KEY` | serve, workers | yes | | |
| `KFM_LADDER_S3_REGION` | serve, workers | yes | `auto` (R2) | |
| `KFM_LADDER_ENGINE_TAG` | serve, workers | yes | `v0.5.1` | The engine release the season runs (`vX.Y.Z`); the image sets it from its build. |
| `KFM_LADDER_ARENA` | workers | worker build | `/opt/kfm/engine/kfm_arena` | The `kfm_arena` binary of `KFM_LADDER_ENGINE_TAG`: validation's smoke match (and step 4's matches). The image sets it. |
| `KFM_BUILD_IMAGE` | worker build | yes | `ghcr.io/minip8/kfm-bot-build:v0.1.0` | The bot build image each build runs in. |
| `KFM_LADDER_BUILD_DIR` | worker build | yes | `/var/lib/kfm-ladder/build` | An absolute directory at the same path on the host and in the worker's container, under which each job makes `job-<id>/src` and `job-<id>/out` for the sibling `docker run` (removed after the job). |
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

## The build worker

`kfm-ladder worker build` claims `build` and `validate` jobs (`docs/ladder-api.md` §9.6). For
each build it unpacks the submission's entry directory into
`$KFM_LADDER_BUILD_DIR/job-<id>/src` and runs, through the `docker` CLI on `PATH`,

```
docker run --rm --name kfm-build-<job> --network none --memory 2g --cpus 1 --pids-limit 256 \
  --read-only --tmpfs /tmp --user builder -v SRC:/src:ro -v OUT:/out $KFM_BUILD_IMAGE timeout 120 kfm-build
```

then checks the module and plays its smoke match with `$KFM_LADDER_ARENA`. It needs:

- the Docker CLI and a daemon it can reach: the host's socket mounted at
  `/var/run/docker.sock` (`deploy/Dockerfile.ladder` ships the CLI). On the build VM the
  daemon's default runtime is gVisor's `runsc`, so every build runs under gVisor without a
  `--runtime` flag;
- the bot build image (`KFM_BUILD_IMAGE`), present or pullable by that daemon;
- `KFM_LADDER_BUILD_DIR` at the same absolute path for the worker and for the daemon (the
  daemon resolves the `-v` paths on the host), writable by the worker; each job's output
  directory is made world-writable so the container's `builder` (uid 10001) can write the
  module, and the job's directory is removed afterwards;
- `kfm_arena` (`KFM_LADDER_ARENA`; the image bakes the engine tag's in);
- the database and the object store, never the portal's secrets.

Locally, against the compose stack (`deploy/docker-compose.yml`: its `build-worker` service
mounts the Docker socket and the build directory `/tmp/kfm-ladder-build` at the same path, and
its `bot-build` service builds the image `kfm-bot-build:dev` first; the host needs nothing but
Docker and those two paths):

```sh
export KFM_ENGINE_TAG=v0.5.1            # while engine.lock has tag= empty
docker compose -f deploy/docker-compose.yml up -d --build postgres minio bucket bot-build migrate
docker compose -f deploy/docker-compose.yml up --build build-worker
```

or natively from `service/`, with those stores up, the image built once
(`docker build -t kfm-bot-build:dev .` at the repository root) and `tools/fetch_kfm_arena.sh`
run for `.kfm/kfm_arena`:

```sh
export DATABASE_URL=postgres://kfm:kfm@127.0.0.1:5432/kfm
export KFM_LADDER_S3_ENDPOINT=http://127.0.0.1:9000 KFM_LADDER_S3_BUCKET=kfm-ladder
export KFM_LADDER_S3_ACCESS_KEY=kfm-dev KFM_LADDER_S3_SECRET_KEY=kfm-dev-secret KFM_LADDER_S3_REGION=us-east-1
export KFM_LADDER_ENGINE_TAG=v0.5.1 KFM_LADDER_ARENA=$PWD/../.kfm/kfm_arena
export KFM_BUILD_IMAGE=kfm-bot-build:dev KFM_LADDER_BUILD_DIR=/tmp/kfm-ladder-build
mkdir -p $KFM_LADDER_BUILD_DIR && cargo run -- worker build
```

Under runc a submission goes from `queued` to `ready` in a few seconds (the starter example:
about 2 s from upload to ready on a laptop); `docker ps` shows its `kfm-build-<job>` container
while it builds.

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
docker run --rm -d --name kfm-ladder-minio -p 127.0.0.1:9000:9000 -e MINIO_ROOT_USER=kfm-dev \
  -e MINIO_ROOT_PASSWORD=kfm-dev-secret pgsty/minio:RELEASE.2026-08-04T00-00-00Z server /data
docker build -t kfm-bot-build:local .                  # at the repository root, once
tools/fetch_kfm_arena.sh                               # .kfm/kfm_arena
export DATABASE_URL=postgres://postgres:dev@127.0.0.1:5432/postgres
export KFM_LADDER_TEST_S3_ENDPOINT=http://127.0.0.1:9000
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
database and no offline metadata.

The object store tests (`tests/store.rs`, and every upload that gets past its checks) run
against the MinIO named by `KFM_LADDER_TEST_S3_ENDPOINT` (credentials
`KFM_LADDER_TEST_S3_ACCESS_KEY` and `_SECRET_KEY`, default `kfm-dev` and `kfm-dev-secret`; the
bucket `KFM_LADDER_TEST_S3_BUCKET`, default `kfm-ladder-test`, made if missing); without it
they print `skipped: KFM_LADDER_TEST_S3_ENDPOINT is unset` and pass. `tests/build_worker.rs`
runs the real sandbox: it also needs Docker with the image `KFM_LADDER_TEST_BUILD_IMAGE`
(default `kfm-bot-build:local`) and `kfm_arena` (`KFM_LADDER_ARENA`, default
`../.kfm/kfm_arena`), and says which is missing when it skips. The module walker's example test
(`src/wasm.rs`) checks the modules in `KFM_LADDER_TEST_EXAMPLES` (default `../out/local`, where
`tools/build.sh examples/<name> out/local/<name>.wasm` puts them) when that directory exists.

The `service` job of `.github/workflows/ci.yml` runs the same three commands against a
`postgres:17` service container and a MinIO started in a step, with the bot build image built
on the runner and the released `kfm_arena` fetched, so every test runs there.
