/* idle: the smallest bot built from the C SDK (bot ABI v1). It never moves
 * and never presses anything. Build: tools/build.sh examples/idle out/idle.wasm. */
#define KFM_BOT_MAIN
#include "kfm_bot.h"

void kfm_init(uint64_t seed, uint32_t slot) {
  (void)seed;
  (void)slot;
}

uint64_t kfm_act(uint32_t tick) {
  (void)tick;
  return 0;
}
