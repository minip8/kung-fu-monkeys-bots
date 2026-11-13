/* kfm_bot.h: the Kung Fu Monkeys bot SDK for C, bot ABI and Observation v2.
 *
 * One dependency-free C11 header (freestanding: <stdint.h> and <stddef.h>
 * only, no libc). Copy it into a bot project. The normative ABI is
 * docs/bot-abi.md of the game repository; this header mirrors
 * engine/ai/include/kfm/ai/observation.hpp field for field and checks the
 * layout with _Static_assert. Its home is the bots repository; the game
 * repository keeps the seed copy (tools/bots/sdk-c) that its tests build.
 *
 * Use:
 *   #define KFM_BOT_MAIN   // in exactly one .c file, before the include:
 *   #include "kfm_bot.h"   // defines kfm_abi_version, kfm_obs_buffer, the
 *                          // observation buffer `kfm_obs` and memcpy/memset
 *   void kfm_init(uint64_t seed, uint32_t slot) { ... }
 *   uint64_t kfm_act(uint32_t tick) { ...read kfm_obs...; return kfm_pack_input(...); }
 *
 * Build (wasm32, no libc; see tools/bots/build_example.sh):
 *   clang --target=wasm32 -O2 -nostdlib -ffreestanding -Wl,--no-entry
 *     -Wl,--export=kfm_abi_version -Wl,--export=kfm_obs_buffer -Wl,--export=kfm_init
 *     -Wl,--export=kfm_act -Wl,--export=__stack_pointer -Wl,--allow-undefined bot.c -o bot.wasm
 * A module imports nothing but env.kfm_log; the host refuses any other import
 * (an unresolved memcpy is one: KFM_BOT_MAIN provides it).
 *
 * Determinism: every helper is integer arithmetic (Q16.16 fixed point with
 * int64 intermediates, the engine's sine table, integer atan2 and sqrt), so a
 * bot behaves identically on every host. A bot's only randomness is
 * kfm_rng seeded from kfm_init's seed.
 *
 * Exactness of the helpers against the sim (engine/sim, architecture §5.7):
 *   exact     kfm_sin/kfm_cos (the engine's 4096-entry table, stored as its
 *             quarter wave), kfm_angle_of (the sim's atan2), kfm_vec_from_bam,
 *             kfm_fx_* (Fixed), kfm_rng (kfm::Rng), and one kfm_flight_step
 *             of a banana given its owner's position after that tick's move
 *             (outbound, return steering, grounding, walls, the outer box);
 *   approx    kfm_predict_banana over several ticks (the owner's future
 *             position is extrapolated from its velocity; catches, hits,
 *             parries and power-ups are not foreseen), kfm_dash_catch_dir
 *             (the own monkey's dash ignores walls).
 */
#ifndef KFM_BOT_H
#define KFM_BOT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#define KFM_STATIC_ASSERT(cond, msg) static_assert(cond, msg)
#else
#define KFM_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#endif

#if defined(__wasm__)
#define KFM_EXPORT(name) __attribute__((export_name(name)))
#define KFM_IMPORT(module, name) __attribute__((import_module(module), import_name(name)))
#else
#define KFM_EXPORT(name)
#define KFM_IMPORT(module, name)
#endif

/* ---- ABI constants -------------------------------------------------------- */

/* Version 2 (architecture v0.80) is version 1 with the `zones` block appended:
   nothing of version 1 moved. A host that speaks 2 still runs a module built
   against version 1 (it writes it version 1's 5748 bytes under a version 1
   header and nothing past them); a module built with this header is version
   2 and needs a host that speaks 2. */
#define KFM_ABI_VERSION 2u /* kfm_abi_version() and header.version */
#define KFM_OBS_SIZE 5880u /* sizeof(kfm_observation) and header.size */

/* Capacities (a change is a new ABI version). */
#define KFM_MAX_PLAYERS 10
#define KFM_MAX_BANANAS 64
#define KFM_MAX_PICKUPS 32
#define KFM_ABILITY_SLOTS 3
#define KFM_MAX_MODIFIERS 8
#define KFM_MAX_SPAWN_POINTS 10
#define KFM_MAX_WALLS 32
#define KFM_MAX_HAZARDS 16
#define KFM_MAX_SPAWNERS 8
#define KFM_MAX_HEARD_CALLS 16
#define KFM_MODE_DATA_WORDS 16
/* v0.69: match.modeData[KFM_ZONE_WORD + 4k .. +3] is the arena's zone k
   (k < KFM_ZONES) as the closed box minX, minY, maxX, maxY (Q16.16, as
   int32_t), all zero when the arena has no zone k. Zone 0 is the hill.
   Version 2 still fills them. */
#define KFM_ZONE_WORD 8
#define KFM_ZONES 2
/* v0.80 (version 2): every zone of the arena, zones 0 and 1 included, is in
   observation.zones. */
#define KFM_MAX_ZONES 8
/* v0.80, the control mode's state in match.modeData (bot-abi.md §3):
   words 0-1 the teams' clocks (u16 each, team 2w low, 2w+1 high), word
   KFM_CONTROL_OWNER_WORD the owners and KFM_CONTROL_CHALLENGER_WORD the
   challengers (4 bits per zone, zone k in bits 4k..4k+3, the team + 1, 0 =
   nobody), words KFM_CONTROL_PROGRESS_WORD.. the capture progress (u16 per
   zone, zone k in word + k/2, the low half for an even k). The kind plays
   at most 4 teams and KFM_MAX_ZONES zones. */
#define KFM_CONTROL_OWNER_WORD 2
#define KFM_CONTROL_CHALLENGER_WORD 3
#define KFM_CONTROL_PROGRESS_WORD 4
/* v0.82, the harvest mode's state in match.modeData (bot-abi.md §3): word
   KFM_HARVEST_BEATS_WORD holds the two beats, bank_ticks in the low half and
   steal_ticks (0 = no stealing) in the high half; every other word 0-7 is 0.
   Team t's home is zone t (observation.zones, or modeData words 8-15 for
   zones 0 and 1); the cargo is self.ammo, up to self.ammoMax. */
#define KFM_HARVEST_BEATS_WORD 0
#define KFM_HARVEST_BANK_TICKS(word) ((uint32_t)(word) & 0xFFFFu)
#define KFM_HARVEST_STEAL_TICKS(word) ((uint32_t)(word) >> 16)

#define KFM_NO_SLOT 0xFFu        /* a slot field naming no player */
#define KFM_NO_APPROACH 0xFFFFu  /* banana.approachTicks while self is not alive */
#define KFM_APPROACH_HORIZON 600 /* ticks searched for the closest approach */
#define KFM_TICK_HZ 60

/* PlayerInput.buttons bits. A button fires on a press edge only (up on the
 * previous applied tick, down on this one). */
#define KFM_BTN_THROW 0x01u
#define KFM_BTN_DASH 0x02u
#define KFM_BTN_ABILITY1 0x04u /* the third ability slot */
#define KFM_BTN_ABILITY2 0x08u
#define KFM_BTN_TAUNT 0x10u

/* Byte-field values. A value not listed here (a later kind) means "something else". */
enum { KFM_REL_NONE = 0, KFM_REL_SELF = 1, KFM_REL_ALLY = 2, KFM_REL_ENEMY = 3 };
enum {
  KFM_PHASE_LOBBY = 0,
  KFM_PHASE_ROUND_STARTING = 1,
  KFM_PHASE_PLAYING = 2,
  KFM_PHASE_ROUND_ENDED = 3,
  KFM_PHASE_MATCH_ENDED = 4
};
enum {
  KFM_MODE_ROUNDS = 0,
  KFM_MODE_DEATHMATCH = 1,
  KFM_MODE_SURVIVAL = 2,
  KFM_MODE_TEAM_ROUNDS = 3,
  KFM_MODE_TEAM_DEATHMATCH = 4,
  KFM_MODE_HILL = 5,
  KFM_MODE_CAPTURE = 6,
  KFM_MODE_CONTROL = 7, /* architecture v0.80 */
  KFM_MODE_HARVEST = 8  /* architecture v0.82: bank bananas in the home zone */
};
enum {
  KFM_ABILITY_NONE = 0,
  KFM_ABILITY_DASH = 1,
  KFM_ABILITY_THROW = 2,
  KFM_ABILITY_BLINK = 3,
  KFM_ABILITY_SLAM = 4,
  KFM_ABILITY_PARRY = 5,
  KFM_ABILITY_CALL = 6 /* architecture v0.35 */
};
enum {
  KFM_ABILITY_IDLE = 0,
  KFM_ABILITY_STARTUP = 1,
  KFM_ABILITY_ACTIVE = 2,
  KFM_ABILITY_RECOVERY = 3
};
/* v0.57: SINKING, a banana that came down over a pit dropping into the hole:
 * harmless, never a pickup. Every phase from GROUNDED on is out of flight. */
