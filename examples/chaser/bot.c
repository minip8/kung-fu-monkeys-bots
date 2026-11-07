/* chaser: the C SDK's example bot (bot ABI v1). Each tick, in priority order:
 *   1. dash-catch an incoming banana when the perfect window allows it;
 *   2. dodge a banana about to hit by dashing perpendicular to its flight
 *      (walking sideways while the dash cools down);
 *   3. dash attack: holding the bananas that arm it, dash into an enemy in
 *      reach (a dashing monkey with ammo strikes on contact);
 *   4. move: with bananas in hand, close in on the nearest enemy and circle it
 *      at throwing range; empty-handed, meet the own banana on its way back or
 *      fetch the nearest one lying around; stay off the outer walls;
 *   5. throw at the nearest enemy, leading its motion, when it is in range.
 * Buttons are pressed as fresh edges only (never two ticks in a row). Build:
 * tools/build.sh examples/chaser out/chaser.wasm. */
#define KFM_BOT_MAIN
#include "kfm_bot.h"

#define CHASER_RANGE_NEAR (4 * KFM_FX_ONE) /* closer than this: back off */
#define CHASER_RANGE_FAR (6 * KFM_FX_ONE)  /* farther than this: close in */
#define CHASER_WALL_MARGIN (3 * KFM_FX_ONE / 2)
#define CHASER_DODGE_TICKS 10u
#define CHASER_CATCH_TICKS 12u

static kfm_rng chaser_rng;
static uint8_t chaser_prev_buttons;
static uint8_t chaser_orbit; /* +64 or -64: the side it circles the target on */

void kfm_init(uint64_t seed, uint32_t slot) {
  kfm_text t = {{0}, 0};
  kfm_rng_seed(&chaser_rng, seed);
  chaser_prev_buttons = 0;
  chaser_orbit = (kfm_rng_next(&chaser_rng) & 1u) != 0 ? 64 : 192;
  kfm_text_str(&t, "chaser: slot ");
  kfm_text_int(&t, (int64_t)slot);
  kfm_text_str(&t, " ready\n");
  kfm_text_log(&t);
}

/* The stick direction from `me` toward `to`, bent toward the arena centre
 * when `me` is close to the outer wall. */
static uint8_t chaser_steer(const kfm_observation* o, kfm_vec me, uint8_t dir) {
  kfm_fx hx = o->arena.halfExtents.x - CHASER_WALL_MARGIN;
  kfm_fx hy = o->arena.halfExtents.y - CHASER_WALL_MARGIN;
  kfm_vec push = kfm_v(0, 0);
  if (me.x > hx) push.x = -KFM_FX_ONE;
  if (me.x < -hx) push.x = KFM_FX_ONE;
  if (me.y > hy) push.y = -KFM_FX_ONE;
  if (me.y < -hy) push.y = KFM_FX_ONE;
  if (push.x == 0 && push.y == 0) return dir;
  return kfm_dir_to(kfm_v(0, 0), kfm_vec_add(kfm_angle_to_vec(dir, KFM_FX_ONE), push));
}

