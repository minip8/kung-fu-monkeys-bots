/* kfm_bot.hpp: the C++ layer of the Kung Fu Monkeys bot SDK (bot ABI and
 * Observation v1, docs/bot-abi.md of the game repository).
 *
 * One header over sdk/c/kfm_bot.h, which it includes by the relative path
 * "../c/kfm_bot.h": keep sdk/c and sdk/cpp side by side and put sdk/cpp on
 * the include path (-I sdk/cpp). Freestanding like the C header: no standard
 * library, no exceptions, no RTTI, no heap, no float; everything is integer
 * arithmetic, so a bot behaves identically on every host.
 *
 * Use, in exactly one source file of the bot:
 *
 *   #define KFM_BOT_MAIN          // the C header's exports, kfm_obs, memcpy/memset
 *   #include "kfm_bot.hpp"
 *
 *   struct MyBot {
 *     void init(uint64_t seed, uint32_t slot) { ... }
 *     kfm::Input act(uint32_t tick, const kfm::Observation& o) { ... }
 *   };
 *   KFM_BOT(MyBot)                // defines kfm_init and kfm_act
 *
 * The bot type must be constant-initialisable (default member initialisers
 * are fine, a constructor that runs code is not; KFM_BOT checks it with
 * constinit). The same goes for every global of a bot: wasm-ld links a
 * module without a C runtime by running the global constructors at the
 * start of every exported call, so a global whose constructor runs code is
 * built again on every kfm_act and loses its state each tick. kfm_init
 * resets the instance to MyBot{} before it calls init. A trap or more than
 * the fuel budget (100 000 wasm instructions by default) costs the tick: the
 * host repeats the last input. Nothing here allocates; do not use new,
 * virtual functions with no body (= 0), exceptions or the standard library:
 * each becomes an import the host refuses.
 *
 * Exactness: Fixed and Vec2 are the C header's kfm_fx/kfm_vec arithmetic (the
 * sim's Fixed, bit for bit); predict() runs the C header's exact one-tick
 * banana flight step, so a prediction is exact while nothing touches the
 * banana and its owner keeps its velocity (catches, hits, parries and the
 * owner's turns are not foreseen). The grid helpers follow the sim's latch
 * rules (architecture §5.12); dashCatchDir on the grid models the dash as a
 * slide of whole cells, as the sim commits it.
 */
#ifndef KFM_BOT_HPP
#define KFM_BOT_HPP

#include "../c/kfm_bot.h"

namespace kfm {

/* ---- Fixed: Q16.16, the sim's fixed point ---------------------------------- */

struct Fixed {
  int32_t raw = 0;

  static constexpr Fixed fromRaw(int32_t r) { return Fixed{r}; }
  static constexpr Fixed fromInt(int32_t i) {
    return Fixed{static_cast<int32_t>(static_cast<uint32_t>(i) << 16)};
  }
  // num / den rounded toward zero (kfm_fx_ratio); 0 when den is 0.
  static constexpr Fixed ratio(int32_t num, int32_t den) {
    return Fixed{den == 0 ? 0 : static_cast<int32_t>((static_cast<int64_t>(num) * 65536) / den)};
  }
  static constexpr Fixed one() { return Fixed{KFM_FX_ONE}; }
  static constexpr Fixed half() { return Fixed{KFM_FX_HALF}; }
  // The largest integer not above the value.
  constexpr int32_t floor() const { return raw >> 16; }
  // The nearest integer (halves round up).
  constexpr int32_t round() const { return (raw + KFM_FX_HALF) >> 16; }
};

// Arithmetic: + and - and the integer * and / are exact; Fixed * Fixed and
// Fixed / Fixed are the sim's (truncating, int64 intermediates).
constexpr Fixed operator+(Fixed a, Fixed b) { return Fixed{a.raw + b.raw}; }
constexpr Fixed operator-(Fixed a, Fixed b) { return Fixed{a.raw - b.raw}; }
constexpr Fixed operator-(Fixed a) { return Fixed{-a.raw}; }
constexpr Fixed operator*(Fixed a, int32_t k) { return Fixed{a.raw * k}; }
constexpr Fixed operator/(Fixed a, int32_t k) { return Fixed{k == 0 ? 0 : a.raw / k}; }
inline Fixed operator*(Fixed a, Fixed b) { return Fixed{kfm_fx_mul(a.raw, b.raw)}; }
// Rounded toward zero; division by zero saturates (the sim's operator/).
inline Fixed operator/(Fixed a, Fixed b) { return Fixed{kfm_fx_div(a.raw, b.raw)}; }
constexpr Fixed& operator+=(Fixed& a, Fixed b) { return a = a + b; }
constexpr Fixed& operator-=(Fixed& a, Fixed b) { return a = a - b; }
constexpr bool operator==(Fixed a, Fixed b) { return a.raw == b.raw; }
constexpr bool operator!=(Fixed a, Fixed b) { return a.raw != b.raw; }
constexpr bool operator<(Fixed a, Fixed b) { return a.raw < b.raw; }
constexpr bool operator<=(Fixed a, Fixed b) { return a.raw <= b.raw; }
constexpr bool operator>(Fixed a, Fixed b) { return a.raw > b.raw; }
constexpr bool operator>=(Fixed a, Fixed b) { return a.raw >= b.raw; }
constexpr Fixed abs(Fixed a) { return a.raw < 0 ? -a : a; }
constexpr Fixed min(Fixed a, Fixed b) { return a < b ? a : b; }
constexpr Fixed max(Fixed a, Fixed b) { return a > b ? a : b; }
constexpr Fixed clamp(Fixed v, Fixed lo, Fixed hi) { return min(max(v, lo), hi); }
// floor(sqrt(a)); 0 for a <= 0.
inline Fixed sqrt(Fixed a) { return Fixed{kfm_fx_sqrt(a.raw)}; }

/* ---- Vec2 ------------------------------------------------------------------ */

struct Vec2 {
  Fixed x;
  Fixed y;