enum {
  KFM_BANANA_OUTBOUND = 0,
  KFM_BANANA_RETURNING = 1,
  KFM_BANANA_GROUNDED = 2,
  KFM_BANANA_SINKING = 3
};
enum { KFM_PICKUP_BANANA = 0, KFM_PICKUP_POWERUP = 1, KFM_PICKUP_FLAG = 2 };
enum { KFM_HAZARD_PIT = 0, KFM_HAZARD_SLOW = 1 };
/* Tag bit numbers (the `tags` fields): test with kfm_has_tag(tags, KFM_TAG_...). */
enum {
  KFM_TAG_DASHING = 0,
  KFM_TAG_PERFECT_DASH = 1,
  KFM_TAG_HIT_STUN = 2,
  KFM_TAG_SHIELDED = 3,
  KFM_TAG_INVULNERABLE = 4,
  KFM_TAG_ELIMINATED = 5,
  KFM_TAG_SLAMMING = 6,
  KFM_TAG_PARRYING = 7,
  KFM_TAG_FRIENDLY_FIRE = 8, /* allies' bananas and dashes hit this monkey (team modes) */
  KFM_TAG_CARRYING = 9,      /* holds a capture flag */
  KFM_TAG_FALLING = 10       /* going down a pit: out of control, out of reach, soon out */
};
/* header.schemaFlags bit 0 (architecture v0.35): the mode's vision is not
 * "all", so players[i].visible and bananas[i].visible are meaningful; an
 * unseen monkey keeps only present, relation, team and respawnTicks, an
 * unseen banana or pickup is all zero. */
#define KFM_SCHEMA_FLAG_FOG 0x1u
/* heardCalls[i].cellX/cellY of a caller this bot does not see. */
#define KFM_NO_CELL (-1)

/* ---- Observation v2 --------------------------------------------------------
 * Little-endian, 5880 bytes, alignment 4, every hole an explicit pad. Units:
 * Q16.16 (int32 / 65536: arena units, units per tick for speeds), angle8
 * (uint8 1/256 turn, 0 = +x, counter-clockwise), bam (uint16 1/65536 turn),
 * ticks (60 per second), slot (0..9, 0xFF none). Field meanings: bot-abi.md §2. */

typedef struct kfm_obs_header {
  uint32_t version; /* 2 */
  uint32_t size;    /* 5880 */
  uint32_t schemaFlags, reserved0;
} kfm_obs_header;

typedef struct kfm_obs_match {
  uint32_t tick;           /* the tick this observation is for */
  uint32_t ticksInPhase;   /* the mode clock while Playing */
  uint32_t timeLimitTicks; /* 0 = none */
  uint32_t contentHash;
  uint8_t modeIndex;
  uint8_t modeKind; /* KFM_MODE_* */
  uint8_t arenaIndex;
  uint8_t phase; /* KFM_PHASE_* */
  uint8_t round, roundsToWin, scoreToWin, aliveCount;
  uint16_t presentMask;    /* bit i: slot i in the roster */
  uint16_t aliveMask;      /* bit i: slot i in the arena */
  uint8_t inputDelayTicks; /* the answer applies on tick + this */
  uint8_t lastRoundWinner;
  uint16_t bananaRegrowTicks; /* v0.57: an empty hand grows a banana after this, 0 = never */
  uint16_t roundStartTicks, roundEndTicks, respawnDelayTicks, respawnInvulnTicks;
  uint16_t turnTicks;    /* a grid mode's turn length, 0 in a continuous one (v0.35) */
  uint16_t ticksToLatch; /* grid: ticks to the next latch, 0 = this tick's input is latched */
  int8_t scores[KFM_MAX_PLAYERS]; /* round wins or points; signed since v0.57 */
  uint8_t deaths[KFM_MAX_PLAYERS];
  uint16_t respawnTicks[KFM_MAX_PLAYERS];
  int16_t teamScores[KFM_MAX_PLAYERS]; /* by team index; signed since v0.57 */
  uint32_t modeData[KFM_MODE_DATA_WORDS]; /* the mode's state (bot-abi.md §3) */
} kfm_obs_match;

typedef struct kfm_obs_rules {
  int32_t playerRadius;      /* Q16.16 */
  int32_t playerMoveSpeed;   /* Q16.16 per tick */
  int32_t bananaRadius;      /* Q16.16 */
  int32_t bananaSpeed;       /* Q16.16 per tick */
  int32_t bananaSpawnOffset; /* Q16.16 */
  int32_t bananaRestitution; /* Q16.16 */
  int32_t pickupRadius;      /* Q16.16 */
  uint16_t bananaOutboundTicks, bananaMaxReturnTicks, bananaTurnRateBam, hitStunTicks;
  uint8_t bananaDamage, dashAttackDamage, dashAttackMinAmmo, dashAttackConsumesAmmo, playerMaxHp;
  uint8_t playerAmmoMax, playerAmmoStart, pad0;
} kfm_obs_rules;

typedef struct kfm_obs_ability {
  uint8_t defIndex;
  uint8_t kind;   /* KFM_ABILITY_DASH.. */
  uint8_t phase;  /* KFM_ABILITY_IDLE.. */
  uint8_t button; /* the KFM_BTN_* bit that fires this slot */
  uint16_t phaseTicks;
  uint16_t cooldownTicks; /* ready when 0 and Idle */
  uint32_t activatedTick;
  uint8_t activatedSubTick, pad0, pad1, pad2;
  uint16_t defCooldownTicks, defStartupTicks, defActiveTicks, defRecoveryTicks;
  uint16_t defPerfectWindowTicks; /* Dash: the catch window from the press */
  uint16_t pad3;
  int32_t defSpeed;  /* Q16.16 per tick */
  int32_t defParam0; /* Q16.16: Slam reach */
  int32_t defParam1; /* Q16.16: Slam push */
  int32_t dirX;      /* Q16.16 unit vector of the last activation */
  int32_t dirY;
} kfm_obs_ability;

typedef struct kfm_obs_modifier {
  uint8_t powerUp, pad0;
  uint16_t ticksLeft;
} kfm_obs_modifier;

typedef struct kfm_obs_self {
  uint8_t slot, alive, hp, maxHp, ammo;
  uint8_t ammoMax; /* a perfect catch needs ammo < ammoMax */
  uint8_t lastButtons, modifierCount;
  int32_t posX; /* Q16.16 */
  int32_t posY, prevPosX, prevPosY;
  int32_t velX; /* Q16.16 per tick */
  int32_t velY, moveSpeed, radius, knockbackX, knockbackY;
  uint32_t tags;
  uint16_t hitStunTicks, knockbackTicks, invulnTicks, respawnTicks;
  uint8_t facing;       /* angle8 */
  uint8_t facingOctant; /* grid: facing as 0..7 (0 east, counter-clockwise), the cone's */
  uint8_t role;         /* the loadout index the slot plays */
  uint8_t team;
  int16_t cellX, cellY; /* grid: the own cell (column from the west, row from the north) */
  kfm_obs_ability abilities[KFM_ABILITY_SLOTS]; /* 0 Dash button, 1 Throw, 2 Ability1 */
  kfm_obs_modifier modifiers[KFM_MAX_MODIFIERS];
} kfm_obs_self;

typedef struct kfm_obs_player {
  uint8_t present, alive;
  uint8_t visible;  /* fog (KFM_SCHEMA_FLAG_FOG): 1 when seen this turn */
  uint8_t relation; /* KFM_REL_* */
  uint8_t team, hp, maxHp, ammo, ammoMax, facing, dashing, pad0;
  uint32_t tags;
  int32_t posX, posY, velX, velY;
  uint16_t hitStunTicks, invulnTicks, respawnTicks, pad1;
  uint8_t abilityKind[KFM_ABILITY_SLOTS], pad2, abilityPhase[KFM_ABILITY_SLOTS], pad3;
  uint16_t abilityCooldownTicks[KFM_ABILITY_SLOTS], pad4, abilityPhaseTicks[KFM_ABILITY_SLOTS];
  uint16_t pad5;
} kfm_obs_player;

typedef struct kfm_obs_banana {
  uint8_t valid;
  uint8_t visible; /* fog: 1 when seen or heard this turn */
  uint8_t phase;   /* KFM_BANANA_* */
  uint8_t owner, ownerRelation, damage, pad0, pad1;
  uint16_t headingBam, ticksAlive, ticksInPhase;
  uint16_t approachTicks; /* closest approach to self, on the current velocity */
  int32_t posX, posY, velX, velY, speed, radius, approachDistance;
} kfm_obs_banana;

typedef struct kfm_obs_pickup {
  uint8_t valid;
  uint8_t kind; /* KFM_PICKUP_* */
  uint8_t powerUp, spawner;
  int32_t posX, posY, radius;
} kfm_obs_pickup;

typedef struct kfm_obs_point {
  int32_t x, y;
} kfm_obs_point;

typedef struct kfm_obs_box {
  int32_t minX, minY, maxX, maxY;
} kfm_obs_box;

typedef struct kfm_obs_hazard {
  kfm_obs_box box;
  int32_t speedMul;
  uint8_t kind; /* KFM_HAZARD_* */
  uint8_t pad0, pad1, pad2;
} kfm_obs_hazard;

/* A power-up spawner, or (v0.82, tree 1) a tree: on the same beat it lays one
   banana pickup on its cell while fewer than maxAlive bananas lie there. */