uint64_t kfm_act(uint32_t tick) {
  const kfm_observation* o = &kfm_obs;
  const kfm_obs_ability* dash = &o->self.abilities[0];
  const kfm_obs_ability* thr = &o->self.abilities[1];
  kfm_vec me;
  uint8_t buttons = 0;
  uint8_t move_dir = 0;
  uint8_t move_mag = 0;
  uint8_t aim = o->self.facing;
  int target;
  int threat;
  int moving = 0;
  int tries;
  int i;
  (void)tick;
  if (!kfm_obs_ok(o) || o->self.alive == 0 || o->match.phase != KFM_PHASE_PLAYING) {
    chaser_prev_buttons = 0;
    return kfm_pack_input(0, 0, aim, 0, 0);
  }
  me = kfm_self_pos(o);
  target = kfm_nearest_enemy(o);

  /* 1. dash-catch: the first two threats due within CHASER_CATCH_TICKS (each
   * try predicts the banana's flight: keep the fuel bounded). */
  for (i = 0, tries = 0; i < KFM_MAX_BANANAS && buttons == 0 && tries < 2; ++i) {
    uint8_t d;
    if (!kfm_banana_is_threat(o, i) || o->bananas[i].approachTicks > CHASER_CATCH_TICKS) continue;
    ++tries;
    if (kfm_dash_catch_dir(o, i, KFM_FX_ONE / 8, &d)) {
      buttons = dash->button;
      move_dir = d;
      move_mag = 255;
      moving = 1;
    }
  }

  /* 2. dodge. */
  threat = kfm_nearest_threat(o, CHASER_DODGE_TICKS, KFM_FX_ONE / 4);
  if (!moving && threat >= 0) {
    const kfm_obs_banana* b = &o->bananas[threat];
    kfm_vec v = kfm_banana_vel(b);
    kfm_vec off = kfm_vec_sub(me, kfm_banana_pos(b));
    /* The side of the banana's line the monkey is on: dodge further that way. */
    int64_t side = ((int64_t)v.x * off.y) - ((int64_t)v.y * off.x);
    uint8_t heading = kfm_angle8_of(v);
    move_dir = (uint8_t)(heading + (side >= 0 ? 64 : 192));
    move_dir = chaser_steer(o, me, move_dir);
    move_mag = 255;
    moving = 1;
    if (kfm_ability_ready(o, 0) && dash->kind == KFM_ABILITY_DASH) buttons |= dash->button;
  }

  /* 3. dash attack: holding the ammo that arms it, dash into a target in reach. */
  if (!moving && target >= 0 && o->self.ammo > 0 && o->self.ammo >= o->rules.dashAttackMinAmmo &&
      dash->kind == KFM_ABILITY_DASH && kfm_ability_ready(o, 0)) {
    const kfm_obs_player* t = &o->players[target];
    kfm_fx reach = dash->defSpeed * (int32_t)dash->defActiveTicks * 3 / 4;
    kfm_vec at = kfm_vec_add(kfm_player_pos(t), kfm_vec_mul_int(kfm_player_vel(t), 3));
    if (kfm_vec_dist(me, at) < reach && !kfm_has_tag(t->tags, KFM_TAG_INVULNERABLE) &&
        !kfm_has_tag(t->tags, KFM_TAG_SHIELDED)) {
      buttons |= dash->button;
      move_dir = kfm_dir_to(me, at);
      move_mag = 255;
      moving = 1;
    }
  }

  /* 4. move. */
  if (!moving) {
    kfm_vec goal = me;
    int have_goal = 0;
    if (o->self.ammo == 0) {
      /* The own returning banana, else the nearest banana pickup. */
      uint64_t best = UINT64_MAX;
      for (i = 0; i < KFM_MAX_BANANAS; ++i) {
        const kfm_obs_banana* b = &o->bananas[i];
        if (b->valid != 0 && b->ownerRelation == KFM_REL_SELF && b->phase == KFM_BANANA_RETURNING) {
          goal = kfm_banana_pos(b);
          have_goal = 1;
          best = 0;
        }
      }
      for (i = 0; i < KFM_MAX_PICKUPS && best != 0; ++i) {
        const kfm_obs_pickup* p = &o->pickups[i];
        kfm_vec at = kfm_v(p->posX, p->posY);
        uint64_t d = kfm_vec_len_sq(kfm_vec_sub(at, me));
        if (p->valid == 0 || p->kind != KFM_PICKUP_BANANA || d >= best) continue;
        best = d;
        goal = at;
        have_goal = 1;
      }
    }
    if (have_goal) {
      move_dir = kfm_dir_to(me, goal); /* a banana by the wall is still worth it */
      move_mag = 255;
    } else if (target >= 0) {
      kfm_vec tp = kfm_player_pos(&o->players[target]);
      kfm_fx dist = kfm_vec_dist(me, tp);
      uint8_t toward = kfm_dir_to(me, tp);
      /* An empty-handed target cannot shoot back: run it down for a dash attack. */
      if (dist > CHASER_RANGE_FAR || o->self.ammo == 0 || o->players[target].ammo == 0) {
        move_dir = o->self.ammo == 0 ? (uint8_t)(toward + 128) : toward; /* unarmed: keep away */
      } else if (dist < CHASER_RANGE_NEAR) {
        move_dir = (uint8_t)(toward + 128 + (chaser_orbit == 64 ? -32 : 32));
      } else {
        move_dir = (uint8_t)(toward + chaser_orbit);
      }
      move_mag = 255;
      if ((kfm_rng_next(&chaser_rng) & 255u) == 0) chaser_orbit = (uint8_t)(256 - chaser_orbit);
      move_dir = chaser_steer(o, me, move_dir);
    }
  }

  /* 5. throw, leading the target. */
  if (target >= 0) {
    const kfm_obs_player* t = &o->players[target];
    kfm_vec tp = kfm_player_pos(t);
    kfm_vec tv = kfm_player_vel(t);
    kfm_fx speed = o->rules.bananaSpeed;
    kfm_fx offset = o->rules.bananaSpawnOffset;
    int32_t wind_up = (int32_t)o->match.inputDelayTicks + thr->defStartupTicks;
    kfm_vec from = kfm_vec_add(me, kfm_vec_mul_int(kfm_self_vel(o), wind_up));
    kfm_vec at = tp;
    kfm_fx range = speed * (int32_t)o->rules.bananaOutboundTicks + offset - KFM_FX_HALF;
    int it;
    for (it = 0; it < 3 && speed > 0; ++it) {
      kfm_fx flight = kfm_fx_div(kfm_fx_max(kfm_vec_dist(from, at) - offset, 0), speed);
      at = kfm_vec_add(tp, kfm_vec_scale(tv, flight + kfm_fx_from_int(wind_up)));
    }
    aim = kfm_dir_to(from, at);
    if (o->self.ammo > 0 && thr->kind == KFM_ABILITY_THROW && kfm_ability_ready(o, 1) &&
        kfm_vec_dist(from, at) <= range && !kfm_has_tag(t->tags, KFM_TAG_INVULNERABLE) &&
        (buttons & dash->button) == 0) {
      buttons |= thr->button;
    }
  }

  buttons = (uint8_t)(buttons & ~chaser_prev_buttons);
  chaser_prev_buttons = buttons;
  return kfm_pack_input(move_dir, move_mag, aim, buttons, 0);
}