  static constexpr Vec2 fromRaw(int32_t x, int32_t y) { return Vec2{Fixed{x}, Fixed{y}}; }
  static constexpr Vec2 of(kfm_vec v) { return fromRaw(v.x, v.y); }
  // `mag` along an angle8 (a stick or aim byte), with the engine's sine table.
  static Vec2 fromAngle8(uint8_t angle8, Fixed mag) {
    return of(kfm_angle_to_vec(angle8, mag.raw));
  }
  constexpr kfm_vec c() const { return kfm_vec{x.raw, y.raw}; }
  constexpr bool zero() const { return x.raw == 0 && y.raw == 0; }
  // |v|^2 in raw Q32.32, exact (compare with sq() of a length).
  uint64_t lenSq() const { return kfm_vec_len_sq(c()); }
  // floor(|v|).
  Fixed len() const { return Fixed{kfm_vec_len(c())}; }
  // v / |v|; the zero vector stays zero.
  Vec2 normalized() const { return of(kfm_vec_normalized(c())); }
  // The nearest angle8 of the heading (0 = +x, counter-clockwise); 0 for zero.
  uint8_t angle8() const { return kfm_angle8_of(c()); }
};

// Componentwise arithmetic; Vec2 * Fixed is the sim's scale.
constexpr Vec2 operator+(Vec2 a, Vec2 b) { return Vec2{a.x + b.x, a.y + b.y}; }
constexpr Vec2 operator-(Vec2 a, Vec2 b) { return Vec2{a.x - b.x, a.y - b.y}; }
constexpr Vec2 operator-(Vec2 a) { return Vec2{-a.x, -a.y}; }
constexpr Vec2 operator*(Vec2 a, int32_t k) { return Vec2{a.x * k, a.y * k}; }
inline Vec2 operator*(Vec2 a, Fixed s) { return Vec2{a.x * s, a.y * s}; }
constexpr Vec2& operator+=(Vec2& a, Vec2 b) { return a = a + b; }
constexpr bool operator==(Vec2 a, Vec2 b) { return a.x == b.x && a.y == b.y; }
constexpr bool operator!=(Vec2 a, Vec2 b) { return !(a == b); }
// The dot product (each product truncated, as the sim's).
inline Fixed dot(Vec2 a, Vec2 b) { return Fixed{kfm_vec_dot(a.c(), b.c())}; }
// floor(|b - a|).
inline Fixed dist(Vec2 a, Vec2 b) { return (b - a).len(); }
// |b - a|^2 in raw Q32.32.
inline uint64_t distSq(Vec2 a, Vec2 b) { return (b - a).lenSq(); }
// r^2 in raw Q32.32, the unit of lenSq and distSq.
inline uint64_t sq(Fixed r) { return kfm_fx_sq(r.raw); }
// The angle8 pointing from `from` toward `to`.
inline uint8_t dirTo(Vec2 from, Vec2 to) { return (to - from).angle8(); }
// The angle8 pointing from `to` away from it, through `from`.
inline uint8_t dirAway(Vec2 from, Vec2 to) { return static_cast<uint8_t>(dirTo(from, to) + 128); }

/* ---- small utilities --------------------------------------------------------- */

// A value or nothing (no <optional> in a freestanding module).
template <class T>
struct Maybe {
  bool has = false;
  T value{};
  explicit operator bool() const { return has; }
};
template <class T>
Maybe<T> just(T v) {
  return Maybe<T>{true, v};
}

// A pointer and a count, for range-for over a fixed array.
template <class T>
struct Span {
  const T* first;
  int count;
  const T* begin() const { return first; }
  const T* end() const { return first + count; }
  const T& operator[](int i) const { return first[i]; }
};

// Range-for over the entries of a fixed array that View::present accepts,
// yielding View objects (their index is the array index).
template <class View, class Raw, int N>
class Each {
 public:
  explicit Each(const Raw* base) : base_(base) {}
  struct Iter {
    const Raw* base;
    int i;
    View operator*() const { return View(base + i, i); }
    Iter& operator++() {
      i = next(base, i + 1);
      return *this;
    }
    bool operator!=(const Iter& o) const { return i != o.i; }
  };
  Iter begin() const { return Iter{base_, next(base_, 0)}; }
  Iter end() const { return Iter{base_, N}; }

 private:
  static int next(const Raw* b, int i) {
    while (i < N && !View::present(b[i]))
      ++i;
    return i;
  }
  const Raw* base_;
};

/* ---- the observation, as views ------------------------------------------------ */

// Ability slots: which button fires each (bot-abi.md §1.2).
inline constexpr int kSlotDash = 0;   // fires on KFM_BTN_DASH
inline constexpr int kSlotThrow = 1;  // fires on KFM_BTN_THROW
inline constexpr int kSlotExtra = 2;  // fires on KFM_BTN_ABILITY1 (a call in the contest kits)

// One ability slot of the own monkey.
class Ability {
 public:
  Ability(const kfm_obs_ability* a, uint8_t lastButtons) : a_(a), last_(lastButtons) {}
  int kind() const { return a_->kind; }  // KFM_ABILITY_*
  bool is(int kind) const { return a_->kind == kind; }
  uint8_t button() const { return a_->button; }  // the KFM_BTN_* bit that fires it
  int phase() const { return a_->phase; }        // KFM_ABILITY_IDLE..RECOVERY
  int cooldown() const { return a_->cooldownTicks; }
  // A press in this tick's answer fires it: it has a kind, is Idle with no
  // cooldown, and its button was up on the last applied tick.
  bool ready() const {
    return a_->kind != KFM_ABILITY_NONE && a_->phase == KFM_ABILITY_IDLE &&
           a_->cooldownTicks == 0 && (last_ & a_->button) == 0;
  }
  int startupTicks() const { return a_->defStartupTicks; }
  int activeTicks() const { return a_->defActiveTicks; }
  int recoveryTicks() const { return a_->defRecoveryTicks; }
  int cooldownTicks() const { return a_->defCooldownTicks; }
  // Dash: a banana met within this many ticks of the press is caught (0: none).
  int perfectWindow() const { return a_->defPerfectWindowTicks; }
  Fixed speed() const { return Fixed{a_->defSpeed}; }    // per tick; see bot-abi.md §2.6
  Fixed param0() const { return Fixed{a_->defParam0}; }  // slam reach, tap charge, call range
  Fixed param1() const { return Fixed{a_->defParam1}; }
  const kfm_obs_ability& raw() const { return *a_; }

 private:
  const kfm_obs_ability* a_;
  uint8_t last_;
};

// Grid coordinates: column 0 at the west edge, row 0 at the north (+y) edge.
struct Cell {
  int col = -1;
  int row = -1;
  bool valid() const { return col >= 0 && row >= 0; }
};
constexpr bool operator==(Cell a, Cell b) { return a.col == b.col && a.row == b.row; }
constexpr bool operator!=(Cell a, Cell b) { return !(a == b); }

// The own monkey. Everything but slot() is zero while it is not alive.
class Self {
 public:
  Self(const kfm_obs_self* s, bool grid) : s_(s), grid_(grid) {}
  bool alive() const { return s_->alive != 0; }
  int slot() const { return s_->slot; }
  int team() const { return s_->team; }
  int role() const { return s_->role; }  // the loadout index this slot plays
  Vec2 pos() const { return Vec2::fromRaw(s_->posX, s_->posY); }
  Vec2 vel() const { return Vec2::fromRaw(s_->velX, s_->velY); }  // per tick, last tick's
  Fixed radius() const { return Fixed{s_->radius}; }
  Fixed moveSpeed() const { return Fixed{s_->moveSpeed}; }  // per tick
  int hp() const { return s_->hp; }
  int ammo() const { return s_->ammo; }
  int ammoMax() const { return s_->ammoMax; }
  uint8_t facing() const { return s_->facing; }
  bool has(int tag) const { return kfm_has_tag(s_->tags, tag) != 0; }  // KFM_TAG_*
  // Grid mode: the own cell; Cell{} in a continuous mode or while dead.
  Cell cell() const { return grid_ && alive() ? Cell{s_->cellX, s_->cellY} : Cell{}; }
  Ability ability(int slot) const { return Ability(&s_->abilities[slot], s_->lastButtons); }
  // The slot running an ability of `kind` (KFM_ABILITY_*), or -1.
  int slotOf(int kind) const {
    for (int i = 0; i < KFM_ABILITY_SLOTS; ++i)
      if (s_->abilities[i].kind == kind) return i;
    return -1;
  }
  const kfm_obs_self& raw() const { return *s_; }