typedef struct kfm_obs_spawner {
  kfm_obs_point pos;
  uint16_t powerUpSet, firstTicks, intervalTicks;
  uint8_t maxAlive;
  uint8_t tree; /* v0.82 (was pad0, always 0): 1 a tree, 0 a power-up spawner */
} kfm_obs_spawner;

typedef struct kfm_obs_arena {
  kfm_obs_point halfExtents; /* the outer box is [-half, +half] */
  uint8_t spawnPointCount, wallCount, hazardCount, spawnerCount;
  kfm_obs_point spawnPoints[KFM_MAX_SPAWN_POINTS];
  kfm_obs_box walls[KFM_MAX_WALLS];
  kfm_obs_hazard hazards[KFM_MAX_HAZARDS];
  kfm_obs_spawner spawners[KFM_MAX_SPAWNERS];
} kfm_obs_arena;

/* A call heard this turn (architecture v0.35): the caller's slot, its aim
 * byte as the symbol, the tick of the call and the caller's cell then, or
 * KFM_NO_CELL when this bot does not see the caller. */
typedef struct kfm_obs_heard_call {
  uint8_t valid, slot, symbol, pad0;
  uint32_t tick;
  int16_t cellX, cellY;
} kfm_obs_heard_call;

/* v0.80 (version 2): an objective zone, the closed box in Q16.16. */
typedef struct kfm_obs_zone {
  int32_t minX, minY;
  int32_t maxX, maxY;
} kfm_obs_zone;

/* v0.80 (version 2): the arena's zones in content order (zone 0 is the hill,
   a capture team t's home is zone t, control plays them all); entries past
   `count` are zero. Fog never hides them. */
typedef struct kfm_obs_zones {
  uint8_t count;
  uint8_t pad0[3];
  kfm_obs_zone zone[KFM_MAX_ZONES];
} kfm_obs_zones;

typedef struct kfm_observation {
  kfm_obs_header header;
  kfm_obs_match match;
  kfm_obs_rules rules;
  kfm_obs_self self;
  kfm_obs_player players[KFM_MAX_PLAYERS];
  kfm_obs_banana bananas[KFM_MAX_BANANAS];
  kfm_obs_pickup pickups[KFM_MAX_PICKUPS];
  kfm_obs_arena arena;
  kfm_obs_heard_call heardCalls[KFM_MAX_HEARD_CALLS]; /* oldest first */
  kfm_obs_zones zones; /* version 2: every zone of the arena */
} kfm_observation;

#define KFM_CHECK_SIZE(T, n) KFM_STATIC_ASSERT(sizeof(T) == (n), "size of " #T)
#define KFM_CHECK_AT(T, f, n) KFM_STATIC_ASSERT(offsetof(T, f) == (n), "offset of " #T "." #f)
KFM_CHECK_SIZE(kfm_obs_header, 16);
KFM_CHECK_SIZE(kfm_obs_match, 168);
KFM_CHECK_SIZE(kfm_obs_rules, 44);
KFM_CHECK_SIZE(kfm_obs_ability, 48);
KFM_CHECK_SIZE(kfm_obs_modifier, 4);
KFM_CHECK_SIZE(kfm_obs_self, 244);
KFM_CHECK_SIZE(kfm_obs_player, 64);
KFM_CHECK_SIZE(kfm_obs_banana, 44);
KFM_CHECK_SIZE(kfm_obs_pickup, 16);
KFM_CHECK_SIZE(kfm_obs_hazard, 24);
KFM_CHECK_SIZE(kfm_obs_spawner, 16);
KFM_CHECK_SIZE(kfm_obs_arena, 1116);
KFM_CHECK_SIZE(kfm_obs_heard_call, 12);
KFM_CHECK_SIZE(kfm_obs_zone, 16);
KFM_CHECK_SIZE(kfm_obs_zones, 132);
KFM_CHECK_SIZE(kfm_observation, KFM_OBS_SIZE);
KFM_CHECK_AT(kfm_observation, match, 16);
KFM_CHECK_AT(kfm_observation, rules, 184);
KFM_CHECK_AT(kfm_observation, self, 228);
KFM_CHECK_AT(kfm_observation, players, 472);
KFM_CHECK_AT(kfm_observation, bananas, 1112);
KFM_CHECK_AT(kfm_observation, pickups, 3928);
KFM_CHECK_AT(kfm_observation, arena, 4440);
KFM_CHECK_AT(kfm_observation, heardCalls, 5556);
KFM_CHECK_AT(kfm_observation, zones, 5748);
KFM_CHECK_AT(kfm_obs_match, modeIndex, 16);
KFM_CHECK_AT(kfm_obs_match, presentMask, 24);
KFM_CHECK_AT(kfm_obs_match, inputDelayTicks, 28);
KFM_CHECK_AT(kfm_obs_match, bananaRegrowTicks, 30);
KFM_CHECK_AT(kfm_obs_match, roundStartTicks, 32);
KFM_CHECK_AT(kfm_obs_match, scores, 44);
KFM_CHECK_AT(kfm_obs_match, deaths, 54);
KFM_CHECK_AT(kfm_obs_match, respawnTicks, 64);
KFM_CHECK_AT(kfm_obs_match, teamScores, 84);
KFM_CHECK_AT(kfm_obs_match, modeData, 104);
KFM_CHECK_AT(kfm_obs_rules, bananaOutboundTicks, 28);
KFM_CHECK_AT(kfm_obs_rules, bananaDamage, 36);
KFM_CHECK_AT(kfm_obs_rules, playerMaxHp, 40);
KFM_CHECK_AT(kfm_obs_ability, phaseTicks, 4);
KFM_CHECK_AT(kfm_obs_ability, activatedTick, 8);
KFM_CHECK_AT(kfm_obs_ability, defCooldownTicks, 16);
KFM_CHECK_AT(kfm_obs_ability, defPerfectWindowTicks, 24);
KFM_CHECK_AT(kfm_obs_ability, defSpeed, 28);
KFM_CHECK_AT(kfm_obs_ability, dirX, 40);
KFM_CHECK_AT(kfm_obs_self, posX, 8);
KFM_CHECK_AT(kfm_obs_self, velX, 24);
KFM_CHECK_AT(kfm_obs_self, moveSpeed, 32);
KFM_CHECK_AT(kfm_obs_self, knockbackX, 40);
KFM_CHECK_AT(kfm_obs_self, tags, 48);
KFM_CHECK_AT(kfm_obs_self, hitStunTicks, 52);
KFM_CHECK_AT(kfm_obs_self, invulnTicks, 56);
KFM_CHECK_AT(kfm_obs_self, facing, 60);
KFM_CHECK_AT(kfm_obs_self, cellX, 64);
KFM_CHECK_AT(kfm_obs_self, abilities, 68);
KFM_CHECK_AT(kfm_obs_self, modifiers, 212);
KFM_CHECK_AT(kfm_obs_player, team, 4);
KFM_CHECK_AT(kfm_obs_player, tags, 12);
KFM_CHECK_AT(kfm_obs_player, posX, 16);
KFM_CHECK_AT(kfm_obs_player, hitStunTicks, 32);
KFM_CHECK_AT(kfm_obs_player, abilityKind, 40);
KFM_CHECK_AT(kfm_obs_player, abilityPhase, 44);
KFM_CHECK_AT(kfm_obs_player, abilityCooldownTicks, 48);
KFM_CHECK_AT(kfm_obs_player, abilityPhaseTicks, 56);
KFM_CHECK_AT(kfm_obs_banana, headingBam, 8);
KFM_CHECK_AT(kfm_obs_banana, approachTicks, 14);
KFM_CHECK_AT(kfm_obs_banana, posX, 16);
KFM_CHECK_AT(kfm_obs_banana, speed, 32);
KFM_CHECK_AT(kfm_obs_banana, approachDistance, 40);
KFM_CHECK_AT(kfm_obs_pickup, posX, 4);
KFM_CHECK_AT(kfm_obs_hazard, speedMul, 16);
KFM_CHECK_AT(kfm_obs_hazard, kind, 20);
KFM_CHECK_AT(kfm_obs_spawner, powerUpSet, 8);
KFM_CHECK_AT(kfm_obs_spawner, maxAlive, 14);
KFM_CHECK_AT(kfm_obs_spawner, tree, 15);
KFM_CHECK_AT(kfm_obs_arena, spawnPointCount, 8);
KFM_CHECK_AT(kfm_obs_arena, spawnPoints, 12);
KFM_CHECK_AT(kfm_obs_arena, walls, 92);
KFM_CHECK_AT(kfm_obs_arena, hazards, 604);
KFM_CHECK_AT(kfm_obs_arena, spawners, 988);
KFM_CHECK_AT(kfm_obs_heard_call, tick, 4);
KFM_CHECK_AT(kfm_obs_heard_call, cellX, 8);
KFM_CHECK_AT(kfm_obs_zones, zone, 4);
KFM_CHECK_AT(kfm_obs_zone, maxY, 12);
#undef KFM_CHECK_SIZE
#undef KFM_CHECK_AT

