// starter: the C++ bot to copy (bot ABI v1, sdk/cpp/kfm_bot.hpp). It plays
// both spaces: the continuous arenas (the stick moves the monkey freely) and
// the grid (a cell per turn, the contest mode). Each tick, in priority order:
//   1. catch: dash into a banana about to arrive when the perfect window allows;
//   2. dodge: step off the path of a banana that would hit it standing still;
//   3. move: empty-handed, fetch a banana; armed on a hill (the contest),
//      go to the hill and hold it; else keep a throwing distance from the
//      nearest enemy; with nobody in sight (fog), go where an enemy was last
//      seen or called out, else explore;
//   4. throw at the nearest enemy, leading its motion, when the line is clear;
//   5. grid with a call ability: call out the cell of an enemy it sees.
// Everything it keeps between ticks is a member with a default initialiser
// (KFM_BOT needs a constant-initialised type). Fuel: each banana prediction
// costs thousands of instructions, so it predicts at most two a tick and
// skips bananas that cannot arrive in time.
//
// Where to go from here: the dash attack (a dash into an enemy while holding
// a banana hits it; chaser does it), charged throws (hold Throw to fly
// faster and farther, continuous only), playing the catcher role on purpose
// (the catcher's dash has a window on the grid; nobody else's does), reading
// calls beyond the last sighting and agreeing on symbols with teammates,
// pits (kfm::inPit is a cautious test; a dash crosses a pit), the latch of
// longer grid turns (ticksToLatch), and a real search instead of rules.
//
// Build: tools/build.sh examples/starter out/starter.wasm
#define KFM_BOT_MAIN
#include "kfm_bot.hpp"

using kfm::Banana;
using kfm::Cell;
using kfm::Fixed;
using kfm::Input;
using kfm::Monkey;
using kfm::Observation;
using kfm::Self;
using kfm::Vec2;

namespace {
constexpr Fixed kNear = Fixed::fromInt(4);         // continuous: back off when closer
constexpr Fixed kFar = Fixed::fromInt(6);          // continuous: close in when farther
constexpr Fixed kWallMargin = Fixed::ratio(3, 2);  // continuous: stay off the outer walls
constexpr Fixed kMargin = Fixed::ratio(1, 8);      // safety on every contact test
constexpr int kLook = 10;                          // ticks of flight a dodge looks ahead
constexpr int kGridLook = 4;                       // the same on the grid (bananas bounce a lot)
constexpr int kGridKeep = 2;                       // grid: back off when closer (cells)
constexpr int kGridThrow = 3;                      // grid: throw from this close (cells)
constexpr uint32_t kCallEvery = 8;                 // ticks between two calls
constexpr uint32_t kMemory = 600;                  // ticks a sighting stays a goal
}  // namespace

struct Starter {
  kfm_rng rng{};
  kfm::Grid grid{};
  Fixed speed{};       // the own bananas' flight speed, once one flew
  uint8_t orbit = 64;  // continuous: +64 or -64, the side it circles on
  uint8_t prevButtons = 0;
  Vec2 seen{};  // where an enemy was last seen or called out
  uint32_t seenTick = 0;
  bool haveSeen = false;
  Cell wander{};  // grid: an exploration goal
  uint32_t lastCall = 0;

  void init(uint64_t seed, uint32_t slot) {
    kfm_rng_seed(&rng, seed);
    orbit = (kfm_rng_next(&rng) & 1u) != 0 ? 64 : 192;
    kfm_text t = {{0}, 0};
    kfm_text_str(&t, "starter: slot ");
    kfm_text_int(&t, slot);
    kfm_text_log(&t);
  }

  Input act(uint32_t tick, const Observation& o) {
    Input in = decide(tick, o);
    in.release(prevButtons);  // a button fires on a fresh press only
    prevButtons = in.buttons;
    return in;
  }

