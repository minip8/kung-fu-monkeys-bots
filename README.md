# Kung Fu Monkeys bots

Bots for [Kung Fu Monkeys](https://github.com/minip8/kung-fu-monkeys), an arena brawler in which
monkeys throw bananas that fly like boomerangs. This repository holds the bot SDK, example bots
and the build recipe the ladder uses; the ladder itself comes later. The game repository is the
engine: it hosts bots, runs matches (`kfm_arena`) and owns the normative bot ABI.

## What a bot is

A bot is one core WebAssembly module (no WASI, no threads) behind four exports:
`kfm_abi_version` (returns 1), `kfm_obs_buffer` (where the host writes the observation),
`kfm_init(seed, slot)` and `kfm_act(tick)`, which returns the monkey's input for that tick packed
into 40 bits (stick direction and magnitude, aim, buttons, sub-tick). Before every `kfm_act` the
host writes an `Observation` v1 into the module's buffer: a fixed 5748-byte layout with the own
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
| `sdk/c/kfm_bot.h` | The C SDK: one freestanding header with the `Observation` v1 structs (layout checked by `static_assert`), the input packing, Q16.16 math, the engine's sine table, atan2 and Rng, and the banana flight step, all bit for bit with the engine. A byte-identical copy of the engine's `tools/bots/sdk-c/kfm_bot.h` (`sdk/c/ENGINE_SOURCE.txt`); never edit it here. |
| `sdk/cpp/kfm_bot.hpp` | The C++ layer over it: `Fixed` and `Vec2` with operators, views over the observation that iterate only what is there, an `Input` builder, the `KFM_BOT` macro that defines the exports, and helpers (nearest enemy and banana, flight prediction and time to impact, "will this hit me", the dash-catch direction, a clear throwing line, the grid's cells and walking). Every helper is documented in the header. |
| `examples/` | Example bots, one directory each: `idle`, `chaser` (C) and `starter` (C++, the one to copy). `examples/README.md` says what each does and how to build and run them. |
| `tools/build.sh` | The build recipe: `tools/build.sh <bot dir> <out.wasm>`. |
| `tools/fetch_kfm_arena.sh` | Fetches the engine's released headless `kfm_arena` for the pinned engine into `.kfm/`. |
| `docs/build.md` | The recipe in full, the build sandbox, the engine pin and CI. |
| `service/` | The ladder service, `kfm-ladder` (Rust): accounts, tokens, teams, submissions built in the sandbox, seasons, scheduled matches, Glicko-2 ratings, replays and the ladder. `service/README.md` says how to run and test it. |
| `docs/ladder-api.md` | The ladder's HTTP API, normative: every route, its auth, its JSON and its errors. |

## Quick start

You need a clang that targets wasm32 with `wasm-ld`. The ladder's is emsdk's
(`emsdk install 6.0.13 && emsdk activate 6.0.13`; `docs/build.md` says why that version).

```sh
git clone https://github.com/minip8/kung-fu-monkeys-bots.git
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
shows what the bot passed to `kfm_log`. Iterate: change the bot, rebuild, play again, with
another `--seed` now and then. While no engine release carries `kfm_arena` yet,
`tools/build_kfm_arena.sh` builds it from the pinned engine commit instead (`docs/build.md`).

## The contest mode

The ladder plays the engine's `contest` mode (`content/modes.toml`; architecture §8 "The ladder"
and "Teams, fog and calls", §5.12 for the grid: read them). Today it is the squad shape of
`grid_rounds_1t` on the `courtyard` arena (24 x 14 cells of one unit): two teams of up to five
monkeys, one module instance each, rounds to three. Monkeys move on the grid, to one of the
eight neighbouring cells per turn at one-tick turns, a dash covering two cells; bananas fly
freely as everywhere else, at about a cell a tick, and bounce off the walls. A monkey has 2 HP;
one per team, the catcher, has a dash whose perfect window catches a banana, and nobody else's
does. Vision is `own` fog: a monkey sees a cone 4 cells deep along its facing (5 when it stood
still) behind a clear line of sight, the cells next to it, bananas in flight within 3 cells and
around its own banana; what it does not see is zeroed in its observation. A call ability
shouts the aim byte (one of 256 symbols) to every monkey within 4 cells, ally or enemy, so a
team shares what it sees through the game. The row may change between engine releases; the
engine's content is the truth, and the observation carries the numbers a bot needs (`rules`,
the ability defs, `turnTicks`).

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
rounds of 5v5 `contest` games played from both sides, Glicko-2 ratings, replays and a public
board; it is not deployed yet. Coming: a `kfm` command line tool to log in, build, test, submit
and fetch replays, and the replay viewer in the browser client. Until then, `kfm_arena` and the
reference bots (`ref:easy`, `ref:medium`, `ref:hard`) are the way to measure a bot.