 private:
  const kfm_obs_self* s_;
  bool grid_;
};

// A monkey in the arena (and, under fog, seen this turn); self included.
// A default-constructed Monkey is "none" and tests false.
class Monkey {
 public:
  Monkey() = default;
  Monkey(const kfm_obs_player* p, int slot) : p_(p), slot_(slot) {}
  static bool present(const kfm_obs_player& p) { return p.alive != 0; }
  explicit operator bool() const { return p_ != nullptr; }
  int slot() const { return slot_; }
  int team() const { return p_->team; }
  bool isSelf() const { return p_->relation == KFM_REL_SELF; }
  bool isAlly() const { return p_->relation == KFM_REL_ALLY; }
  bool isEnemy() const { return p_->relation == KFM_REL_ENEMY; }
  Vec2 pos() const { return Vec2::fromRaw(p_->posX, p_->posY); }
  Vec2 vel() const { return Vec2::fromRaw(p_->velX, p_->velY); }
  int hp() const { return p_->hp; }
  int ammo() const { return p_->ammo; }
  uint8_t facing() const { return p_->facing; }
  bool dashing() const { return p_->dashing != 0; }
  bool has(int tag) const { return kfm_has_tag(p_->tags, tag) != 0; }
  // Spawn protection or a shield: a banana or a dash does it no harm now.
  bool guarded() const { return has(KFM_TAG_INVULNERABLE) || has(KFM_TAG_SHIELDED); }
  const kfm_obs_player& raw() const { return *p_; }

 private:
  const kfm_obs_player* p_ = nullptr;
  int slot_ = -1;
};

// A banana in the pool (in flight, grounded or sinking; under fog, seen).
class Banana {
 public:
  Banana() = default;
  Banana(const kfm_obs_banana* b, int index) : b_(b), index_(index) {}
  static bool present(const kfm_obs_banana& b) { return b.valid != 0; }
  explicit operator bool() const { return b_ != nullptr; }
  int index() const { return index_; }  // its pool slot, the index predict() takes
  Vec2 pos() const { return Vec2::fromRaw(b_->posX, b_->posY); }
  Vec2 vel() const { return Vec2::fromRaw(b_->velX, b_->velY); }
  Fixed speed() const { return Fixed{b_->speed}; }
  Fixed radius() const { return Fixed{b_->radius}; }
  int phase() const { return b_->phase; }  // KFM_BANANA_*
  bool inFlight() const { return b_->phase < KFM_BANANA_GROUNDED; }
  int owner() const { return b_->owner; }
  bool mine() const { return b_->ownerRelation == KFM_REL_SELF; }
  bool allys() const { return b_->ownerRelation == KFM_REL_ALLY; }
  // Ticks to its closest approach to self on its current velocity (a cue:
  // a returning banana curves; predict() is exact).
  int approachTicks() const { return b_->approachTicks; }
  Fixed approachDistance() const { return Fixed{b_->approachDistance}; }
  const kfm_obs_banana& raw() const { return *b_; }

 private:
  const kfm_obs_banana* b_ = nullptr;
  int index_ = -1;
};

// A pickup: a banana lying around, a power-up or a capture flag.
class Pickup {
 public:
  Pickup() = default;
  Pickup(const kfm_obs_pickup* p, int index) : p_(p), index_(index) {}
  static bool present(const kfm_obs_pickup& p) { return p.valid != 0; }
  explicit operator bool() const { return p_ != nullptr; }
  int index() const { return index_; }
  int kind() const { return p_->kind; }  // KFM_PICKUP_*
  bool isBanana() const { return p_->kind == KFM_PICKUP_BANANA; }
  Vec2 pos() const { return Vec2::fromRaw(p_->posX, p_->posY); }
  Fixed radius() const { return Fixed{p_->radius}; }
  int flagTeam() const { return p_->spawner; }  // a Flag's team
  const kfm_obs_pickup& raw() const { return *p_; }

 private:
  const kfm_obs_pickup* p_ = nullptr;
  int index_ = -1;
};

// A call heard this turn: the caller's slot, its symbol (aim byte), the tick
// of the call and the caller's cell then (Cell{} when not seen).
class Call {
 public:
  Call(const kfm_obs_heard_call* c, int index) : c_(c), index_(index) {}
  static bool present(const kfm_obs_heard_call& c) { return c.valid != 0; }
  int index() const { return index_; }
  int slot() const { return c_->slot; }
  uint8_t symbol() const { return c_->symbol; }
  uint32_t tick() const { return c_->tick; }
  Cell cell() const { return c_->cellX < 0 ? Cell{} : Cell{c_->cellX, c_->cellY}; }

 private:
  const kfm_obs_heard_call* c_;
  int index_;
};

// An objective zone of the arena (engine v0.7.0, bot-abi.md §3): a closed
// box; a monkey whose centre is inside stands in it. The hill is zone 0.
struct Zone {
  Vec2 min;
  Vec2 max;
  bool contains(Vec2 p) const {
    return p.x >= min.x && p.x <= max.x && p.y >= min.y && p.y <= max.y;
  }
  Vec2 centre() const {
    return Vec2::fromRaw((min.x.raw + max.x.raw) / 2, (min.y.raw + max.y.raw) / 2);
  }
};

// The observation the host wrote for this tick. A view: it never copies the
// 5748-byte buffer.
class Observation {
 public:
  explicit Observation(const kfm_observation& o) : o_(o) {}
  const kfm_observation& raw() const { return o_; }
  bool ok() const { return kfm_obs_ok(&o_) != 0; }  // version and size match this SDK

