# Kung Fu Monkeys bots

Bots for [Kung Fu Monkeys](https://github.com/minip8/kung-fu-monkeys), an arena brawler in which
monkeys throw bananas that fly like boomerangs. This repository holds the bot SDK, example bots,
the build recipe, the ladder service and `kfm`, the ladder's command line tool. The game repository is the
engine: it hosts bots, runs matches (`kfm_arena`) and owns the normative bot ABI.

## What a bot is

A bot is one core WebAssembly module (no WASI, no threads) behind four exports:
`kfm_abi_version` (returns 2), `kfm_obs_buffer` (where the host writes the observation),
`kfm_init(seed, slot)` and `kfm_act(tick)`, which returns the monkey's input for that tick packed
into 40 bits (stick direction and magnitude, aim, buttons, sub-tick). Before every `kfm_act` the
host writes an `Observation` v2 into the module's buffer: a fixed 5880-byte layout with the own
monkey, every monkey, banana and pickup, the arena's walls and hazards, the match clock and
scores, and the bot's input delay. The host runs one instance per monkey, each with its own
memory, and meters every `kfm_act` at 100 000 wasm instructions.

A module is a pure function of its seed and its observations: no clock, no randomness but the
seed, no I/O, so a match replays bit for bit. Every SDK helper is integer arithmetic (Q16.16
fixed point, the engine's sine table) for the same reason.

The normative contract is the engine's
[`docs/bot-abi.md`](https://github.com/minip8/kung-fu-monkeys/blob/main/docs/bot-abi.md): the
exports, the observation field by field (§2), what a bot sees under fog and hears in calls
(§3), and how to build a module (§5). Where this repository and that document disagree, the
document is right.

## Language

The only ladder language is C++. The ladder builds every bot from source with this
repository's recipe: the emsdk `clang++` over every source file of the bot's directory in one
command, C++20 with no exceptions and no RTTI, freestanding, against the SDK and nothing else.
There are no third-party packages: the SDK has no dependencies, so a build is offline. C
compiles too (the C header is C11 that compiles as C++), and a directory of C sources alone is
built as C.

## Layout

| Path | What |
|---|---|
| `sdk/c/kfm_bot.h` | The C SDK: one freestanding header with the `Observation` v2 structs (layout checked by `static_assert`), the input packing, Q16.16 math, the engine's sine table, atan2 and Rng, and the banana flight step, all bit for bit with the engine. A byte-identical copy of the engine's `tools/bots/sdk-c/kfm_bot.h` (`sdk/c/ENGINE_SOURCE.txt`); never edit it here. |
| `sdk/cpp/kfm_bot.hpp` | The C++ layer over it: `Fixed` and `Vec2` with operators, views over the observation that iterate only what is there, an `Input` builder, the `KFM_BOT` macro that defines the exports, and helpers (nearest enemy and banana, flight prediction and time to impact, "will this hit me", the dash-catch direction, a clear throwing line, the grid's cells and walking). Every helper is documented in the header. |
| `examples/` | Example bots, one directory each: `idle`, `chaser` (C), `starter` (C++, the one to copy) and `holder` (C++, the starter that stays on the hill). `examples/README.md` says what each does and how to build and run them. |
| `tools/build.sh` | The build recipe: `tools/build.sh <bot dir> <out.wasm>`. |
| `tools/fetch_kfm_arena.sh` | Fetches the engine's released headless `kfm_arena` for the pinned engine into `.kfm/`. |
| `tools/contest_maps.py` | Draws the contest's arenas from an engine checkout's content, for the site's contest guide. |
| `docs/build.md` | The recipe in full, the build sandbox, the engine pin and CI. |
| `service/` | The ladder service, `kfm-ladder` (Rust): accounts, tokens, teams, submissions built in the sandbox, seasons, scheduled matches, Glicko-2 ratings, replays and the ladder. `service/README.md` says how to run and test it. |
| `docs/ladder-api.md` | The ladder's HTTP API, normative: every route, its auth, its JSON and its errors. |
| `cli/` | `kfm`, the command line tool (Rust; PyPI `kfm-cli`): log in, build, test, submit, matches, replays. `cli/README.md` documents every command. |

## Quick start

With the `kfm` command line tool ([`cli/README.md`](cli/README.md)): it builds in the
ladder's own image (Docker) or with a local clang, plays with the engine's released runner and
talks to the ladder at <https://bots.kfm.minip8.com/>, where you sign in with GitHub or Google
and create a team first.

```sh
uv tool install kfm-cli                 # the PyPI name; the command is kfm (pipx install kfm-cli works too)
git clone --depth 1 https://github.com/minip8/kung-fu-monkeys-bots.git
cp -r kung-fu-monkeys-bots/examples/starter mybot && cd mybot   # a bot is a directory of sources
kfm login                               # the browser confirms; a submit token is stored
kfm build                               # out/bot.wasm with the ladder's recipe, checked like the host does
kfm test                                # 10 ranked-shape 5v5 matches against ref:hard: wins, faults, fuel
kfm submit --wait --activate            # built and validated by the ladder, then your ranked bot
```

Iterate: change the bot, `kfm build && kfm test` (another `--seed` now and then), submit again.
`kfm matches list`, `kfm ladder` and `kfm replay open <match>` follow it on the ladder.

### Without the CLI

The scripts under `tools/` do the same from a checkout (contributors, CI). You need a clang
that targets wasm32 with `wasm-ld`; the ladder's is emsdk's (`emsdk install 6.0.13 && emsdk
activate 6.0.13`; `docs/build.md` says why that version).

```sh
cd kung-fu-monkeys-bots
cp -r examples/starter mybot                      # a bot is a directory of sources
tools/build.sh mybot out/mybot.wasm               # compile and check the module
tools/fetch_kfm_arena.sh                          # the engine's runner into .kfm/
.kfm/kfm_arena --bot out/mybot.wasm --bot ref:hard --matches 10                           # one on one
m=out/mybot.wasm; .kfm/kfm_arena --mode contest --team $m,$m,$m,$m,$m --team ref:hard --matches 10 --rotate   # 5v5, as ranked
```

`--team` takes one spec per slot, and a shorter list repeats its last spec up to the longer
team's size: `--team out/mybot.wasm --team ref:hard` is one on one, and the five-slot list above
is the ladder's 5v5 (`ref:hard` fills the other five).

The last line of `kfm_arena`'s output sums the matches up: wins, kills and deaths per bot,
and for a module its substitutions, `traps`, `over` (ticks over budget) and `fuel_max` (the
most instructions one `kfm_act` used). `--json` prints one JSON object per match (the engine's
`docs/arena-result.md`), `--replay out/m.kfmr` writes a replay the game plays back, and `--log`
shows what the bot passed to `kfm_log`. `tools/build_kfm_arena.sh` builds the runner from the
pinned engine commit instead (`docs/build.md`).

## The contest mode

The ladder plays the engine's `contest` mode. Since engine v0.10.0 it is the `harvest` kind,
"bananas as the economy": two teams of five monkeys on a grid at one-tick turns, one module
instance per monkey. Bananas come from the arena's trees and from nowhere else; a monkey carries
up to three and banks one every 2 turns while it stands in its team's home zone, a point each.
An undefended home loses one back every 6 turns to each enemy standing in it. The first team to
40 wins, else the leader after 3600 turns (a tied lead is a draw). Eliminations score nothing,
but a dead monkey drops its cargo, respawns only after 240 turns and comes back empty-handed. The
same bananas are the weapon: thrown, they fly out and come back; a dash with one in hand is an
attack; and one monkey per team, the catcher, takes an enemy's banana out of the air with a
perfectly timed dash. Vision is `own` fog (a cone along the facing), and a call shouts one of
256 symbols to every monkey within 4 cells. A game is played on one of four arenas (`grove`,
`orchards`, `gorge`, `burrows`), drawn at random with the sides.

**The guide is <https://bots.kfm.minip8.com/docs/contest>**: the rules in full, the four arenas
drawn, what a monkey sees, and where each of these is in the observation (the homes in `zones`,
the trees in `arena.spawners`, the two beats in `match.modeData`). Its source is
`service/templates/contest.html`. The row may change between engine releases: the engine's
content (`content/modes.toml`, `content/arenas/`) is the truth, the engine's `docs/bot-abi.md`
§3.3 is the kind as a bot sees it, and the observation carries the numbers a bot needs.

The example bots here were written before these rules (for king of the hill, the contest of
engine v0.7.0 to v0.9.0): they move, see and fight on the grid under fog, and make no effort to
gather or bank. The smallest bot that plays the economy is the engine's
[`tools/bots/examples/gatherer.c`](https://github.com/minip8/kung-fu-monkeys/blob/v0.10.0/tools/bots/examples/gatherer.c).

## The sandbox

The host enforces (engine `docs/bot-abi.md` §1.4; a contest publishes the values it runs with):

- **Imports:** nothing but, optionally, `env.kfm_log(ptr, len)`. Any other import refuses the
  module: a `memcpy` from a struct copy, `operator new`, an exception, the C++ runtime's
  `__cxa_*`. `KFM_BOT_MAIN` defines `memcpy`, `memmove` and `memset`; avoid the rest.
- **Budget:** 100 000 wasm instructions per `kfm_act`, 10 000 000 for instantiation and for
  `kfm_init`, counted in instructions, never time. A trap or a tick over budget costs that tick
  (the host repeats the bot's last input) and is counted in the result; a fault in `kfm_init`
  leaves the bot dead for the match.
- **Size:** a module of at most 4 MiB and a linear memory of at most 16 MiB, its own memory
  exported, and `__stack_pointer` exported (the recipe does it) so the host can restore the
  stack after a fault.
- **Determinism:** no clock and no randomness beyond the seed. Wasm floating point is
  deterministic and allowed, but the SDK uses none.

`tools/build.sh` checks the module's exports, imports and sizes the way the host does, so a
module that builds is one the host loads.

## The ladder

The service (`service/`, `docs/ladder-api.md`) has accounts (GitHub or Google sign-in) and
teams, source submissions built in the pinned build image and validated, seasons, Swiss-style
rounds of 5v5 `contest` games on an arena and sides drawn at random, Glicko-2 ratings, replays and a public
board. `kfm` (`cli/`) is its command line tool: log in, build, test, submit, follow matches and
fetch replays; a game's viewer link plays its replay in the browser client of the game's
engine release, from the first release after v0.5.1 (the engine's replay viewer, architecture
v0.68). `kfm test` (or `kfm_arena` itself) against the reference bots (`ref:easy`,
`ref:medium`, `ref:hard`) is the way to measure a bot before it is ranked.