/* ---- the module's exports and import ---------------------------------------
 * The bot defines kfm_init and kfm_act; KFM_BOT_MAIN defines the other two.
 * The host calls kfm_init once per instance (seed: from the match seed; slot:
 * 0..9), then per tick writes the observation into kfm_obs and calls kfm_act
 * with match.tick. A trap or more than the fuel budget (100 000 wasm
 * instructions by default) costs the tick: the host repeats the last input. */
KFM_EXPORT("kfm_abi_version") uint32_t kfm_abi_version(void);
KFM_EXPORT("kfm_obs_buffer") kfm_observation* kfm_obs_buffer(void);
KFM_EXPORT("kfm_init") void kfm_init(uint64_t seed, uint32_t slot);
KFM_EXPORT("kfm_act") uint64_t kfm_act(uint32_t tick);

/* Optional: len bytes of UTF-8 text to the host's log (never affects the match). */
KFM_IMPORT("env", "kfm_log") void kfm_log(const char* text, uint32_t len);

/* The observation buffer the host writes before every kfm_act. */
extern kfm_observation kfm_obs;

#if defined(KFM_BOT_MAIN)
kfm_observation kfm_obs;
uint32_t kfm_abi_version(void) { return KFM_ABI_VERSION; }
kfm_observation* kfm_obs_buffer(void) { return &kfm_obs; }
#if defined(__wasm__)
/* No libc: the compiler lowers struct copies and zeroing to these calls. */
void* memcpy(void* dst, const void* src, size_t n) {
  unsigned char* d = (unsigned char*)dst;
  const unsigned char* s = (const unsigned char*)src;
  while (n-- > 0)
    *d++ = *s++;
  return dst;
}
void* memmove(void* dst, const void* src, size_t n) {
  unsigned char* d = (unsigned char*)dst;
  const unsigned char* s = (const unsigned char*)src;
  if (d < s) {
    while (n-- > 0)
      *d++ = *s++;
  } else {
    while (n-- > 0)
      d[n] = s[n];
  }
  return dst;
}
void* memset(void* dst, int c, size_t n) {
  unsigned char* d = (unsigned char*)dst;
  while (n-- > 0)
    *d++ = (unsigned char)c;
  return dst;
}
#endif
#endif

/* ---- the action ----------------------------------------------------------- */

/* kfm_act's return value: the five PlayerInput bytes in the low 40 bits.
 * moveDir/aimDir are angle8; moveMag 0 = idle stick, 255 = full; buttons
 * KFM_BTN_* bits; subTick the 1/256 tick of the latest press (0 = tick start). */
static inline uint64_t kfm_pack_input(uint8_t moveDir, uint8_t moveMag, uint8_t aimDir,
                                      uint8_t buttons, uint8_t subTick) {
  return (uint64_t)moveDir | ((uint64_t)moveMag << 8) | ((uint64_t)aimDir << 16) |
         ((uint64_t)(buttons & 0x1Fu) << 24) | ((uint64_t)subTick << 32);
}

/* Whether the observation is one this header understands (version and size). */
static inline int kfm_obs_ok(const kfm_observation* o) {
  return o->header.version == KFM_ABI_VERSION && o->header.size == KFM_OBS_SIZE;
}

static inline int kfm_has_tag(uint32_t tags, int tag) { return (tags >> tag) & 1u; }

/* ---- fixed point (Q16.16, the sim's Fixed) -------------------------------- */

typedef int32_t kfm_fx;
#define KFM_FX_ONE 65536
#define KFM_FX_HALF 32768

typedef struct kfm_vec {
  kfm_fx x;
  kfm_fx y;
} kfm_vec;

static inline kfm_fx kfm_fx_from_int(int32_t i) { return (kfm_fx)((uint32_t)i << 16); }
/* num / den in Q16.16, rounded toward zero (Fixed::fromRatio). */
static inline kfm_fx kfm_fx_ratio(int32_t num, int32_t den) {
  return den == 0 ? 0 : (kfm_fx)(((int64_t)num * 65536) / den);
}
static inline int32_t kfm_fx_floor(kfm_fx a) { return a >> 16; }
static inline kfm_fx kfm_fx_mul(kfm_fx a, kfm_fx b) { return (kfm_fx)(((int64_t)a * b) >> 16); }
/* a / b rounded toward zero; division by zero saturates (Fixed's operator/). */
static inline kfm_fx kfm_fx_div(kfm_fx a, kfm_fx b) {
  if (b == 0) return a < 0 ? INT32_MIN : INT32_MAX;
  return (kfm_fx)(((int64_t)a * 65536) / b);
}
static inline kfm_fx kfm_fx_abs(kfm_fx a) { return a < 0 ? -a : a; }
static inline kfm_fx kfm_fx_min(kfm_fx a, kfm_fx b) { return a < b ? a : b; }
static inline kfm_fx kfm_fx_max(kfm_fx a, kfm_fx b) { return a > b ? a : b; }
static inline kfm_fx kfm_fx_clamp(kfm_fx v, kfm_fx lo, kfm_fx hi) {
  return kfm_fx_min(kfm_fx_max(v, lo), hi);
}

/* floor(sqrt(n)), exact for every uint64. */
static inline uint32_t kfm_isqrt64(uint64_t n) {
  uint64_t res = 0;
  uint64_t bit = (uint64_t)1 << 62;
  while (bit > n)
    bit >>= 2;
  while (bit != 0) {
    if (n >= res + bit) {
      n -= res + bit;
      res = (res >> 1) + bit;
    } else {
      res >>= 1;
    }
    bit >>= 2;
  }
  return (uint32_t)res;
}
/* floor(sqrt(a)) in Q16.16; 0 for a <= 0. */
static inline kfm_fx kfm_fx_sqrt(kfm_fx a) {
  return a <= 0 ? 0 : (kfm_fx)kfm_isqrt64((uint64_t)a << 16);
}

static inline kfm_vec kfm_v(kfm_fx x, kfm_fx y) {
  kfm_vec r;
  r.x = x;
  r.y = y;
  return r;
}
static inline kfm_vec kfm_vec_add(kfm_vec a, kfm_vec b) { return kfm_v(a.x + b.x, a.y + b.y); }
static inline kfm_vec kfm_vec_sub(kfm_vec a, kfm_vec b) { return kfm_v(a.x - b.x, a.y - b.y); }
static inline kfm_vec kfm_vec_scale(kfm_vec a, kfm_fx s) {
  return kfm_v(kfm_fx_mul(a.x, s), kfm_fx_mul(a.y, s));
}
static inline kfm_vec kfm_vec_mul_int(kfm_vec a, int32_t k) { return kfm_v(a.x * k, a.y * k); }
static inline kfm_fx kfm_vec_dot(kfm_vec a, kfm_vec b) {
  return kfm_fx_mul(a.x, b.x) + kfm_fx_mul(a.y, b.y);
}
/* |a|^2 in raw Q32.32 (exact for the whole range). */
static inline uint64_t kfm_vec_len_sq(kfm_vec a) {
  return (uint64_t)((int64_t)a.x * a.x) + (uint64_t)((int64_t)a.y * a.y);
}
/* floor(|a|) in Q16.16. */
static inline kfm_fx kfm_vec_len(kfm_vec a) {
  uint32_t r = kfm_isqrt64(kfm_vec_len_sq(a));
  return r > (uint32_t)INT32_MAX ? INT32_MAX : (kfm_fx)r;
}
static inline kfm_fx kfm_vec_dist(kfm_vec a, kfm_vec b) { return kfm_vec_len(kfm_vec_sub(b, a)); }
/* a / |a| (the sim's normalized); zero for the zero vector. */
static inline kfm_vec kfm_vec_normalized(kfm_vec a) {
  kfm_fx len = kfm_vec_len(a);
  if (len == 0) return kfm_v(0, 0);
  return kfm_v(kfm_fx_div(a.x, len), kfm_fx_div(a.y, len));
}
/* r*r in raw Q32.32, the unit of kfm_vec_len_sq (compare squared distances). */
static inline uint64_t kfm_fx_sq(kfm_fx r) { return (uint64_t)((int64_t)r * r); }

/* ---- angles (bam: 1/65536 turn; angle8: 1/256 turn) ---------------------- */

/* sin over the first quarter turn: entries 0..1024 of the engine's 4096-entry
 * table (engine/core sin_table.inc, sin(2*pi*i/4096) in Q16.16, rounded),
 * which is exactly symmetric, so the full table follows from these. */