  Input decide(uint32_t tick, const Observation& o) {
    const Self me = o.self();
    Input in;
    in.aim(me.facing());
    if (!o.playing() || !me.alive()) return in;
    grid.update(o);
    for (Banana b : o.bananas())
      if (b.mine() && b.inFlight()) speed = b.speed();
    const Monkey target = kfm::nearestEnemy(o);
    remember(o, tick, target);
    const kfm::Ability dash = me.ability(kfm::kSlotDash);

    // The two bananas that may hit soonest, predicted (kfm::findThreats
    // skips the ones that cannot arrive in time; each prediction costs fuel).
    const int look = o.grid() ? kGridLook : kLook;
    kfm::Threats th;
    kfm::findThreats(o, look, kMargin, 2, th);

    // 1. Catch: a dash that meets a threat inside the perfect window.
    for (int i = 0; i < th.count; ++i) {
      if (auto d = kfm::dashCatchDir(o, grid, th.banana[i], th.path[i], kMargin)) {
        return in.move(d.value).press(dash.button());
      }
    }

    // 2. Dodge the threat that would hit soonest if the monkey stood still.
    int soonest = -1;
    int hitIn = 1000;
    for (int i = 0; i < th.count; ++i) {
      const int t =
          th.path[i].firstContact(me.pos(), me.radius() + th.banana[i].radius() + kMargin);
      if (t >= 0 && t < hitIn) {
        soonest = i;
        hitIn = t;
      }
    }
    if (soonest >= 0) {
      const Banana b = th.banana[soonest];
      if (o.grid() ? dodgeOnGrid(o, b, th.path[soonest], in) : dodge(o, b, hitIn, in)) return in;
    }

    // 3. Move, and 4. throw.
    if (o.grid()) {
      moveOnGrid(o, tick, target, in);
    } else {
      move(o, target, in);
    }
    throwAt(o, target, in);
    callOut(o, tick, target, in);
    return in;
  }

  // The last enemy seen, or the cell an ally called out (the reference bot's
  // encoding, so it understands them too: (col / 2) | (row / 2) << 4).
  void remember(const Observation& o, uint32_t tick, Monkey target) {
    if (target) {
      seen = target.pos();
      seenTick = tick;
      haveSeen = true;
      return;
    }
    for (kfm::Call c : o.calls()) {
      if (o.relation(c.slot()) != KFM_REL_ALLY || !grid.ready()) continue;
      if (haveSeen && c.tick() <= seenTick) continue;
      seen = grid.centre(Cell{(c.symbol() & 15) * 2 + 1, (c.symbol() >> 4) * 2 + 1});
      seenTick = c.tick();
      haveSeen = true;
    }
    if (haveSeen && tick - seenTick > kMemory) haveSeen = false;
  }

  // Continuous dodge: run across the banana's line, away from it; dash when
  // it is close.
  bool dodge(const Observation& o, Banana b, int hitIn, Input& in) {
    const Self me = o.self();
    const Vec2 off = me.pos() - b.pos();
    const int64_t side = int64_t{b.vel().x.raw} * off.y.raw - int64_t{b.vel().y.raw} * off.x.raw;
    in.move(steer(o, static_cast<uint8_t>(b.vel().angle8() + (side >= 0 ? 64 : 192))));
    if (hitIn <= 6 && me.ability(kfm::kSlotDash).ready())
      in.press(me.ability(kfm::kSlotDash).button());
    return true;
  }

