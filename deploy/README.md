# Deploying the ladder (M7 step 3)

The decision record is the engine's architecture §11, "M7 step 3 design (v0.63)" ("Deployment"),
with §8 "The ladder" ("The build sandbox") and §13.5 "The M6a deployment", whose pattern this
follows: images on GHCR, systemd units, a Caddyfile and scripted VMs, no provider-specific
service. The engine's own runbook (its `deploy/README.md`) covers the page, the lobby and the
game servers; this one covers the ladder, and the two read as one. The HTTP surface is
`docs/ladder-api.md`.

| Part | What | Where |
|---|---|---|
| API and portal | `kfm-ladder serve` behind Caddy (TLS from Let's Encrypt), the only public endpoint | `kfm-ladder-1`, `ghcr.io/<owner>/kfm-ladder` |
| Database | Postgres 17 in a container, data in `/var/lib/kfm/postgres`, dumped nightly to the bucket | `kfm-ladder-1`, `postgres:17` |
| Match workers | `kfm-ladder worker match` × N, each match one `kfm_arena` process | `kfm-ladder-1`, the same image |
| Build worker | `kfm-ladder worker build`: each submission built in a throwaway `kfm-bot-build` container under gVisor | `kfm-build-1`, the same image plus `ghcr.io/<owner>/kfm-bot-build` |
| Object store | sources, modules, build logs, replays, database dumps | Cloudflare R2 (any S3-compatible store; MinIO in development) |

Files:

- `Dockerfile.ladder`: the service image, built from the repository root; it carries the
  engine's released `kfm_arena` of `engine.lock`'s `tag=` ("The engine release" below). The
  bot build image is the repository's `Dockerfile` (`docs/build.md`).
- `docker-compose.yml`: the whole stack on one machine against MinIO, for development.
- `vm/`: what a VM runs. Docker runs the images under systemd (`kfm-postgres.service`,
  `kfm-ladder-api.service`, `kfm-ladder-match@.service`, `kfm-ladder-build.service`, the
  dump's `kfm-postgres-backup.service` and `.timer`), configured by `/etc/kfm/ladder.env`
  (`ladder.env.example`); Caddy (`Caddyfile`) on the ladder VM; `install.sh`,
  `install-gvisor.sh` (the build VM), `update.sh`, `postgres-backup.sh`.
- `gcp/`: `create_ladder_vm.sh`, `create_build_vm.sh`, `common.sh`.
- `hetzner.md`: the same two VMs on Hetzner Cloud.

## What runs where

Each unit hands its container only the variables its role needs, so the two VMs' env files
differ (`ladder.env.example` marks each line `[ladder]` or `[build]`):

| Role | Container | Gets |
|---|---|---|
| `serve` (+ `migrate` before it) | unprivileged user, read-only root, no capabilities, host network on loopback | the database, the portal's URL, the session key, both OAuth clients, the object store, Sentry |
| `worker match` | the same, plus a 768 MiB memory cap and a process cap | the database, the object store, Sentry |
| `worker build` | root, `--runtime runc`, the Docker socket and `/var/lib/kfm-ladder/build` mounted | the database, the object store, Sentry, `KFM_BUILD_IMAGE`, `KFM_LADDER_BUILD_DIR` |

The build VM holds no OAuth secret and no session key: a compromise there cannot sign anyone
in. The build worker runs as a container with the host's Docker socket rather than natively:
both are root on the VM (whoever can start containers can mount `/`), so running it natively
would protect nothing more and would need a second install path; as a container it is the same
image, unit and `update.sh` as everything else. The socket is why the build VM holds nothing
but the worker, and why the builds themselves run under gVisor ("What gVisor protects
against"). The worker names its build directories in the sandbox's `-v SRC:/src:ro -v
OUT:/out`, which the daemon resolves on the host, so `/var/lib/kfm-ladder/build` is mounted at
the same path inside and outside (`KFM_LADDER_BUILD_DIR`, and `TMPDIR` too).

## Day one

1. **The bucket.** In the Cloudflare dashboard, R2 → Create bucket (`kfm-ladder`, automatic
   location). Then R2 → Manage API tokens → Create API token: *Object Read & Write*, applied to
   that bucket only. It shows an access key id and a secret access key once: they are
   `KFM_LADDER_S3_ACCESS_KEY` and `KFM_LADDER_S3_SECRET_KEY`; the endpoint is
   `https://<account id>.r2.cloudflarestorage.com` (`KFM_LADDER_S3_ENDPOINT`), the region
   `auto`. The bucket stays private: the service hands out presigned URLs. For replays viewed
   from `https://kfm.minip8.com`, add a CORS policy on the bucket allowing `GET` from that
   origin (R2 → the bucket → Settings → CORS policy).
2. **GitHub sign-in.** github.com → Settings → Developer settings → OAuth Apps → New OAuth App
   (under the `minip8` organisation's settings, so it outlives one account): homepage
   `https://ladder.kfm.minip8.com`, authorization callback URL
   `https://ladder.kfm.minip8.com/auth/github/callback`. Generate a client secret:
   `KFM_LADDER_GITHUB_CLIENT_ID` and `_SECRET`. A development app has the callback
   `http://127.0.0.1:8090/auth/github/callback` (an OAuth app has one callback URL, so
   development gets its own app).
3. **Google sign-in.** Google Cloud console (any project; the VMs' is fine) → APIs & Services →
   OAuth consent screen: External, the app name, the support email, scopes `openid`, `email`,
   `profile`; publish it (in testing only listed test users can sign in). Then Credentials →
   Create credentials → OAuth client ID, type *Web application*, authorised redirect URI
   `https://ladder.kfm.minip8.com/auth/google/callback` (add
   `http://127.0.0.1:8090/auth/google/callback` for development; Google takes several):
   `KFM_LADDER_GOOGLE_CLIENT_ID` and `_SECRET`.
4. **The engine release.** `engine.lock`'s `tag=` must name an engine release with
   `kfm_arena` assets (v0.5.1 or later; bumping it is `docs/build.md` "The engine pin"):
   `Dockerfile.ladder` bakes that release's runner in and fails while `tag=` is empty.
5. **First release.** `git tag v0.1.0 && git push origin v0.1.0`. `release.yml` pushes
   `ghcr.io/<owner>/kfm-ladder:v0.1.0` and `kfm-bot-build:v0.1.0` (plus `latest`). GHCR
   packages start private: make both public (package settings), or put a `read:packages`
   token in `KFM_GHCR_USER`/`KFM_GHCR_TOKEN` in each env file.
6. **The env files.** Two, kept out of git:
   ```sh
   cp deploy/vm/ladder.env.example ladder.env   # every [ladder] line
   cp deploy/vm/ladder.env.example build.env    # the [build] lines only; empty the rest
   openssl rand -hex 24   # POSTGRES_PASSWORD (in ladder.env, and inside both DATABASE_URLs)
   openssl rand -hex 32   # KFM_LADDER_SESSION_KEY (ladder.env only)
   ```
   `ladder.env`: `DATABASE_URL=postgres://kfm:<password>@127.0.0.1:5432/kfm`;
   `build.env`: `DATABASE_URL=postgres://kfm:<password>@kfm-ladder-1:5432/kfm` (GCP's internal
   DNS resolves the VM name inside the project). Both: `KFM_IMAGE_TAG=v0.1.0` and the bucket's
   four values. Passing a file to the create scripts embeds it in the VM's startup-script
   metadata, which anyone with read access to the project can see; for a stricter setup leave
   `--env-file` out and edit `/etc/kfm/ladder.env` over `gcloud compute ssh`.
7. **The VMs.** In the engine's GCP project (or any other with Compute Engine enabled):
   ```sh
   deploy/gcp/create_ladder_vm.sh --project my-proj --zone australia-southeast1-b --env-file ladder.env
   deploy/gcp/create_build_vm.sh  --project my-proj --zone australia-southeast1-b --env-file build.env
   ```
   The first reserves the static IP `kfm-ladder-1-ip`, creates the firewall rules
   `kfm-ladder-web` (TCP 80/443 to tag `kfm-ladder`) and `kfm-ladder-postgres` (TCP 5432 to
   tag `kfm-ladder` from tag `kfm-build` only) and an `e2-small` on Debian 12 whose startup
   script runs `install.sh --ladder`: Docker, Caddy, Postgres, the API (which migrates the
   empty database to the release's schema before it serves) and two match workers. The
   second creates an `e2-medium` with tag `kfm-build` and no rule of its own whose startup
   script runs `install.sh --build`: Docker, gVisor as the default runtime (checked), the
   build worker. Progress: `gcloud compute ssh NAME --zone Z -- sudo journalctl -u
   google-startup-scripts -f`. The default network's `default-allow-ssh` (and
   `default-allow-rdp`) apply to every VM until deleted: after step 10, delete them and the
   build VM has no inbound port but IAP's SSH.
8. **DNS.** An A record `ladder.kfm.minip8.com` → the ladder VM's printed IP, *DNS only*
   (grey cloud): Caddy answers the ACME HTTP challenge itself and retries until the name
   resolves.
9. **The schema.** The API's unit runs `kfm-ladder migrate` before every start, so a fresh
   database is migrated on first boot and every release's schema is applied by its update.
   To run it by hand, `sudo systemctl restart kfm-ladder-api` on the ladder VM, or the step
   alone (`docker --env-file` reads `ladder.env` literally, which is right for it):
   ```sh
   sudo docker run --rm --network host --env-file /etc/kfm/ladder.env \
     ghcr.io/minip8/kfm-ladder:v0.1.0 migrate
   ```
10. **Automatic updates.** The engine's `deploy/gcp/setup_deploy_identity.sh`, run from an
    engine checkout for this repository, adds a second provider to the same pool and service
    account (its first provider admits only the engine repository):
    ```sh
    deploy/gcp/setup_deploy_identity.sh --project my-proj --repo minip8/kung-fu-monkeys-bots \
      --provider github-bots
    ```
    Set the `GCP_*` lines it prints on this repository (`gh variable set ... -R
    minip8/kung-fu-monkeys-bots`), but not its `KFM_VMS` (that lists every `kfm-*` VM, the
    game servers included); instead, ladder VM first:
    ```sh
    gh variable set KFM_LADDER_VMS -R minip8/kung-fu-monkeys-bots \
      --body 'kfm-ladder-1:australia-southeast1-b kfm-build-1:australia-southeast1-b'
    ```
    Run it after the VMs exist: it grants act-as on the service accounts the `kfm-*` VMs use.
11. **Check.** `curl https://ladder.kfm.minip8.com/healthz`; sign in on the portal with both
    providers; on the build VM, the gVisor check below; submit an example bot
    (`examples/starter`) and watch it go queued → building → ready
    (`journalctl -u kfm-ladder-build` on `kfm-build-1`).

## Updating

Tag a release (`v0.2.0`) and `release.yml` does the rest: it pushes both images, then its
`vms` job ships `deploy/vm/` to every VM in the repository variable `KFM_LADDER_VMS` over an
IAP SSH tunnel and runs `update.sh v0.2.0` on each, in the listed order. `update.sh` pulls the
tag, refreshes the units and the Caddyfile from the shipped copy, writes the tag into
`ladder.env` and restarts at once: on the ladder VM the API first (its `migrate` brings the
schema to the release's before any new worker runs), then the match workers one at a time; on
the build VM the build worker. A job a restarted worker had claimed goes stale and is
re-queued, so an update loses no work; an API restart drops the requests in flight. Postgres is
never restarted or pulled by an update. Watch it under Actions → Release. A manual
`workflow_dispatch` run pushes `sha-<short>` images and never touches the VMs.

By hand, for one VM or when the workflow cannot reach it (ladder VM first):

```sh
gcloud compute ssh kfm-ladder-1 --zone australia-southeast1-b --tunnel-through-iap -- \
  sudo /opt/kfm-ladder-deploy/vm/update.sh v0.2.0
gcloud compute ssh kfm-build-1 --zone australia-southeast1-b --tunnel-through-iap -- \
  sudo /opt/kfm-ladder-deploy/vm/update.sh v0.2.0
```

To change the number of match workers, set `KFM_MATCH_WORKERS` in `ladder.env` and re-run
`sudo /opt/kfm-ladder-deploy/vm/install.sh --ladder`. The VM's startup script re-extracts the
`deploy/vm/` embedded at creation on every boot, so units changed by a release hold until the
next reboot; to make them permanent, recreate the VM (or update its startup-script metadata).

Logs: `journalctl -u kfm-ladder-api -u 'kfm-ladder-match@*' -u kfm-postgres -u caddy` on the
ladder VM, `journalctl -u kfm-ladder-build` on the build VM.

## The engine release

The ladder plays every match with the `kfm_arena` baked into its image: the one of
`engine.lock`'s `tag=`, downloaded at build time and checked against that release's
`SHA256SUMS` by `tools/fetch_kfm_arena.sh`, the script authors use. The image's entrypoint
exports it as `KFM_LADDER_ENGINE_TAG` (the tag recorded on every match) with
`KFM_LADDER_ARENA=/opt/kfm/engine/kfm_arena`. It is baked in rather than fetched at container
start so that every container of one image runs the same verified bytes, a worker starts
without reaching GitHub, and the containers keep a read-only root. Moving the season to a new
engine release is the pin bump of `docs/build.md` (`engine.lock`'s `tag=` and `commit=`, the
SDK header) and a release of this repository; the env files never name the engine. A
container whose env sets `KFM_LADDER_ENGINE_TAG` to another tag while using the baked runner
refuses to start, so the recorded tag and the runner can never disagree.

## Backups and restore

The database is the only state on a VM disk: the blobs are in the bucket already. Every night
at 03:30 UTC (`kfm-postgres-backup.timer`, caught up after downtime) `postgres-backup.sh dump`
streams `pg_dump -Fc` from the `kfm-postgres` container into
`<bucket>/backups/postgres/kfm-<UTC time>.dump` through rclone in its own container (no tool on
the host, S3 credentials passed by name, never on a command line), then deletes dumps older
than `KFM_BACKUP_KEEP_DAYS` (30). rclone rather than the `aws` CLI because it speaks to R2 and
MinIO by provider name with no checksum or region quirks, and streams an upload of unknown
size.

```sh
sudo systemctl start kfm-postgres-backup             # a dump now (journalctl -u kfm-postgres-backup)
sudo /opt/kfm-ladder-deploy/vm/postgres-backup.sh list
```

**Restore** (into the running Postgres of a ladder VM, an old one or a fresh one):

1. On the build VM: `sudo systemctl stop kfm-ladder-build`.
2. On the ladder VM: `sudo /opt/kfm-ladder-deploy/vm/postgres-backup.sh restore
   kfm-20261107T033000Z.dump --yes`. It stops the API and the match workers, runs
   `pg_restore --clean --if-exists --single-transaction` (a failed restore changes nothing),
   and starts them again; the API migrates the restored schema forward if the dump is older
   than the release.
3. On the build VM: `sudo systemctl start kfm-ladder-build`.

A whole lost VM is `create_ladder_vm.sh` again (the static IP is kept), then the restore. The
dumps live in the same bucket under the same token as the service's blobs; a second copy
elsewhere (`rclone copy` to another store, monthly) protects against losing the bucket or the
token.

**Postgres updates.** The image stays `postgres:17`. A minor update (17.x) is `sudo docker
pull postgres:17 && sudo systemctl restart kfm-postgres` (the API and workers reconnect). A
major one is a dump, a new data directory and a restore: take a dump, stop the units, move
`/var/lib/kfm/postgres` aside, change the tag in `kfm-postgres.service`, start it (an empty
database), then `restore`.

## What gVisor protects against

A build compiles code an anonymous author wrote with a large C++ compiler; the §8 flags (no
network, a read-only root, an unprivileged user, caps on memory, CPU, processes and wall time)
bound what the build can do inside its container, but a plain container shares the host's
kernel, so one kernel bug reachable from inside (a container escape) gives the VM. Under
gVisor (`runsc`, Docker's default runtime on `kfm-build-1`) the container's system calls are
served by gVisor's own user-space kernel, itself confined by seccomp to a small set of host
calls: an exploit has to break gVisor first and then the host from that narrow surface. What
it does not cover: the build worker itself (outside gVisor, with the Docker socket) unpacks
each submission's archive, so the worker must treat the archive as hostile (no paths outside
its directory, a size cap); and the build VM still holds the database URL and the bucket keys,
which is why it holds nothing else. Builds run noticeably slower under gVisor than under runc;
the 120 s limit has room for it.

The check that it is active, on the build VM:

```sh
sudo docker run --rm debian:trixie-slim dmesg    # gVisor's boot log: "Starting gVisor..."
```

(under runc the same command prints the host's log or is refused); `sudo docker info --format
'{{.DefaultRuntime}}'` prints `runsc`. `install-gvisor.sh` runs both checks on every boot.

## Local development

```sh
KFM_ENGINE_TAG=v0.5.1 docker compose -f deploy/docker-compose.yml up --build
curl http://127.0.0.1:8090/healthz
```

Postgres 17, MinIO with the bucket `kfm-ladder` created at start, the bot build image, and the
service as `migrate`, `serve`, a match worker and a build worker (with this machine's Docker
socket, under runc), all on the host network. The portal is `http://127.0.0.1:8090`, the MinIO
console `http://127.0.0.1:9001` (user `kfm-dev`, password `kfm-dev-secret`), Postgres
`postgres://kfm:kfm@127.0.0.1:5432/kfm` (`KFM_DEV_POSTGRES_PORT=N` moves it when 5432 is
taken). Sign-in needs development OAuth apps (Day one, steps 2 and 3) exported as
`KFM_LADDER_GITHUB_CLIENT_ID` and so on before `up`. `KFM_ENGINE_TAG` is needed only while
`engine.lock` has `tag=` empty. `docker compose -f deploy/docker-compose.yml down -v` drops the
data.

## Cost

On GCP at list prices (check the current ones; Sydney is about a fifth dearer than the US
regions): the `e2-small` (about 13 USD a month) and its static IPv4 (about 3.6 USD), the
`e2-medium` (about 25 USD) and its in-use ephemeral IPv4 (about 3.6 USD), 50 GB of balanced
disk (about 5 USD): roughly 50 USD a month for the two VMs, covered by the credits while they
last. Egress is small: replays go to browsers from R2, not from the VMs. R2's free tier (10 GB
stored, a million writes and ten million reads a month, no egress fee) holds a season of
sources, modules, logs, replays and a month of dumps. Sentry's free tier and Let's Encrypt
cost nothing. No spot or preemptible VMs: they are stopped mid-job. On Hetzner (`hetzner.md`)
the two VMs are a few euros a month each.