// clang-format off
static const int32_t kfm_sin_quarter[1025] = {
    0,101,201,302,402,503,603,704,804,905,1005,1106,1206,1307,1407,1508,
    1608,1709,1809,1910,2010,2111,2211,2312,2412,2513,2613,2714,2814,2914,3015,3115,
    3216,3316,3417,3517,3617,3718,3818,3918,4019,4119,4219,4320,4420,4520,4621,4721,
    4821,4921,5022,5122,5222,5322,5422,5523,5623,5723,5823,5923,6023,6123,6224,6324,
    6424,6524,6624,6724,6824,6924,7024,7124,7224,7323,7423,7523,7623,7723,7823,7923,
    8022,8122,8222,8322,8421,8521,8621,8720,8820,8919,9019,9119,9218,9318,9417,9517,
    9616,9716,9815,9914,10014,10113,10212,10312,10411,10510,10609,10709,10808,10907,11006,11105,
    11204,11303,11402,11501,11600,11699,11798,11897,11996,12095,12193,12292,12391,12490,12588,12687,
    12785,12884,12983,13081,13180,13278,13376,13475,13573,13672,13770,13868,13966,14065,14163,14261,
    14359,14457,14555,14653,14751,14849,14947,15045,15143,15240,15338,15436,15534,15631,15729,15826,
    15924,16021,16119,16216,16314,16411,16508,16606,16703,16800,16897,16994,17091,17188,17285,17382,
    17479,17576,17673,17770,17867,17963,18060,18156,18253,18350,18446,18543,18639,18735,18832,18928,
    19024,19120,19216,19313,19409,19505,19600,19696,19792,19888,19984,20080,20175,20271,20366,20462,
    20557,20653,20748,20844,20939,21034,21129,21224,21320,21415,21510,21604,21699,21794,21889,21984,
    22078,22173,22268,22362,22457,22551,22645,22740,22834,22928,23022,23116,23210,23304,23398,23492,
    23586,23680,23774,23867,23961,24054,24148,24241,24335,24428,24521,24614,24708,24801,24894,24987,
    25080,25172,25265,25358,25451,25543,25636,25728,25821,25913,26005,26098,26190,26282,26374,26466,
    26558,26650,26742,26833,26925,27017,27108,27200,27291,27382,27474,27565,27656,27747,27838,27929,
    28020,28111,28202,28293,28383,28474,28564,28655,28745,28835,28926,29016,29106,29196,29286,29376,
    29466,29555,29645,29735,29824,29914,30003,30093,30182,30271,30360,30449,30538,30627,30716,30805,
    30893,30982,31071,31159,31248,31336,31424,31512,31600,31688,31776,31864,31952,32040,32127,32215,
    32303,32390,32477,32565,32652,32739,32826,32913,33000,33087,33173,33260,33347,33433,33520,33606,
    33692,33778,33865,33951,34037,34122,34208,34294,34380,34465,34551,34636,34721,34806,34892,34977,
    35062,35146,35231,35316,35401,35485,35570,35654,35738,35823,35907,35991,36075,36159,36243,36326,
    36410,36493,36577,36660,36744,36827,36910,36993,37076,37159,37241,37324,37407,37489,37572,37654,
    37736,37818,37900,37982,38064,38146,38228,38309,38391,38472,38554,38635,38716,38797,38878,38959,
    39040,39120,39201,39282,39362,39442,39523,39603,39683,39763,39843,39922,40002,40082,40161,40241,
    40320,40399,40478,40557,40636,40715,40794,40872,40951,41029,41108,41186,41264,41342,41420,41498,
    41576,41653,41731,41808,41886,41963,42040,42117,42194,42271,42348,42424,42501,42578,42654,42730,
    42806,42882,42958,43034,43110,43186,43261,43337,43412,43487,43562,43638,43713,43787,43862,43937,
    44011,44086,44160,44234,44308,44382,44456,44530,44604,44677,44751,44824,44898,44971,45044,45117,
    45190,45262,45335,45408,45480,45552,45625,45697,45769,45841,45912,45984,46056,46127,46199,46270,
    46341,46412,46483,46554,46624,46695,46765,46836,46906,46976,47046,47116,47186,47256,47325,47395,
    47464,47534,47603,47672,47741,47809,47878,47947,48015,48084,48152,48220,48288,48356,48424,48491,
    48559,48626,48694,48761,48828,48895,48962,49029,49095,49162,49228,49295,49361,49427,49493,49559,
    49624,49690,49756,49821,49886,49951,50016,50081,50146,50211,50275,50340,50404,50468,50532,50596,
    50660,50724,50787,50851,50914,50977,51041,51104,51166,51229,51292,51354,51417,51479,51541,51603,
    51665,51727,51789,51850,51911,51973,52034,52095,52156,52217,52277,52338,52398,52459,52519,52579,
    52639,52699,52759,52818,52878,52937,52996,53055,53114,53173,53232,53290,53349,53407,53465,53523,
    53581,53639,53697,53754,53812,53869,53926,53983,54040,54097,54154,54210,54267,54323,54379,54435,
    54491,54547,54603,54658,54714,54769,54824,54879,54934,54989,55043,55098,55152,55206,55260,55314,
    55368,55422,55476,55529,55582,55636,55689,55742,55794,55847,55900,55952,56004,56056,56108,56160,
    56212,56264,56315,56367,56418,56469,56520,56571,56621,56672,56722,56773,56823,56873,56923,56972,
    57022,57072,57121,57170,57219,57268,57317,57366,57414,57463,57511,57559,57607,57655,57703,57750,
    57798,57845,57892,57939,57986,58033,58079,58126,58172,58219,58265,58311,58356,58402,58448,58493,
    58538,58583,58628,58673,58718,58763,58807,58851,58896,58940,58983,59027,59071,59114,59158,59201,
    59244,59287,59330,59372,59415,59457,59499,59541,59583,59625,59667,59708,59750,59791,59832,59873,
    59914,59954,59995,60035,60075,60116,60156,60195,60235,60275,60314,60353,60392,60431,60470,60509,
    60547,60586,60624,60662,60700,60738,60776,60813,60851,60888,60925,60962,60999,61035,61072,61108,
    61145,61181,61217,61253,61288,61324,61359,61394,61429,61464,61499,61534,61568,61603,61637,61671,
    61705,61739,61772,61806,61839,61873,61906,61939,61971,62004,62036,62069,62101,62133,62165,62197,
    62228,62260,62291,62322,62353,62384,62415,62445,62476,62506,62536,62566,62596,62626,62655,62685,
    62714,62743,62772,62801,62830,62858,62886,62915,62943,62971,62998,63026,63054,63081,63108,63135,
    63162,63189,63215,63242,63268,63294,63320,63346,63372,63397,63423,63448,63473,63498,63523,63547,
    63572,63596,63621,63645,63668,63692,63716,63739,63763,63786,63809,63832,63854,63877,63899,63922,
    63944,63966,63987,64009,64031,64052,64073,64094,64115,64136,64156,64177,64197,64217,64237,64257,
    64277,64296,64316,64335,64354,64373,64392,64410,64429,64447,64465,64483,64501,64519,64536,64554,
    64571,64588,64605,64622,64639,64655,64672,64688,64704,64720,64735,64751,64766,64782,64797,64812,
    64827,64841,64856,64870,64884,64899,64912,64926,64940,64953,64967,64980,64993,65006,65018,65031,
    65043,65055,65067,65079,65091,65103,65114,65126,65137,65148,65159,65169,65180,65190,65200,65210,
    65220,65230,65240,65249,65259,65268,65277,65286,65294,65303,65311,65320,65328,65336,65343,65351,
    65358,65366,65373,65380,65387,65393,65400,65406,65413,65419,65425,65430,65436,65442,65447,65452,
    65457,65462,65467,65471,65476,65480,65484,65488,65492,65495,65499,65502,65505,65508,65511,65514,
    65516,65519,65521,65523,65525,65527,65528,65530,65531,65532,65533,65534,65535,65535,65536,65536,
    65536,
};
// clang-format on

static inline int32_t kfm__sin_entry(uint32_t i) {
  i &= 4095u;
  if (i <= 1024u) return kfm_sin_quarter[i];
  if (i <= 2048u) return kfm_sin_quarter[2048u - i];
  if (i <= 3072u) return -kfm_sin_quarter[i - 2048u];
  return -kfm_sin_quarter[4096u - i];
}
/* sin/cos of a bam angle in Q16.16, bit-identical to the engine's kfm::sin/cos
 * (table lookup with linear interpolation between samples). */
static inline kfm_fx kfm_sin(uint16_t bam) {
  uint32_t i0 = (uint32_t)bam >> 4;
  int32_t f = (int32_t)(bam & 15u);
  int32_t s0 = kfm__sin_entry(i0);
  int32_t s1 = kfm__sin_entry(i0 + 1u);
  return s0 + (((s1 - s0) * f) >> 4);
}
static inline kfm_fx kfm_cos(uint16_t bam) { return kfm_sin((uint16_t)(bam + 0x4000u)); }

/* magnitude along a bam heading: (cos, sin) * mag (Vec2::fromAngle). */
static inline kfm_vec kfm_vec_from_bam(uint16_t bam, kfm_fx mag) {
  return kfm_v(kfm_fx_mul(kfm_cos(bam), mag), kfm_fx_mul(kfm_sin(bam), mag));
}
/* magnitude along an angle8 (a stick or aim byte). */
static inline kfm_vec kfm_angle_to_vec(uint8_t angle8, kfm_fx mag) {
  return kfm_vec_from_bam((uint16_t)((uint16_t)angle8 << 8), mag);
}
static inline uint16_t kfm_angle8_to_bam(uint8_t a) { return (uint16_t)((uint16_t)a << 8); }
/* the nearest angle8 of a bam angle. */
static inline uint8_t kfm_bam_to_angle8(uint16_t bam) {
  return (uint8_t)((((uint32_t)bam + 128u) >> 8) & 0xFFu);
}
/* signed shortest difference a - b in [-32768, 32767] bam. */
static inline int32_t kfm_bam_delta(uint16_t a, uint16_t b) {
  int32_t d = (int32_t)(uint16_t)(a - b);
  return d >= 32768 ? d - 65536 : d;
}