  uint32_t tick() const { return o_.match.tick; }
  int phase() const { return o_.match.phase; }  // KFM_PHASE_*
  bool playing() const { return o_.match.phase == KFM_PHASE_PLAYING; }
  int modeKind() const { return o_.match.modeKind; }  // KFM_MODE_*
  uint32_t ticksInPhase() const { return o_.match.ticksInPhase; }
  int inputDelay() const { return o_.match.inputDelayTicks; }  // answer applies this much later
  // Under fog, unseen monkeys, bananas and pickups are zeroed (bot-abi.md §3.1).
  bool fog() const { return (o_.header.schemaFlags & KFM_SCHEMA_FLAG_FOG) != 0; }
  // A grid mode: monkeys move a cell per turn (bot-abi.md §3, architecture §5.12).
  bool grid() const { return o_.match.turnTicks > 0; }
  int turnTicks() const { return o_.match.turnTicks; }
  int ticksToLatch() const { return o_.match.ticksToLatch; }  // 0: this answer is latched
  bool alive(int slot) const { return ((o_.match.aliveMask >> slot) & 1u) != 0; }  // seen or not
  // The relation of `slot` to self (KFM_REL_*), kept for unseen monkeys too.
  int relation(int slot) const {
    return slot >= 0 && slot < KFM_MAX_PLAYERS ? o_.players[slot].relation : int{KFM_REL_NONE};
  }

  Self self() const { return Self(&o_.self, o_.match.turnTicks > 0); }
  // Monkeys in the arena (self included); under fog only the seen ones.
  Each<Monkey, kfm_obs_player, KFM_MAX_PLAYERS> monkeys() const {
    return Each<Monkey, kfm_obs_player, KFM_MAX_PLAYERS>(o_.players);
  }
  Each<Banana, kfm_obs_banana, KFM_MAX_BANANAS> bananas() const {
    return Each<Banana, kfm_obs_banana, KFM_MAX_BANANAS>(o_.bananas);
  }
  Each<Pickup, kfm_obs_pickup, KFM_MAX_PICKUPS> pickups() const {
    return Each<Pickup, kfm_obs_pickup, KFM_MAX_PICKUPS>(o_.pickups);
  }
  Each<Call, kfm_obs_heard_call, KFM_MAX_HEARD_CALLS> calls() const {
    return Each<Call, kfm_obs_heard_call, KFM_MAX_HEARD_CALLS>(o_.heardCalls);
  }
  // The monkey in `slot` when it is in the arena (and seen), else none.
  Monkey monkey(int slot) const {
    if (slot < 0 || slot >= KFM_MAX_PLAYERS || o_.players[slot].alive == 0) return Monkey();
    return Monkey(&o_.players[slot], slot);
  }
  // Banana `index` of the pool when valid, else none.
  Banana banana(int index) const {
    if (index < 0 || index >= KFM_MAX_BANANAS || o_.bananas[index].valid == 0) return Banana();
    return Banana(&o_.bananas[index], index);
  }

  const kfm_obs_rules& rules() const { return o_.rules; }
  const kfm_obs_arena& arena() const { return o_.arena; }
  Vec2 halfExtents() const { return Vec2::fromRaw(o_.arena.halfExtents.x, o_.arena.halfExtents.y); }
  Span<kfm_obs_box> walls() const { return Span<kfm_obs_box>{o_.arena.walls, o_.arena.wallCount}; }
  Span<kfm_obs_hazard> hazards() const {
    return Span<kfm_obs_hazard>{o_.arena.hazards, o_.arena.hazardCount};
  }
  // The arena's zone k (k < KFM_ZONES), none when the arena has no zone k or
  // the host predates engine v0.7.0 (the words were 0). Zone 0 is the hill, a
  // capture team t's home is zone t.
  Maybe<Zone> zone(int k) const {
    if (k < 0 || k >= KFM_ZONES) return Maybe<Zone>{};
    const uint32_t* w = &o_.match.modeData[KFM_ZONE_WORD + (4 * k)];
    if ((w[0] | w[1] | w[2] | w[3]) == 0) return Maybe<Zone>{};
    return just(Zone{Vec2::fromRaw(static_cast<int32_t>(w[0]), static_cast<int32_t>(w[1])),
                     Vec2::fromRaw(static_cast<int32_t>(w[2]), static_cast<int32_t>(w[3]))});
  }
  Maybe<Zone> hill() const { return zone(0); }
  // Hill mode: the team holding the hill alone, -1 when nobody or it is contested.
  int hillHolder() const {
    return o_.match.modeKind == KFM_MODE_HILL ? static_cast<int>(o_.match.modeData[5]) - 1 : -1;
  }

 private:
  const kfm_observation& o_;
};

/* ---- the action ---------------------------------------------------------------- */

// The answer to an observation: the five PlayerInput bytes, built fluently:
//   return kfm::Input().move(dir).aim(at).press(KFM_BTN_THROW);
// A button fires on a press edge only: release it for a tick to press again.
struct Input {
  uint8_t moveDir = 0;  // angle8 of the stick
  uint8_t moveMag = 0;  // 0 idle, 255 full
  uint8_t aimDir = 0;   // angle8: the facing, and where a throw goes
  uint8_t buttons = 0;  // KFM_BTN_* bits
  uint8_t subTick = 0;  // 1/256 tick of the latest press