  // Grid dodge: the first neighbour cell (sideways first) the banana's path
  // misses; else a dash to a cell it misses; else stand and hope.
  bool dodgeOnGrid(const Observation& o, Banana b, const kfm::Path& path, Input& in) {
    const Self me = o.self();
    const Cell here = me.cell();
    const Fixed reach = me.radius() + b.radius() + kMargin;
    const int across = kfm::octantOf(b.vel().angle8()) + 2;
    for (int i = 0; i < 8; ++i) {
      const int oct = (across + (i & 1) * 4 + (i >> 1)) & 7;  // +2, -2, +3, -1, ...
      if (grid.canStep(here, oct) &&
          path.firstContact(grid.centre(kfm::neighbour(here, oct)), reach) < 0) {
        in.move(kfm::octantDir(oct));
        return true;
      }
    }
    const kfm::Ability dash = me.ability(kfm::kSlotDash);
    for (int oct = 0; dash.ready() && oct < 8; ++oct) {
      const Cell land = grid.dashLanding(here, oct, grid.dashCells(dash));
      if (land != here && path.firstContact(grid.centre(land), reach) < 0) {
        in.move(kfm::octantDir(oct)).press(dash.button());
        return true;
      }
    }
    return false;
  }

  // Continuous movement: fetch a banana, else circle the target at range.
  void move(const Observation& o, Monkey target, Input& in) {
    const Self me = o.self();
    if (me.ammo() == 0) {
      const Banana mine = kfm::myReturningBanana(o);
      const kfm::Pickup p = kfm::nearestBananaPickup(o, me.pos());
      if (mine) return void(in.move(kfm::dirTo(me.pos(), mine.pos())));
      if (p) return void(in.move(kfm::dirTo(me.pos(), p.pos())));
    }
    if (!target) return;
    const Fixed d = kfm::dist(me.pos(), target.pos());
    const uint8_t toward = kfm::dirTo(me.pos(), target.pos());
    uint8_t dir = static_cast<uint8_t>(toward + orbit);  // circle it
    if (me.ammo() == 0)
      dir = static_cast<uint8_t>(toward + 128);  // unarmed: away
    else if (d > kFar)
      dir = toward;  // too far
    else if (d < kNear)
      dir = static_cast<uint8_t>(toward + 128 + (orbit == 64 ? -32 : 32));
    if ((kfm_rng_next(&rng) & 255u) == 0) orbit = static_cast<uint8_t>(256 - orbit);
    in.move(steer(o, dir));
  }

  // Bends a continuous stick direction off the outer walls and away from pits.
  uint8_t steer(const Observation& o, uint8_t dir) {
    const Self me = o.self();
    const Vec2 half = o.halfExtents() - Vec2{kWallMargin, kWallMargin};
    Vec2 push{};
    if (me.pos().x > half.x) push.x = -Fixed::one();
    if (me.pos().x < -half.x) push.x = Fixed::one();
    if (me.pos().y > half.y) push.y = -Fixed::one();
    if (me.pos().y < -half.y) push.y = Fixed::one();
    if (!push.zero()) dir = (Vec2::fromAngle8(dir, Fixed::one()) + push).angle8();
    for (int i = 0; i < 8; ++i, dir = static_cast<uint8_t>(dir + 32)) {
      if (!kfm::inPit(o, me.pos() + Vec2::fromAngle8(dir, me.moveSpeed() * 12 + me.radius())))
        break;
    }
    return dir;
  }

