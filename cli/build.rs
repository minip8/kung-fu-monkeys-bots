//! Pins two releases into the binary (cli/README.md "Pins"):
//!
//! - `KFM_PINNED_ENGINE_TAG`: the engine release whose `kfm_arena` `kfm test`
//!   plays when the ladder does not name one: `$KFM_ENGINE_TAG` at build time,
//!   else `tag=` of the repository's `engine.lock`.
//! - `KFM_PINNED_BOTS_TAG`: this repository's release, which names the bot
//!   build image `kfm build` runs (`ghcr.io/minip8/kfm-bot-build:<tag>`):
//!   `$KFM_BOTS_TAG` at build time (the release workflow sets it to the tag
//!   it builds), else `v` + the crate's version.
//!
//! The SDK headers `kfm build --local` compiles against are embedded from
//! `../sdk` by `include_bytes!`; this script only makes cargo rebuild when
//! they change.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=KFM_ENGINE_TAG");
    println!("cargo:rerun-if-env-changed=KFM_BOTS_TAG");
    println!("cargo:rerun-if-changed=../engine.lock");
    println!("cargo:rerun-if-changed=../sdk/c/kfm_bot.h");
    println!("cargo:rerun-if-changed=../sdk/cpp/kfm_bot.hpp");

    let engine = std::env::var("KFM_ENGINE_TAG")
        .ok()
        .filter(|t| !t.is_empty())
        .or_else(|| lock_tag(Path::new("../engine.lock")))
        .expect(
            "no engine release to pin: set KFM_ENGINE_TAG or build from the repository \
             (engine.lock's tag=)",
        );
    let bots = std::env::var("KFM_BOTS_TAG")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("v{}", std::env::var("CARGO_PKG_VERSION").unwrap()));
    println!("cargo:rustc-env=KFM_PINNED_ENGINE_TAG={engine}");
    println!("cargo:rustc-env=KFM_PINNED_BOTS_TAG={bots}");
}

/// The last `tag=<value>` line of engine.lock, as tools/lock.sh reads it.
fn lock_tag(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|l| l.strip_prefix("tag="))
        .map(|v| {
            v.split(|c: char| c.is_whitespace() || c == '#')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .next_back()
        .filter(|t| !t.is_empty())
}