/* Rotates v into the first quadrant (x > 0, y >= 0) by exact quarter turns;
 * returns how many (v must not be zero). */
static inline uint32_t kfm__quadrant(kfm_vec v, int64_t* px, int64_t* py) {
  int64_t x = v.x;
  int64_t y = v.y;
  uint32_t q = 0;
  if (x <= 0 && y > 0) {
    *px = y;
    *py = -x;
    q = 1;
  } else if (x < 0 && y <= 0) {
    *px = -x;
    *py = -y;
    q = 2;
  } else if (x >= 0 && y < 0) {
    *px = -y;
    *py = x;
    q = 3;
  } else {
    *px = x;
    *py = y;
  }
  return q;
}
/* The first `steps` (<= 10) halvings of the sim's search over the quarter
 * turn: the largest m in [0, 2^steps) whose direction at bam m << (14 - steps)
 * is not counter-clockwise past (x, y). Those bam are table samples, read
 * without interpolation, and are exactly the midpoints the full search visits
 * first, so the result is the same. */
static inline uint32_t kfm__coarse_angle(int64_t x, int64_t y, uint32_t steps) {
  uint32_t lo = 0;
  uint32_t hi = 1u << steps;
  while (hi - lo > 1) {
    uint32_t mid = (lo + hi) / 2;
    uint32_t idx = mid << (10 - steps);
    int64_t c = ((int64_t)kfm_sin_quarter[1024u - idx] * y) - ((int64_t)kfm_sin_quarter[idx] * x);
    if (c >= 0) {
      lo = mid;
    } else {
      hi = mid;
    }
  }
  return lo;
}

/* The heading of v in bam (0 = +x, 16384 = +y), the zero vector giving 0:
 * the sim's integer atan2 (projectile steering), bit for bit. The vector is
 * rotated into the first quadrant by exact quarter turns, then the largest
 * angle whose table direction is not counter-clockwise past v is binary
 * searched with an exact int64 cross product (14 halvings: 10 on table
 * samples, 4 interpolated). */
static inline uint16_t kfm_angle_of(kfm_vec v) {
  int64_t x;
  int64_t y;
  uint32_t quadrant;
  uint32_t lo;
  uint32_t hi;
  if (v.x == 0 && v.y == 0) return 0;
  quadrant = kfm__quadrant(v, &x, &y);
  lo = kfm__coarse_angle(x, y, 10) * 16u;
  hi = lo + 16u;
  while (hi - lo > 1) {
    uint32_t mid = (lo + hi) / 2;
    int64_t c = ((int64_t)kfm_cos((uint16_t)mid) * y) - ((int64_t)kfm_sin((uint16_t)mid) * x);
    if (c >= 0) {
      lo = mid;
    } else {
      hi = mid;
    }
  }
  return (uint16_t)(lo + quadrant * 0x4000u);
}
/* The nearest angle8 of v's heading: kfm_bam_to_angle8(kfm_angle_of(v)),
 * exactly, in 7 halvings (the heading's 1/512 turn is all the rounding
 * needs). The zero vector gives 0. */
static inline uint8_t kfm_angle8_of(kfm_vec v) {
  int64_t x;
  int64_t y;
  uint32_t quadrant;
  if (v.x == 0 && v.y == 0) return 0;
  quadrant = kfm__quadrant(v, &x, &y);
  return (uint8_t)((quadrant * 128u + kfm__coarse_angle(x, y, 7) + 1u) >> 1);
}
/* The angle8 pointing from `from` to `to` (a stick or aim byte). */
static inline uint8_t kfm_dir_to(kfm_vec from, kfm_vec to) {
  return kfm_angle8_of(kfm_vec_sub(to, from));
}

/* ---- deterministic rng: xoshiro128**, seeded like the engine's kfm::Rng --- */

typedef struct kfm_rng {
  uint32_t s[4];
} kfm_rng;

static inline uint32_t kfm__rotl(uint32_t x, int k) { return (x << k) | (x >> (32 - k)); }
/* Seeds through splitmix32 (nearby seeds give unrelated streams). */
static inline void kfm_rng_seed(kfm_rng* r, uint64_t seed) {
  uint32_t x = (uint32_t)seed ^ ((uint32_t)(seed >> 32) * 0x9E3779B9u);
  int i;
  for (i = 0; i < 4; ++i) {
    uint32_t z;
    x += 0x9E3779B9u;
    z = x;
    z = (z ^ (z >> 16)) * 0x85EBCA6Bu;
    z = (z ^ (z >> 13)) * 0xC2B2AE35u;
    r->s[i] = z ^ (z >> 16);
  }
  if ((r->s[0] | r->s[1] | r->s[2] | r->s[3]) == 0) r->s[0] = 1;
}
static inline uint32_t kfm_rng_next(kfm_rng* r) {
  uint32_t* s = r->s;
  uint32_t result = kfm__rotl(s[1] * 5u, 7) * 9u;
  uint32_t t = s[1] << 9;
  s[2] ^= s[0];
  s[3] ^= s[1];
  s[1] ^= s[2];
  s[0] ^= s[3];
  s[2] ^= t;
  s[3] = kfm__rotl(s[3], 11);
  return result;
}
/* Uniform in [0, n); 0 for n == 0. */
static inline uint32_t kfm_rng_below(kfm_rng* r, uint32_t n) {
  return (uint32_t)(((uint64_t)kfm_rng_next(r) * n) >> 32);
}
/* Uniform in [lo, hi] (lo <= hi). */
static inline int32_t kfm_rng_range(kfm_rng* r, int32_t lo, int32_t hi) {
  uint64_t span = (uint64_t)((int64_t)hi - lo) + 1;
  uint32_t v = span > 0xFFFFFFFFu ? kfm_rng_next(r) : kfm_rng_below(r, (uint32_t)span);
  return (int32_t)(lo + (int64_t)v);
}

/* ---- logging -------------------------------------------------------------- */

/* A small text builder for kfm_log: kfm_text_str / kfm_text_int append (and
 * truncate at the capacity), kfm_text_log sends it. */
typedef struct kfm_text {
  char data[160];
  uint32_t len;
} kfm_text;

static inline void kfm_text_str(kfm_text* t, const char* s) {
  while (*s != 0 && t->len < sizeof t->data)
    t->data[t->len++] = *s++;
}
static inline void kfm_text_int(kfm_text* t, int64_t v) {
  char digits[20];
  int n = 0;
  uint64_t u = v < 0 ? (uint64_t)0 - (uint64_t)v : (uint64_t)v;
  if (v < 0 && t->len < sizeof t->data) t->data[t->len++] = '-';
  do {
    digits[n++] = (char)('0' + (int)(u % 10u));
    u /= 10u;
  } while (u != 0 && n < 20);
  while (n > 0 && t->len < sizeof t->data)
    t->data[t->len++] = digits[--n];
}
static inline void kfm_text_log(const kfm_text* t) { kfm_log(t->data, t->len); }

/* ---- observation accessors ------------------------------------------------ */

static inline kfm_vec kfm_self_pos(const kfm_observation* o) {
  return kfm_v(o->self.posX, o->self.posY);
}
static inline kfm_vec kfm_self_vel(const kfm_observation* o) {
  return kfm_v(o->self.velX, o->self.velY);
}
static inline kfm_vec kfm_player_pos(const kfm_obs_player* p) { return kfm_v(p->posX, p->posY); }
static inline kfm_vec kfm_player_vel(const kfm_obs_player* p) { return kfm_v(p->velX, p->velY); }
static inline kfm_vec kfm_banana_pos(const kfm_obs_banana* b) { return kfm_v(b->posX, b->posY); }
static inline kfm_vec kfm_banana_vel(const kfm_obs_banana* b) { return kfm_v(b->velX, b->velY); }

/* Whether the ability slot fires on a press this tick: it has a kind, is Idle
 * with no cooldown, and its button was up on the last applied tick (a held
 * button never re-fires). */
static inline int kfm_ability_ready(const kfm_observation* o, int slot) {
  const kfm_obs_ability* a = &o->self.abilities[slot];
  return a->kind != KFM_ABILITY_NONE && a->phase == KFM_ABILITY_IDLE && a->cooldownTicks == 0 &&
         (o->self.lastButtons & a->button) == 0;
}

/* Whether banana i can hurt the own monkey: in flight and not its own (the
 * own outbound banana flies through its thrower, the own returning one is
 * caught on contact). */
static inline int kfm_banana_is_threat(const kfm_observation* o, int i) {
  const kfm_obs_banana* b = &o->bananas[i];
  return b->valid != 0 && b->phase < KFM_BANANA_GROUNDED && b->ownerRelation != KFM_REL_SELF;
}