  // Grid movement: walk (around walls) to a banana, to throwing distance of
  // the target, to the last sighting, or to a random cell; look where it goes.
  void moveOnGrid(const Observation& o, uint32_t tick, Monkey target, Input& in) {
    const Self me = o.self();
    const Cell here = me.cell();
    Cell goal{};
    if (me.ammo() == 0) {
      if (const kfm::Pickup p = kfm::nearestBananaPickup(o, me.pos())) goal = grid.cellOf(p.pos());
    }
    // Hill (the contest since engine v0.7.0): armed, walk to the hill and
    // hold it, standing to aim at whoever comes; the dodge and the throw
    // still run around this.
    const kfm::Maybe<kfm::Zone> hill = o.hill();
    if (!goal.valid() && hill && o.modeKind() == KFM_MODE_HILL && me.ammo() > 0) {
      if (hill.value.contains(me.pos())) {
        if (target) in.aim(kfm::dirTo(me.pos(), target.pos()));
        return;
      }
      goal = grid.cellOf(hill.value.centre());
    }
    if (!goal.valid() && target) {
      const Cell t = grid.cellOf(target.pos());
      const int d = kfm::chebyshev(here, t);
      in.aim(kfm::dirTo(me.pos(), target.pos()));
      if (d < kGridKeep || me.ammo() == 0) {  // too close, or unarmed: step away
        int best = -1;
        for (int oct = 0; oct < 8; ++oct) {
          const Cell n = kfm::neighbour(here, oct);
          if (grid.canStep(here, oct) && kfm::chebyshev(n, t) > d &&
              (best < 0 || !kfm::diagonal(oct)))
            best = oct;
        }
        if (best >= 0) in.move(kfm::octantDir(best));
        return;
      }
      if (d <= kGridThrow &&
          kfm::lineClear(o, me.pos(), target.pos(), Fixed{o.rules().bananaRadius}))
        return;
      goal = t;
    }
    if (!goal.valid() && haveSeen) {
      goal = grid.cellOf(seen);
      if (goal == here) haveSeen = false;
    }
    if (!goal.valid()) {
      if (!wander.valid() || wander == here || (tick & 255u) == 0) {
        wander = Cell{static_cast<int>(kfm_rng_below(&rng, grid.cols())),
                      static_cast<int>(kfm_rng_below(&rng, grid.rows()))};
      }
      goal = wander;
    }
    // Walk straight while the straight step is open; search round walls
    // when it is not (the search costs fuel in proportion to the distance).
    int step = kfm::octantToward(here, goal);
    if (step < 0 || !grid.canStep(here, step)) step = grid.stepToward(here, goal, 40);
    if (step >= 0) {
      in.move(kfm::octantDir(step));
      if (!target) in.aim(kfm::octantDir(step));  // the cone looks ahead
    } else if (!target && !grid.open(goal)) {
      wander = Cell{};  // an unreachable goal: pick another
    }
  }

  // Throw at the target, leading it, when it is in range and the line is clear.
  void throwAt(const Observation& o, Monkey target, Input& in) {
    const Self me = o.self();
    const kfm::Ability thr = me.ability(kfm::kSlotThrow);
    if (!target || target.guarded() || me.ammo() == 0 || !thr.is(KFM_ABILITY_THROW) || !thr.ready())
      return;
    if (in.pressed(me.ability(kfm::kSlotDash).button())) return;
    Fixed s = speed;
    if (s == Fixed{} && o.grid()) s = kfm::observedBananaSpeed(o);
    if (s == Fixed{}) s = kfm::tapThrowSpeed(o);
    const int wait = o.inputDelay() + thr.startupTicks();
    const Vec2 from = me.pos() + me.vel() * wait;
    const Vec2 at = kfm::leadPoint(o, from, target.pos(), target.vel(), s, wait);
    const bool inRange = o.grid() ? kfm::chebyshev(grid.cellOf(from), grid.cellOf(at)) <= kGridThrow
                                  : kfm::dist(from, at) <= kfm::throwRange(o, s) - Fixed::half();
    if (!inRange || !kfm::lineClear(o, from, at, Fixed{o.rules().bananaRadius})) return;
    in.aim(kfm::dirTo(from, at)).press(thr.button());
    if (o.grid()) in.stop();  // stand to throw: the next turn is rooted anyway
  }

  // Grid: call out the target's coarse cell to everyone in range (allies and
  // enemies alike hear it), now and then, when not throwing.
  void callOut(const Observation& o, uint32_t tick, Monkey target, Input& in) {
    const int slot = o.self().slotOf(KFM_ABILITY_CALL);
    if (!target || !grid.ready() || slot < 0 || in.buttons != 0 || tick - lastCall < kCallEvery)
      return;
    const kfm::Ability call = o.self().ability(slot);
    if (!call.ready()) return;
    const Cell t = grid.cellOf(target.pos());
    in.aim(static_cast<uint8_t>((t.col / 2) | ((t.row / 2) << 4))).press(call.button());
    lastCall = tick;
  }
};

KFM_BOT(Starter)