  Input& move(uint8_t dir, uint8_t mag = 255) {
    moveDir = dir;
    moveMag = mag;
    return *this;
  }
  Input& stop() {
    moveMag = 0;
    return *this;
  }
  Input& aim(uint8_t dir) {
    aimDir = dir;
    return *this;
  }
  Input& press(uint8_t bits) {
    buttons = static_cast<uint8_t>(buttons | bits);
    return *this;
  }
  Input& release(uint8_t bits) {
    buttons = static_cast<uint8_t>(buttons & ~bits);
    return *this;
  }
  Input& at(uint8_t sub) {
    subTick = sub;
    return *this;
  }
  bool pressed(uint8_t bits) const { return (buttons & bits) != 0; }
  uint64_t pack() const { return kfm_pack_input(moveDir, moveMag, aimDir, buttons, subTick); }
};

/* ---- finding things ------------------------------------------------------------- */

// The nearest enemy monkey (seen, under fog), preferring one without spawn
// protection; ties go to the lower slot. None when there is none.
inline Monkey nearestEnemy(const Observation& o) { return o.monkey(kfm_nearest_enemy(&o.raw())); }

// Whether banana b can hurt the own monkey: in flight, not its own, and not a
// teammate's unless friendly fire is on for this monkey.
inline bool isThreat(const Observation& o, Banana b) {
  if (!b || !b.inFlight() || b.mine()) return false;
  return !b.allys() || o.self().has(KFM_TAG_FRIENDLY_FIRE);
}

// The nearest banana lying on the ground (a banana pickup) to `from`; none
// when there is none (or none is seen).
inline Pickup nearestBananaPickup(const Observation& o, Vec2 from) {
  Pickup best;
  uint64_t bestD = ~uint64_t{0};
  for (Pickup p : o.pickups()) {
    const uint64_t d = distSq(from, p.pos());
    if (p.isBanana() && d < bestD) {
      best = p;
      bestD = d;
    }
  }
  return best;
}

// The own banana on its way back, if any (walk into it to catch it).
inline Banana myReturningBanana(const Observation& o) {
  for (Banana b : o.bananas())
    if (b.mine() && b.phase() == KFM_BANANA_RETURNING) return b;
  return Banana();
}

// The speed of a banana in flight this tick, the own one first, else 0. The
// observation carries only the base speed (rules.bananaSpeed); a mode's
// baseline or a charge scales real throws, so learn it from what flies.
inline Fixed observedBananaSpeed(const Observation& o) {
  Fixed any;
  for (Banana b : o.bananas()) {
    if (!b.inFlight() || b.speed() <= Fixed{}) continue;
    if (b.mine()) return b.speed();
    if (any == Fixed{}) any = b.speed();
  }
  return any;
}

// The speed of a tapped throw from the own throw slot by the rules alone:
// the base speed times the tap multiplier of a charging throw. Power-ups and
// a mode's baseline are not in it (observedBananaSpeed sees them).
inline Fixed tapThrowSpeed(const Observation& o) {
  const Ability t = o.self().ability(kSlotThrow);
  const Fixed base = Fixed{o.rules().bananaSpeed};
  return t.speed() > Fixed{} ? base * t.param0() : base;
}

// How far a banana thrown at `speed` (per tick) flies before it turns home.
inline Fixed throwRange(const Observation& o, Fixed speed) {
  return speed * static_cast<int32_t>(o.rules().bananaOutboundTicks) +
         Fixed{o.rules().bananaSpawnOffset};
}

// Where to aim from `from` so a banana at `speed` meets a monkey at `pos`
// moving at `vel`, when the banana leaves `waitTicks` from now (input delay
// plus the throw's startup): three iterations on the flight time.
inline Vec2 leadPoint(const Observation& o, Vec2 from, Vec2 pos, Vec2 vel, Fixed speed,
                      int waitTicks) {
  const Fixed offset = Fixed{o.rules().bananaSpawnOffset};
  Vec2 at = pos + vel * waitTicks;
  for (int i = 0; i < 3 && speed > Fixed{}; ++i) {
    const Fixed flight = max(dist(from, at) - offset, Fixed{}) / speed;
    at = pos + vel * (flight + Fixed::fromInt(waitTicks));
  }
  return at;
}

// Whether a banana of `radius` flies straight from `from` to `to` without
// touching an interior wall (each wall box grown by the radius).
inline bool lineClear(const Observation& o, Vec2 from, Vec2 to, Fixed radius) {
  const int64_t p[2] = {from.x.raw, from.y.raw};
  const int64_t v[2] = {int64_t{to.x.raw} - from.x.raw, int64_t{to.y.raw} - from.y.raw};
  for (const kfm_obs_box& w : o.walls()) {
    // Slab test of from + t * (to - from), t in [0, 1] (Q16.16 in int64, so a
    // short segment's large quotients cannot overflow), against the grown box.
    const int64_t lo[2] = {int64_t{w.minX} - radius.raw, int64_t{w.minY} - radius.raw};
    const int64_t hi[2] = {int64_t{w.maxX} + radius.raw, int64_t{w.maxY} + radius.raw};
    int64_t enter = 0;
    int64_t leave = KFM_FX_ONE;
    bool miss = false;
    for (int a = 0; a < 2 && !miss; ++a) {
      if (v[a] == 0) {
        miss = p[a] < lo[a] || p[a] > hi[a];
        continue;
      }
      int64_t t0 = ((lo[a] - p[a]) * KFM_FX_ONE) / v[a];
      int64_t t1 = ((hi[a] - p[a]) * KFM_FX_ONE) / v[a];
      if (t0 > t1) {
        const int64_t t = t0;
        t0 = t1;
        t1 = t;
      }
      enter = t0 > enter ? t0 : enter;
      leave = t1 < leave ? t1 : leave;
      miss = enter > leave;
    }
    if (!miss) return false;
  }
  return true;
}

// Whether `at` lies inside a pit box (closed). A monkey falls only when its
// whole disc is over the pits and it is not dashing; this is the cautious test.
inline bool inPit(const Observation& o, Vec2 at) {
  for (const kfm_obs_hazard& h : o.hazards()) {
    if (h.kind == KFM_HAZARD_PIT && at.x.raw >= h.box.minX && at.x.raw <= h.box.maxX &&
        at.y.raw >= h.box.minY && at.y.raw <= h.box.maxY) {
      return true;
    }
  }
  return false;
}

/* ---- banana flight --------------------------------------------------------------- */

inline constexpr int kPredictMax = KFM_PREDICT_MAX;  // 48: the longest path predict() makes

// A predicted banana path: at(0) now, at(k) after k Playing ticks. Kept as
// the C header's plain kfm_vec so a Path costs no zero-fill.
struct Path {
  kfm_vec pos[kPredictMax + 1];
  int ticks;   // entries 0..ticks are filled
  int flying;  // ticks before it lands (== ticks when still in flight at the end)