/* The slot of the nearest living enemy (ties: the lower slot), preferring one
 * without spawn protection; -1 when there is none. */
static inline int kfm_nearest_enemy(const kfm_observation* o) {
  kfm_vec me = kfm_self_pos(o);
  int best = -1;
  int bestProtected = 1;
  uint64_t bestD = UINT64_MAX;
  int i;
  for (i = 0; i < KFM_MAX_PLAYERS; ++i) {
    const kfm_obs_player* p = &o->players[i];
    int prot;
    uint64_t d;
    if (p->alive == 0 || p->relation != KFM_REL_ENEMY) continue;
    prot = kfm_has_tag(p->tags, KFM_TAG_INVULNERABLE);
    d = kfm_vec_len_sq(kfm_vec_sub(kfm_player_pos(p), me));
    if ((bestProtected && !prot) || (prot == bestProtected && d < bestD)) {
      best = i;
      bestD = d;
      bestProtected = prot;
    }
  }
  return best;
}

/* The threatening banana that comes closest to the own monkey soonest: among
 * kfm_banana_is_threat bananas whose closest approach (approachTicks within
 * maxTicks, on its current velocity) passes within contact + margin, the one
 * with the smallest approachTicks (ties: the lower index); -1 when none. A
 * returning banana curves, so this is a cue: kfm_predict_banana refines it. */
static inline int kfm_nearest_threat(const kfm_observation* o, uint32_t maxTicks, kfm_fx margin) {
  int best = -1;
  uint32_t bestTicks = 0xFFFFFFFFu;
  int i;
  if (o->self.alive == 0) return -1;
  for (i = 0; i < KFM_MAX_BANANAS; ++i) {
    const kfm_obs_banana* b = &o->bananas[i];
    if (!kfm_banana_is_threat(o, i) || b->approachTicks > maxTicks) continue;
    if (b->approachDistance >= o->self.radius + b->radius + margin) continue;
    if (b->approachTicks < bestTicks) {
      best = i;
      bestTicks = b->approachTicks;
    }
  }
  return best;
}

/* Time of impact, in Q16.16 ticks: the earliest t in [0, maxTicks] with
 * |rel + t * vel| < radius, for a relative position `rel` (Q16.16) and a
 * relative velocity `vel` (Q16.16 per tick); 0 when already in contact, -1
 * when it never comes that close. Closed-form closest approach, then a
 * binary search to 1/65536 tick on the approaching side (overflow-free for
 * arena-sized inputs, int64 only). */
static inline kfm_fx kfm_time_to_impact(kfm_vec rel, kfm_vec vel, kfm_fx radius,
                                        uint32_t maxTicks) {
  uint64_t r2 = kfm_fx_sq(radius);
  int64_t a = ((int64_t)vel.x * vel.x) + ((int64_t)vel.y * vel.y);
  int64_t b = ((int64_t)rel.x * vel.x) + ((int64_t)rel.y * vel.y);
  int64_t tmax = (int64_t)(maxTicks > 32767u ? 32767u : maxTicks) * 65536;
  int64_t tstar;
  int64_t lo = 0;
  int64_t hi;
  if (kfm_vec_len_sq(rel) < r2) return 0;
  if (a == 0 || b >= 0) return -1;
  tstar = (-b * 65536) / a;
  hi = tstar < tmax ? tstar : tmax;
#define KFM__AT(t) \
  kfm_vec_add(     \
      rel, kfm_v((kfm_fx)(((int64_t)vel.x * (t)) >> 16), (kfm_fx)(((int64_t)vel.y * (t)) >> 16)))
  if (kfm_vec_len_sq(KFM__AT(hi)) >= r2) return -1;
  while (hi - lo > 1) {
    int64_t mid = (lo + hi) / 2;
    if (kfm_vec_len_sq(KFM__AT(mid)) < r2) {
      hi = mid;
    } else {
      lo = mid;
    }
  }
#undef KFM__AT
  return (kfm_fx)hi;
}

/* min over s in [0, 1] of |r + s*d|^2 (raw Q32.32): the closest two moving
 * circles come within one tick, r = relative start, d = relative motion over
 * the tick. The sim's collision is swept the same way. */
static inline uint64_t kfm_segment_min_sq(kfm_vec r, kfm_vec d) {
  int64_t rd = ((int64_t)r.x * d.x) + ((int64_t)r.y * d.y);
  int64_t dd;
  kfm_fx s;
  if (rd >= 0) return kfm_vec_len_sq(r);
  dd = ((int64_t)d.x * d.x) + ((int64_t)d.y * d.y);
  if (-rd >= dd) return kfm_vec_len_sq(kfm_vec_add(r, d));
  s = (kfm_fx)((-rd * 65536) / dd);
  return kfm_vec_len_sq(kfm_vec_add(r, kfm_vec_scale(d, s)));
}

/* ---- banana flight (the sim's ProjectileSystem) --------------------------- */

typedef struct kfm_flight {
  kfm_vec pos;
  kfm_vec vel;
  kfm_fx speed;  /* flight speed (base x the thrower's multiplier) */
  kfm_fx radius; /* contact radius */
  uint16_t heading;
  uint16_t ticksInPhase;
  uint8_t phase; /* KFM_BANANA_* */
  uint8_t owner;
} kfm_flight;

static inline kfm_flight kfm_flight_of(const kfm_obs_banana* b) {
  kfm_flight f;
  f.pos = kfm_banana_pos(b);
  f.vel = kfm_banana_vel(b);
  f.speed = b->speed;
  f.radius = b->radius;
  f.heading = b->headingBam;
  f.ticksInPhase = b->ticksInPhase;
  f.phase = b->phase;
  f.owner = b->owner;
  return f;
}

/* One axis of a bounce off the interior walls (statics::moveAxis, Bounce). */
static inline int kfm__bounce_axis(const kfm_obs_arena* ar, int isX, kfm_vec* pos, kfm_vec* vel,
                                   kfm_fx d, kfm_fx r, kfm_fx rest) {
  kfm_fx* pa = isX ? &pos->x : &pos->y;
  kfm_fx* va = isX ? &vel->x : &vel->y;
  kfm_fx start = *pa;
  kfm_fx end = start + d;
  kfm_fx face = 0;
  int blocked = 0;
  int i;
  for (i = 0; d != 0 && i < ar->wallCount && i < KFM_MAX_WALLS; ++i) {
    const kfm_obs_box* w = &ar->walls[i];
    kfm_fx other = isX ? pos->y : pos->x;
    kfm_fx f;
    if (other < (isX ? w->minY : w->minX) || other > (isX ? w->maxY : w->maxX)) continue;
    if (d > 0) {
      f = (isX ? w->minX : w->minY) - r;
      if (start <= f && end > f && (!blocked || f < face)) {
        face = f;
        blocked = 1;
      }
    } else {
      f = (isX ? w->maxX : w->maxY) + r;
      if (start >= f && end < f && (!blocked || f > face)) {
        face = f;
        blocked = 1;
      }
    }
  }
  if (!blocked) {
    *pa = end;
    return 0;
  }
  *pa = (kfm_fx)((int64_t)face * 2 - end);
  *va = kfm_fx_mul(-*va, rest);
  return 1;
}

/* Pushes the circle out of every wall it overlaps (statics::pushOut, Bounce). */
static inline int kfm__push_out(const kfm_obs_arena* ar, kfm_vec* pos, kfm_vec* vel, kfm_fx r,
                                kfm_fx rest) {
  int moved = 0;
  int i;
  for (i = 0; i < ar->wallCount && i < KFM_MAX_WALLS; ++i) {
    const kfm_obs_box* w = &ar->walls[i];
    kfm_vec closest =
        kfm_v(kfm_fx_clamp(pos->x, w->minX, w->maxX), kfm_fx_clamp(pos->y, w->minY, w->maxY));
    kfm_vec normal;
    kfm_fx vn;
    if (kfm_vec_len_sq(kfm_vec_sub(*pos, closest)) >= kfm_fx_sq(r)) continue;
    if (closest.x == pos->x && closest.y == pos->y) {
      kfm_fx left = pos->x - w->minX, right = w->maxX - pos->x;
      kfm_fx down = pos->y - w->minY, up = w->maxY - pos->y;
      kfm_fx best = kfm_fx_min(kfm_fx_min(left, right), kfm_fx_min(down, up));
      if (best == left) {
        pos->x = w->minX - r;
        normal = kfm_v(-KFM_FX_ONE, 0);
      } else if (best == right) {
        pos->x = w->maxX + r;
        normal = kfm_v(KFM_FX_ONE, 0);
      } else if (best == down) {
        pos->y = w->minY - r;
        normal = kfm_v(0, -KFM_FX_ONE);
      } else {
        pos->y = w->maxY + r;
        normal = kfm_v(0, KFM_FX_ONE);
      }
    } else {
      normal = kfm_vec_normalized(kfm_vec_sub(*pos, closest));
      if (normal.x == 0 && normal.y == 0) continue;
      *pos = kfm_vec_add(closest, kfm_vec_scale(normal, r));
    }
    vn = kfm_vec_dot(*vel, normal);
    if (vn < 0) *vel = kfm_vec_sub(*vel, kfm_vec_scale(normal, kfm_fx_mul(vn, KFM_FX_ONE + rest)));
    moved = 1;
  }
  return moved;
}

