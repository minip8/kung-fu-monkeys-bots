# kfm: the Kung Fu Monkeys bot ladder from the command line

`kfm` logs you in to the [Kung Fu Monkeys](https://github.com/minip8/kung-fu-monkeys) bot
ladder, builds a bot with the ladder's own recipe, plays it against the reference bot with the
engine's released `kfm_arena`, submits it, and follows its matches and replays. It is one
static binary; it talks to the ladder over its documented HTTP API
([`docs/ladder-api.md`](https://github.com/minip8/kung-fu-monkeys-bots/blob/main/docs/ladder-api.md))
and nothing else.

## Install

```sh
uv tool install kfm-cli        # or: pipx install kfm-cli
uvx --from kfm-cli kfm --help  # or run it once without installing
```

The PyPI project is **`kfm-cli`** (the name `kfm` is taken on PyPI by an unrelated package);
the command it installs is `kfm`. The wheels hold the binary only, for Linux x86_64 (static,
musl), macOS arm64 and Windows x86_64; the same wheels are on the bots repository's
[releases page](https://github.com/minip8/kung-fu-monkeys-bots/releases). From a checkout of
the repository: `cargo install --path cli`.

`kfm build` needs [Docker](https://docs.docker.com/get-docker/) (or Podman, `DOCKER=podman`)
for the ladder's build image, or a clang that targets wasm32 for `--local`. `kfm test`
downloads its runner itself.

## The five-minute path

You need an account and a team: sign in on the portal, <https://bots.kfm.minip8.com/>, with
GitHub or Google and create a team there (a solo player is a team of one).

```sh
git clone --depth 1 https://github.com/minip8/kung-fu-monkeys-bots.git
cp -r kung-fu-monkeys-bots/examples/starter mybot && cd mybot   # a bot is a directory of sources
kfm login                       # the browser confirms; a `submit` token is stored
kfm build                       # out/bot.wasm, built in the ladder's image and checked
kfm test                        # 10 matches of the ranked 5v5 against ref:hard: wins, faults, fuel
kfm submit --wait --activate    # the ladder builds and validates it; then it is your ranked bot
kfm matches list --team "My Team"   # once the scheduler has paired it (every few minutes)
kfm replay open <match id>      # the game in its own release's browser client
```

Edit `bot.cpp`, `kfm build && kfm test`, repeat; `kfm test --seed 7` plays other seeds. The
SDK (`kfm_bot.hpp` over `kfm_bot.h`) is on the include path of every build, so a bot directory
holds only its own sources.

## Commands

| Command | What |
|---|---|
| `kfm login [--no-browser]` | Opens the ladder's login page, which asks you to authorize "kfm on <this computer>"; stores the `submit` token it creates. `--no-browser` prints the URL and waits (from another machine, forward the printed port with `ssh -L` first). |
| `kfm logout` | Revokes the stored token on the ladder and forgets it. |
| `kfm whoami` | The account the token belongs to and its teams. |
| `kfm build [DIR] [-o OUT.wasm] [--local] [--image IMAGE]` | Builds every `.c`, `.cc` and `.cpp` file directly in DIR (default `.`) into `DIR/out/bot.wasm` with the ladder's recipe, then checks the module as the host will (size, exports, imports, memory). Default: in the pinned build image under Docker, with the ladder's sandbox flags (no network, read-only source, 2 GiB, 120 s); the log goes to `build.log` beside the module. `--local`: with a local clang (`CLANG`, `clang` on `PATH`, `$EMSDK`, `~/emsdk`), byte-identical to the ladder's build with emsdk 6.0.13. |
| `kfm test [MODULE] [--against SPEC] [--matches N] [--mode MODE] [--ffa] [--engine-tag TAG] [--arena PATH] [--seed S] [--ticks N]` | Plays MODULE (default `out/bot.wasm`) in the ranked shape: its five slots against five `ref:hard` in the `contest` mode, `--rotate` so every pair plays both sides, 10 matches. Prints each match, wins, draws and losses, faults (traps, ticks over budget, bad returns, dead slots) and `fuel_max`; exits 1 if the module faulted (the ladder's validation rejects that). `--against` takes `ref:easy`, `ref:medium` or another module; `--ffa` plays a free-for-all against three. The runner is the engine's released `kfm_arena`, downloaded once per release and checked against the release's `SHA256SUMS`; `--arena PATH` uses a local one. |
| `kfm submit [DIR] [--team NAME\|ID] [--entry SUBDIR] [--language cpp] [--activate] [--wait]` | Zips DIR (regular files only; `.git/`, `out/`, `.kfm/`, `*.wasm` and links left out; refused locally past the ladder's 4 MiB zipped, 256 files or 16 MiB), uploads it with its manifest and prints the submission's id and state. `--wait` follows it to `ready`, `failed` or `rejected` (printing the build log on failure, exit 3); `--activate` waits and then makes it the team's ranked bot. The team defaults to your only one. |
| `kfm bots list [--team T]` | The team's submissions: state, active, module hash, ranked games and faults, error. |
| `kfm bots activate <submission> [--team T]` | Makes a ready submission its team's ranked bot. |
| `kfm bots log <submission>` | Its build and validation log. |
| `kfm matches list [--team T] [--season S] [--page N]` | Ranked games, newest first (every team's without `--team`; T is a team id or one of your teams' names). |
| `kfm matches show <match>` | One game: both sides, scores, kills, rating changes, faults, the seed and hashes to re-run it, the viewer URL. |
| `kfm ladder [--season S] [--page N]` | The board: rank, rating and deviation, record. |
| `kfm seasons` | The seasons, the current one marked, and the engine release new games run on. |
| `kfm replay get <match> [-o FILE]` | Downloads the game's `.kfmr` (default `<match>.kfmr`). |
| `kfm replay open <match> [--no-browser]` | Prints the replay's viewer URL (the browser client of the game's own release) and opens it. |

Every command takes `--server URL` and `--json`, which prints the ladder's raw JSON (for
`kfm test`, `kfm_arena`'s JSON lines, with the summary on stderr) instead of the summary.

## Configuration

`kfm login` writes `config.toml`, readable by you only (mode 0600), in the platform's config
directory: `~/.config/kfm/config.toml` on Linux (`$XDG_CONFIG_HOME/kfm/`), `~/Library/Application
Support/kfm/config.toml` on macOS, `%APPDATA%\kfm\config\config.toml` on Windows.

```toml
server = "https://bots.kfm.minip8.com"   # the ladder; the default when absent
token = "kfm_…"                            # the API token kfm login stored (scope submit)
token_id = "0190f0f0-…"                    # so kfm logout can revoke it
token_name = "kfm on laptop"               # its name on the portal
```

The stored token is sent only to the server it was made for: `--server` naming another ladder
runs logged out there (log in to it with `kfm --server URL login`, which replaces the file's
pair). Revoke the token on the portal or with `kfm logout`.

| Variable | Meaning |
|---|---|
| `KFM_SERVER` | The ladder, as `--server`. |
| `KFM_TOKEN` | A token to use instead of the stored one (CI: a `submit` token made on the portal). |
| `KFM_CONFIG` | Another configuration file. |
| `KFM_CACHE_DIR` | Another cache directory (default the platform's: `~/.cache/kfm` on Linux) for the downloaded runners and the SDK of `--local`. |
| `KFM_BUILD_IMAGE` | The build image, as `--image`. |
| `DOCKER` | The container engine (`docker`; `podman` takes the same flags). |
| `CLANG`, `EMSDK` | The compiler of `kfm build --local`. |
| `KFM_SDK_DIR` | An `sdk/` directory to compile against instead of the one built into kfm. |
| `KFM_ARENA` | A local `kfm_arena`, as `--arena`. |
| `KFM_RELEASE_URL`, `KFM_ENGINE_REPO` | Where runners are downloaded from (a mirror; another GitHub repository than `minip8/kung-fu-monkeys`). |

## Pins

Two releases are fixed when the binary is built (`cli/build.rs`), so a given `kfm` builds and
plays the way its release does:

- **The build image**: `ghcr.io/minip8/kfm-bot-build:<tag>`, `<tag>` this kfm's release: the
  `KFM_BOTS_TAG` environment variable at build time (the release workflow sets it to the tag
  it builds, after checking it is `v` + the crate version), else `v` + the version in
  `Cargo.toml`. `kfm --version` names the version; `--image` or `KFM_BUILD_IMAGE` runs another
  image.
- **The engine release** of `kfm test` when the ladder names none: `KFM_ENGINE_TAG` at build
  time, else `tag=` of the repository's `engine.lock`. Logged in, `kfm test` asks the ladder
  which release new games run on (`GET /seasons`' `engine_tag`) and plays that one, so you see
  a release's changes before the scheduler does; `--engine-tag` names one yourself.

The SDK headers `kfm build --local` compiles against are the repository's `sdk/` at build time,
embedded in the binary.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Done. |
| 1 | An error: the ladder's, shown as `kfm: <message> (<code>)` (the codes of `docs/ladder-api.md` §2), or a failed build, download or match, or a module that faulted in `kfm test`. |
| 2 | Usage: a bad flag, a missing module or directory, several teams and no `--team` (`ambiguous`). |
| 3 | `kfm submit --wait` (or `--activate`): the submission failed to build or was rejected by validation. |

## Development

```sh
cd cli
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
uvx maturin build --release        # the wheel, in target/wheels/
```

The tests run the real binary against a fake ladder in process (`tests/ladder.rs`); the
toolchain tests (`tests/toolchain.rs`) build `examples/` in the image (`KFM_TEST_BUILD_IMAGE`,
default `kfm-bot-build:local`) and with a local emsdk clang, and play them with a `kfm_arena`
(`KFM_TEST_ARENA`, default `../.kfm/kfm_arena`), each skipping with a message when its tool is
missing.