  Vec2 at(int k) const { return Vec2::of(pos[k]); }
  // The first tick k in [1, flying] whose motion brings the banana within
  // `reach` of a disc standing at `c` (swept, as the sim tests), 0 when it
  // already is, -1 when it never is. `reach` is the sum of both radii.
  int firstContact(Vec2 c, Fixed reach) const {
    const uint64_t r2 = sq(reach);
    if (distSq(at(0), c) < r2) return 0;
    for (int k = 1; k <= flying; ++k) {
      if (kfm_segment_min_sq((at(k - 1) - c).c(), (at(k) - at(k - 1)).c()) < r2) return k;
    }
    return -1;
  }
};

// Predicts banana b over `ticks` ticks (at most kPredictMax) with the C
// header's exact flight step (kfm_predict_banana): exact for an outbound
// banana, approximate once it homes on an owner that turns; an unseen owner
// under fog counts as gone (the banana grounds when it turns home). Fuel: the
// step costs about a thousand instructions, more with many interior walls,
// so an outbound banana that stays outbound over the horizon on a straight
// line clear of the walls and the outer box (exactly what the step would
// compute) is filled without stepping. Predict only what matters.
inline void predict(const Observation& o, Banana b, int ticks, Path& out) {
  if (ticks > kPredictMax) ticks = kPredictMax;
  if (ticks < 0) ticks = 0;
  out.ticks = ticks;
  if (!b) {
    out.pos[0] = kfm_vec{0, 0};
    out.flying = 0;
    return;
  }
  const Vec2 p = b.pos();
  const Vec2 end = p + b.vel() * ticks;
  const Vec2 lim = o.halfExtents() - Vec2{b.radius(), b.radius()};
  if (b.phase() == KFM_BANANA_OUTBOUND &&
      b.raw().ticksInPhase + ticks < o.rules().bananaOutboundTicks && abs(p.x) <= lim.x &&
      abs(p.y) <= lim.y && abs(end.x) <= lim.x && abs(end.y) <= lim.y &&
      lineClear(o, p, end, b.radius() + Fixed::fromRaw(256))) {
    for (int k = 0; k <= ticks; ++k)
      out.pos[k] = (p + b.vel() * k).c();
    out.flying = ticks;
    return;
  }
  out.flying = kfm_predict_banana(&o.raw(), b.index(), ticks, out.pos);
}

// Ticks until banana b touches a disc of `radius` standing at `at` (0: it
// already does), or -1 when it does not within `maxTicks`; the banana's own
// radius is added. Exact flight, see predict().
inline int timeToImpact(const Observation& o, Banana b, Vec2 at, Fixed radius, int maxTicks) {
  if (!b || !b.inFlight()) return -1;
  Path p;
  predict(o, b, maxTicks, p);
  return p.firstContact(at, radius + b.radius());
}

// Ticks until banana b hits the own monkey if it stays where it is, with
// `margin` to spare, or -1 when it does not within `maxTicks` (or cannot hurt
// it: see isThreat).
inline int ticksToHit(const Observation& o, Banana b, int maxTicks, Fixed margin) {
  if (!isThreat(o, b)) return -1;
  return timeToImpact(o, b, o.self().pos(), o.self().radius() + margin, maxTicks);
}

// Whether banana b reaches the own monkey within `maxTicks` if it stays put.
inline bool willHitMe(const Observation& o, Banana b, int maxTicks, Fixed margin) {
  return ticksToHit(o, b, maxTicks, margin) >= 0;
}

// The threats worth predicting this tick, each with its predicted path.
struct Threats {
  static constexpr int kMax = 4;
  Banana banana[kMax];
  Path path[kMax];  // path[i] is banana[i]'s over the look-ahead
  int count = 0;
};

// Fills `out` with at most `max` (<= Threats::kMax) threats (isThreat) that
// can reach the own monkey within `look` ticks, the soonest closest approach
// first, and predicts each. A banana farther away than its speed times
// `look` cannot arrive in time, and an outbound one flies straight until a
// wall, so one whose line misses by a unit (plus `margin`) cannot either:
// neither is predicted, as each prediction costs fuel.
inline void findThreats(const Observation& o, int look, Fixed margin, int max, Threats& out) {
  const Self me = o.self();
  out.count = 0;
  if (max > Threats::kMax) max = Threats::kMax;
  for (Banana b : o.bananas()) {
    if (!isThreat(o, b) || b.approachTicks() > look) continue;
    const Fixed reach = me.radius() + b.radius() + margin;
    if (dist(b.pos(), me.pos()) > b.speed() * look + reach) continue;
    if (b.phase() == KFM_BANANA_OUTBOUND && b.approachDistance() > reach + Fixed::one() &&
        lineClear(o, b.pos(), b.pos() + b.vel() * look, b.radius())) {
      continue;
    }
    // Insert by approachTicks, dropping the latest when full.
    int i = out.count < max ? out.count++ : max;
    if (i == max && b.approachTicks() >= out.banana[max - 1].approachTicks()) continue;
    if (i == max) i = max - 1;
    for (; i > 0 && out.banana[i - 1].approachTicks() > b.approachTicks(); --i)
      out.banana[i] = out.banana[i - 1];
    out.banana[i] = b;
  }
  for (int i = 0; i < out.count; ++i)
    predict(o, out.banana[i], look, out.path[i]);
}

/* ---- the grid space -------------------------------------------------------------- */

// Octants: 0 east, then counter-clockwise (1 north-east, 2 north, ... 7
// south-east). The stick byte of octant k is k * 32; a byte maps to the
// octant ((byte + 16) >> 5) & 7 (the sim's rounding).
inline int octantOf(uint8_t angle8) { return ((angle8 + 16) >> 5) & 7; }
inline uint8_t octantDir(int octant) { return static_cast<uint8_t>((octant & 7) * 32); }
inline bool diagonal(int octant) { return (octant & 1) != 0; }
// The column and row step of an octant (rows grow southward).
inline int octantCol(int octant) {
  constexpr int k[8] = {1, 1, 0, -1, -1, -1, 0, 1};
  return k[octant & 7];
}
inline int octantRow(int octant) {
  constexpr int k[8] = {0, -1, -1, -1, 0, 1, 1, 1};
  return k[octant & 7];
}
// The neighbour of a cell along an octant (it may lie outside the arena).
inline Cell neighbour(Cell c, int octant) {
  return Cell{c.col + octantCol(octant), c.row + octantRow(octant)};
}
// The larger of the column and row distances (the fog's and calls' metric).
inline int chebyshev(Cell a, Cell b) {
  const int dc = a.col > b.col ? a.col - b.col : b.col - a.col;
  const int dr = a.row > b.row ? a.row - b.row : b.row - a.row;
  return dc > dr ? dc : dr;
}
// The octant of the neighbour cell of `from` in the direction of `to` (the
// signs of the column and row differences), or -1 when they are the same cell.
inline int octantToward(Cell from, Cell to) {
  const int dc = (to.col > from.col) - (to.col < from.col);
  const int dr = (to.row > from.row) - (to.row < from.row);
  for (int k = 0; k < 8; ++k)
    if (octantCol(k) == dc && octantRow(k) == dr) return k;
  return -1;
}

// The arena as cells for the own monkey: the cell size (learned from the own
// cell, as Observation v1 does not carry it), and which cells its circle may
// stand in. Call update() every tick; it is cheap once built. At most 64 x 64
// cells. In a continuous mode it stays empty (ready() false).
class Grid {
 public:
  static constexpr int kMaxSide = 64;

  void update(const Observation& o) {
    if (!o.grid()) return;
    const Self me = o.self();
    const Vec2 half = o.halfExtents();
    // At rest on a cell centre: x + hx = (col + 1/2) * cell, and the same
    // down the rows; the two estimates must agree.
    if (me.alive() && me.vel().zero() && me.raw().posX == me.raw().prevPosX &&
        me.raw().posY == me.raw().prevPosY && me.cell().valid()) {
      const int64_t cx = ((int64_t{me.pos().x.raw} + half.x.raw) * 2) / (2 * me.cell().col + 1);
      const int64_t cy = ((int64_t{half.y.raw} - me.pos().y.raw) * 2) / (2 * me.cell().row + 1);
      if (cx > 0 && cx < 0x7FFFFFFF && cx - cy <= 2 && cy - cx <= 2) {
        cell_ = Fixed{static_cast<int32_t>(cx)};
      }
    }
    if (me.alive() && (cell_ != builtCell_ || me.radius() != radius_)) build(o, me.radius());
  }
  bool ready() const { return cols_ > 0; }
  Fixed cellSize() const { return cell_; }
  int cols() const { return cols_; }
  int rows() const { return rows_; }