/* One axis against the outer box (statics::reflectAxis). */
static inline int kfm__reflect_outer(kfm_fx* p, kfm_fx* v, kfm_fx limit, kfm_fx rest) {
  int bounced = 0;
  if (*p > limit) {
    *p = (kfm_fx)((int64_t)limit * 2 - *p);
    *v = kfm_fx_mul(-*v, rest);
    bounced = 1;
  } else if (*p < -limit) {
    *p = (kfm_fx)((int64_t)-limit * 2 - *p);
    *v = kfm_fx_mul(-*v, rest);
    bounced = 1;
  }
  *p = kfm_fx_clamp(*p, -limit, limit);
  return bounced;
}

/* Advances a banana by one Playing tick, exactly as the sim's ProjectileSystem
 * does: ticksInPhase grows (saturating); an Outbound banana at
 * bananaOutboundTicks turns Returning; a Returning one whose owner is gone
 * (ownerAlive 0) or past bananaMaxReturnTicks is Grounded (stops), else it
 * turns toward ownerPos by at most bananaTurnRateBam and flies at its speed;
 * then it moves by its velocity through the walls and the outer box (a
 * crossed face mirrors the overshoot and scales that velocity component by
 * bananaRestitution). ownerPos is the owner's position after its move on the
 * same tick. Contacts (catches, hits, parries) are not modelled. Returns 1 on
 * a bounce. */
static inline int kfm_flight_step(kfm_flight* f, const kfm_observation* o, kfm_vec ownerPos,
                                  int ownerAlive) {
  const kfm_obs_rules* ru = &o->rules;
  const kfm_obs_arena* ar = &o->arena;
  kfm_vec pos;
  kfm_vec vel;
  int hitWall = 0;
  int hitOuter;
  if (f->ticksInPhase < 0xFFFFu) ++f->ticksInPhase;
  if (f->phase >= KFM_BANANA_GROUNDED) return 0;
  if (f->phase == KFM_BANANA_OUTBOUND && f->ticksInPhase >= ru->bananaOutboundTicks) {
    f->phase = KFM_BANANA_RETURNING;
    f->ticksInPhase = 0;
  }
  if (f->phase == KFM_BANANA_RETURNING) {
    kfm_vec to;
    if (!ownerAlive || f->ticksInPhase > ru->bananaMaxReturnTicks) {
      f->phase = KFM_BANANA_GROUNDED;
      f->ticksInPhase = 0;
      f->vel = kfm_v(0, 0);
      return 0;
    }
    to = kfm_vec_sub(ownerPos, f->pos);
    if (to.x != 0 || to.y != 0) {
      int32_t rate = ru->bananaTurnRateBam;
      int32_t delta = kfm_bam_delta(kfm_angle_of(to), f->heading);
      delta = delta < -rate ? -rate : (delta > rate ? rate : delta);
      f->heading = (uint16_t)(f->heading + (uint16_t)delta);
    }
    f->vel = kfm_vec_from_bam(f->heading, f->speed);
  } else if (f->vel.x != 0 || f->vel.y != 0) {
    f->heading = kfm_angle_of(f->vel);
  }
  pos = f->pos;
  vel = f->vel;
  if (ar->wallCount == 0) {
    pos = kfm_vec_add(pos, vel);
  } else {
    kfm_vec delta = vel;
    hitWall |= kfm__bounce_axis(ar, 1, &pos, &vel, delta.x, f->radius, ru->bananaRestitution);
    hitWall |= kfm__push_out(ar, &pos, &vel, f->radius, ru->bananaRestitution);
    hitWall |= kfm__bounce_axis(ar, 0, &pos, &vel, delta.y, f->radius, ru->bananaRestitution);
    hitWall |= kfm__push_out(ar, &pos, &vel, f->radius, ru->bananaRestitution);
  }
  {
    kfm_fx lx = kfm_fx_max(ar->halfExtents.x - f->radius, 0);
    kfm_fx ly = kfm_fx_max(ar->halfExtents.y - f->radius, 0);
    int bx = kfm__reflect_outer(&pos.x, &vel.x, lx, ru->bananaRestitution);
    int by = kfm__reflect_outer(&pos.y, &vel.y, ly, ru->bananaRestitution);
    hitOuter = bx || by;
  }
  f->pos = pos;
  f->vel = vel;
  if ((hitWall || hitOuter) && (vel.x != 0 || vel.y != 0)) f->heading = kfm_angle_of(vel);
  return hitWall || hitOuter;
}

/* Predicts banana i over `ticks` Playing ticks: out[0] = now, out[k] = after
 * k ticks (out holds ticks + 1 entries). The owner is assumed to keep its
 * current velocity and to stay alive (exact for an Outbound banana until it
 * turns, approximate once it homes on a moving owner). Returns the number of
 * ticks before it lands (ticks when it is still flying at the end); a landed
 * banana keeps its position. */
static inline int kfm_predict_banana(const kfm_observation* o, int i, int ticks, kfm_vec* out) {
  const kfm_obs_banana* b = &o->bananas[i];
  const kfm_obs_player* owner = b->owner < KFM_MAX_PLAYERS ? &o->players[b->owner] : 0;
  kfm_flight f = kfm_flight_of(b);
  int flying = b->phase >= KFM_BANANA_GROUNDED ? 0 : ticks;
  int k;
  out[0] = f.pos;
  for (k = 1; k <= ticks; ++k) {
    int alive = owner != 0 && owner->alive != 0;
    kfm_vec op = alive
                     ? kfm_vec_add(kfm_player_pos(owner), kfm_vec_mul_int(kfm_player_vel(owner), k))
                     : kfm_v(0, 0);
    if (f.phase != KFM_BANANA_GROUNDED) {
      kfm_flight_step(&f, o, op, alive);
      if (f.phase == KFM_BANANA_GROUNDED && flying == ticks) flying = k - 1;
    }
    out[k] = f.pos;
  }
  return flying;
}

/* ---- the dash-catch ------------------------------------------------------- */

#define KFM_PREDICT_MAX 48 /* the longest path kfm_dash_catch_dir predicts */

/* Whether a Dash pressed in this tick's answer catches banana i: the dash slot
 * (abilities[0]) is a ready Dash with a perfect window, the own monkey has
 * room (ammo < ammoMax), the banana is in flight and not the own outbound
 * one, and a dash toward one of the banana's predicted positions inside the
 * window meets it within the window [press, press + defPerfectWindowTicks)
 * with `margin` (Q16.16) to spare. The press lands inputDelayTicks later (the
 * monkey keeps its current velocity until then); the dash moves at defSpeed,
 * walls ignored. Returns 1 and writes the stick direction to *dir. */
static inline int kfm_dash_catch_dir(const kfm_observation* o, int i, kfm_fx margin, uint8_t* dir) {
  const kfm_obs_ability* dash = &o->self.abilities[0];
  const kfm_obs_banana* b = &o->bananas[i];
  kfm_vec path[KFM_PREDICT_MAX + 1];
  uint32_t lead = o->match.inputDelayTicks;
  uint32_t window = dash->defPerfectWindowTicks;
  uint32_t last;
  uint32_t aim;
  kfm_vec start;
  kfm_fx contact;
  uint64_t reach;
  if (o->self.alive == 0 || dash->kind != KFM_ABILITY_DASH || !kfm_ability_ready(o, 0)) return 0;
  if (window == 0 || o->self.ammo >= o->self.ammoMax) return 0;
  if (b->valid == 0 || b->phase == KFM_BANANA_GROUNDED) return 0;
  if (b->ownerRelation == KFM_REL_SELF && b->phase == KFM_BANANA_OUTBOUND) return 0;
  last = lead + window;
  if (last > KFM_PREDICT_MAX) return 0;
  if ((uint32_t)kfm_predict_banana(o, i, (int)last, path) < last) return 0; /* lands first */
  start = kfm_vec_add(kfm_self_pos(o), kfm_vec_mul_int(kfm_self_vel(o), (int32_t)lead));
  contact = o->self.radius + b->radius - margin;
  if (contact <= 0) return 0;
  reach = kfm_fx_sq(contact);
  for (aim = lead + 1; aim <= last; ++aim) {
    uint8_t d = kfm_dir_to(start, path[aim]);
    kfm_vec step = kfm_angle_to_vec(d, dash->defSpeed);
    kfm_vec me = start;
    uint32_t k;
    for (k = lead + 1; k <= last; ++k) {
      kfm_vec next = kfm_vec_add(me, step);
      kfm_vec r = kfm_vec_sub(path[k - 1], me);
      kfm_vec dv = kfm_vec_sub(kfm_vec_sub(path[k], path[k - 1]), step);
      if (kfm_segment_min_sq(r, dv) < reach) {
        *dir = d;
        return 1;
      }
      me = next;
    }
  }
  return 0;
}

#ifdef __cplusplus
}
#endif
#endif /* KFM_BOT_H */