  // The cell holding `p` (the sim's mapping: a point on an edge belongs to
  // the cell east of / south of it), clamped to the arena; Cell{} before
  // the map is ready().
  Cell cellOf(Vec2 p) const {
    int col = ((p.x + half_.x) / cell_).floor();
    int row = ((half_.y - p.y) / cell_).floor();
    col = col < 0 ? 0 : (col >= cols_ ? cols_ - 1 : col);
    row = row < 0 ? 0 : (row >= rows_ ? rows_ - 1 : row);
    return Cell{col, row};
  }
  // The centre of a cell (where a grid monkey rests).
  Vec2 centre(Cell c) const {
    return Vec2{cell_ * c.col + cell_ / 2 - half_.x, half_.y - cell_ * c.row - cell_ / 2};
  }
  bool inside(Cell c) const { return c.col >= 0 && c.row >= 0 && c.col < cols_ && c.row < rows_; }
  // The own circle fits at the cell's centre: inside the outer box, clear of
  // every wall, and not a pit cell (the sim lets a monkey walk into a pit;
  // this map never does).
  bool open(Cell c) const { return inside(c) && (open_[c.row] & bit(c.col)) != 0; }
  // The sim's latch rule for one step: the target open and, for a diagonal,
  // both orthogonal neighbours open (no corner cutting).
  bool canStep(Cell from, int octant) const {
    if (!open(neighbour(from, octant))) return false;
    if (!diagonal(octant)) return true;
    return open(Cell{from.col + octantCol(octant), from.row}) &&
           open(Cell{from.col, from.row + octantRow(octant)});
  }
  // Where a dash of `cells` along `octant` from `from` ends: each cell checked
  // like a step, so a wall stops it on the last free cell.
  Cell dashLanding(Cell from, int octant, int cells) const {
    Cell at = from;
    for (int i = 0; i < cells && canStep(at, octant); ++i)
      at = neighbour(at, octant);
    return at;
  }
  // The cells a dash covers: its speed times its active ticks over the cell
  // size (the grid content's convention), at least 1.
  int dashCells(const Ability& dash) const {
    const int n = cell_ > Fixed{} ? (dash.speed() * dash.activeTicks() / cell_).round() : 1;
    return n < 1 ? 1 : n;
  }

  // The octant of a first step on a shortest walk from `from` to `goal` over
  // the open cells (8-connected, no corner cutting), or -1 when `from` is the
  // goal, the goal cannot be reached, or it is more than `maxSteps` steps
  // away. A breadth-first search from the goal over row bit masks: about one
  // thousand instructions per step of distance on a 24 x 14 map, so walk
  // straight when nothing is in the way and search when something is.
  int stepToward(Cell from, Cell goal, int maxSteps = kMaxSide * kMaxSide) const {
    if (!ready() || !inside(from) || !inside(goal) || from == goal) return -1;
    // Row r lives at index r + 1, between two empty rows.
    uint64_t op[kMaxSide + 2], seen[kMaxSide + 2], a[kMaxSide + 2], b[kMaxSide + 2];
    op[0] = seen[0] = a[0] = b[0] = 0;
    op[rows_ + 1] = seen[rows_ + 1] = a[rows_ + 1] = b[rows_ + 1] = 0;
    for (int r = 0; r < rows_; ++r) {
      op[r + 1] = open_[r];
      seen[r + 1] = a[r + 1] = 0;
    }
    op[from.row + 1] |= bit(from.col);
    op[goal.row + 1] |= bit(goal.col);
    a[goal.row + 1] = seen[goal.row + 1] = bit(goal.col);
    // The cells one allowed step from `from`: its row above, its own, below.
    uint64_t steps[3] = {0, 0, 0};
    for (int k = 0; k < 8; ++k)
      if (canStep(from, k)) steps[octantRow(k) + 1] |= bit(from.col + octantCol(k));
    const int straight = octantToward(from, goal);
    uint64_t* front = a;
    uint64_t* next = b;
    for (int layer = 0; layer < maxSteps; ++layer) {
      // The first layer that holds a cell the monkey can step to is one
      // closer to the goal than the monkey: take the straightest such step.
      const uint64_t* f = front + from.row;  // f[0..2]: the rows above, at, below
      if (((f[0] & steps[0]) | (f[1] & steps[1]) | (f[2] & steps[2])) != 0) {
        int best = -1;
        int bestTurn = 9;
        for (int k = 0; k < 8; ++k) {
          const Cell n = neighbour(from, k);
          if (!inside(n) || ((f[octantRow(k) + 1] & steps[octantRow(k) + 1]) & bit(n.col)) == 0) {
            continue;
          }
          const int turn = (k - straight + 8) & 7;
          const int t = turn > 4 ? 8 - turn : turn;
          if (t < bestTurn) {
            best = k;
            bestTurn = t;
          }
        }
        return best;
      }
      if (!grow(op, seen, front, next)) return -1;
      uint64_t* t = front;
      front = next;
      next = t;
    }
    return -1;
  }

 private:
  void build(const Observation& o, Fixed radius) {
    builtCell_ = cell_;
    radius_ = radius;
    half_ = o.halfExtents();
    cols_ = (half_.x * 2 / cell_).floor();
    rows_ = (half_.y * 2 / cell_).floor();
    if (cols_ <= 0 || rows_ <= 0 || cols_ > kMaxSide || rows_ > kMaxSide) {
      cols_ = rows_ = 0;
      return;
    }
    // The outer box: a column (row) is open when the circle at its centre
    // fits inside it.
    uint64_t colsOk = 0;
    for (int c = 0; c < cols_; ++c)
      if (abs(centre(Cell{c, 0}).x) <= half_.x - radius) colsOk |= bit(c);
    for (int r = 0; r < rows_; ++r)
      open_[r] = abs(centre(Cell{0, r}).y) <= half_.y - radius ? colsOk : 0;
    // Walls: only the cells near each box are tested (strict overlap).
    for (const kfm_obs_box& w : o.walls()) {
      const Cell a = cellOf(Vec2::fromRaw(w.minX, w.maxY) + Vec2{-radius, radius});
      const Cell b = cellOf(Vec2::fromRaw(w.maxX, w.minY) + Vec2{radius, -radius});
      for (int r = a.row; r <= b.row; ++r) {
        for (int c = a.col; c <= b.col; ++c) {
          const Vec2 at = centre(Cell{c, r});
          const Vec2 near = Vec2{clamp(at.x, Fixed{w.minX}, Fixed{w.maxX}),
                                 clamp(at.y, Fixed{w.minY}, Fixed{w.maxY})};
          if (distSq(at, near) < sq(radius)) open_[r] &= ~bit(c);
        }
      }
    }
    // Pits: a cell whose centre lies in a pit box is closed.
    for (const kfm_obs_hazard& h : o.hazards()) {
      if (h.kind != KFM_HAZARD_PIT) continue;
      const Cell a = cellOf(Vec2::fromRaw(h.box.minX, h.box.maxY));
      const Cell b = cellOf(Vec2::fromRaw(h.box.maxX, h.box.minY));
      for (int r = a.row; r <= b.row; ++r)
        for (int c = a.col; c <= b.col; ++c)
          if (inPit(o, centre(Cell{c, r}))) open_[r] &= ~bit(c);
    }
  }

  static uint64_t bit(int col) { return uint64_t{1} << col; }

  // One breadth-first layer over padded rows (index r + 1): the open, unseen
  // cells one step from `front` (a diagonal needs both orthogonal cells
  // open), written to `next` and added to `seen`. False when there are none.
  bool grow(const uint64_t* op, uint64_t* seen, const uint64_t* front, uint64_t* next) const {
    uint64_t any = 0;
    for (int r = 1; r <= rows_; ++r) {
      const uint64_t f = front[r];
      const uint64_t up = front[r - 1];    // the row to the north
      const uint64_t dn = front[r + 1];    // the row to the south
      const uint64_t fromDn = dn & op[r];  // a step north from a southern cell
      const uint64_t fromUp = up & op[r];  // a step south from a northern cell
      uint64_t n = (f << 1) | (f >> 1) | fromDn | fromUp;
      n |= ((dn << 1) & op[r + 1]) & (fromDn << 1);  // north-east, both orthogonals open
      n |= ((dn >> 1) & op[r + 1]) & (fromDn >> 1);  // north-west
      n |= ((up << 1) & op[r - 1]) & (fromUp << 1);  // south-east
      n |= ((up >> 1) & op[r - 1]) & (fromUp >> 1);  // south-west
      n &= op[r] & ~seen[r];
      next[r] = n;
      seen[r] |= n;
      any |= n;
    }
    return any != 0;
  }

  Fixed cell_ = Fixed::one();  // learned; one unit until then
  Fixed builtCell_;
  Fixed radius_;
  Vec2 half_;
  int cols_ = 0;
  int rows_ = 0;
  uint64_t open_[kMaxSide] = {};
};

/* ---- the dash-catch ------------------------------------------------------------- */

// The stick direction of a Dash pressed in this tick's answer that catches
// banana b, or nothing: the dash slot is a ready Dash with a perfect window,
// the own monkey has room (ammo < ammoMax), the banana is in flight and not
// the own outbound one, and the dash meets the banana's predicted `path`
// inside the window [press, press + perfectWindow) with `margin` to spare.
// `path` is predict()'s for b over at least inputDelay + perfectWindow ticks
// (pass the one a dodge check already made). Continuous: a straight dash at
// the def's speed toward each of the banana's positions in the window, walls
// ignored (as the C header's kfm_dash_catch_dir). Grid: the eight octants,
// each a slide of grid.dashCells() whole cells, stopped by walls.
inline Maybe<uint8_t> dashCatchDir(const Observation& o, const Grid& grid, Banana b,
                                   const Path& path, Fixed margin) {
  const Self me = o.self();
  const Ability dash = me.ability(kSlotDash);
  if (!b || !me.alive() || !dash.is(KFM_ABILITY_DASH) || !dash.ready()) return {};
  if (dash.perfectWindow() == 0 || me.ammo() >= me.ammoMax()) return {};
  if (!b.inFlight() || (b.mine() && b.phase() == KFM_BANANA_OUTBOUND)) return {};
  if (o.grid() && !grid.ready()) return {};
  const int lead = o.inputDelay();
  const int last = lead + dash.perfectWindow();
  if (path.ticks < last || path.flying < last) return {};  // too short, or it lands first
  const Fixed contact = me.radius() + b.radius() - margin;
  if (contact <= Fixed{}) return {};
  const uint64_t reach = sq(contact);
  const Vec2 start = me.pos() + me.vel() * lead;  // inputs already sent keep it moving
  const int active = dash.activeTicks() > 0 ? dash.activeTicks() : 1;
  // Whether a dash moving `step` a tick over its active ticks meets the path.
  auto meets = [&](Vec2 step) {
    auto at = [&](int j) {  // the monkey j ticks after the press
      j -= dash.startupTicks();
      return start + step * (j < 0 ? 0 : (j > active ? active : j));
    };
    for (int k = lead + 1; k <= last; ++k) {
      const Vec2 m0 = at(k - 1 - lead);
      const Vec2 m1 = at(k - lead);
      const Vec2 d = (path.at(k) - path.at(k - 1)) - (m1 - m0);
      if (kfm_segment_min_sq((path.at(k - 1) - m0).c(), d.c()) < reach) return true;
    }
    return false;
  };
  if (!o.grid()) {
    for (int k = lead + 1; k <= last; ++k) {
      const uint8_t d = dirTo(start, path.at(k));
      if (meets(Vec2::fromAngle8(d, dash.speed()))) return just(d);
    }
    return {};
  }
  const Cell from = grid.cellOf(start);
  for (int oct = 0; oct < 8; ++oct) {
    const Cell land = grid.dashLanding(from, oct, grid.dashCells(dash));
    const Vec2 move = grid.centre(land) - start;
    if (land != from && meets(Vec2{move.x / active, move.y / active})) return just(octantDir(oct));
  }
  return {};
}

// The same, predicting the banana's path itself.
inline Maybe<uint8_t> dashCatchDir(const Observation& o, const Grid& grid, Banana b, Fixed margin) {
  if (!b) return {};
  Path path;
  predict(o, b, o.inputDelay() + o.self().ability(kSlotDash).perfectWindow(), path);
  return dashCatchDir(o, grid, b, path, margin);
}

}  // namespace kfm

/* ---- the exports -------------------------------------------------------------------
 * KFM_BOT(T) defines kfm_init and kfm_act over one constant-initialised T
 * (the C header's KFM_BOT_MAIN defines kfm_abi_version, kfm_obs_buffer and
 * the observation buffer kfm_obs). kfm_act answers an observation this SDK
 * does not understand (a version or size mismatch) with the idle input. */
#if defined(KFM_BOT_MAIN)
#define KFM_BOT(T)                                         \
  static constinit T kfm_bot_instance{};                   \
  extern "C" void kfm_init(uint64_t seed, uint32_t slot) { \
    kfm_bot_instance = T{};                                \
    kfm_bot_instance.init(seed, slot);                     \
  }                                                        \
  extern "C" uint64_t kfm_act(uint32_t tick) {             \
    const ::kfm::Observation kfm_view(kfm_obs);            \
    if (!kfm_view.ok()) return 0;                          \
    return kfm_bot_instance.act(tick, kfm_view).pack();    \
  }
#else
#define KFM_BOT(T)              \
  static_assert(sizeof(T) == 0, \
                "KFM_BOT needs #define KFM_BOT_MAIN before #include \"kfm_bot.hpp\"")
#endif

#endif /* KFM_BOT_HPP */
