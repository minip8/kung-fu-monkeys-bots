//! `kfm build` and `kfm test` with the real toolchain, when it is around:
//! the build image under docker (`KFM_TEST_BUILD_IMAGE`, default
//! `kfm-bot-build:local`, the image CI builds), a local wasm clang (`CLANG`,
//! `EMSDK` or `~/emsdk`) and a `kfm_arena` (`KFM_TEST_ARENA`, default the
//! repository's `.kfm/kfm_arena` that tools/fetch_kfm_arena.sh writes). Each
//! test says why it is skipped when something is missing.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

fn image() -> String {
    std::env::var("KFM_TEST_BUILD_IMAGE")
        .ok()
        .filter(|i| !i.is_empty())
        .unwrap_or_else(|| "kfm-bot-build:local".to_owned())
}

/// Whether docker runs and has the image; prints why not.
fn have_image(test: &str) -> bool {
    let ok = Command::new("docker")
        .args(["image", "inspect", &image()])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!(
            "{test}: skipped: no docker or no image {} (docker build -t kfm-bot-build:local .)",
            image()
        );
    }
    ok
}

fn arena(test: &str) -> Option<PathBuf> {
    let path = std::env::var("KFM_TEST_ARENA")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| repo().join(".kfm/kfm_arena"));
    if path.is_file() {
        Some(path)
    } else {
        eprintln!(
            "{test}: skipped: no kfm_arena at {} (tools/fetch_kfm_arena.sh, or set KFM_TEST_ARENA)",
            path.display()
        );
        None
    }
}

/// A wasm clang the way `kfm build --local` looks for one.
fn local_clang(test: &str) -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("CLANG").map(PathBuf::from),
        std::env::var_os("EMSDK").map(|e| PathBuf::from(e).join("upstream/bin/clang")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("emsdk/upstream/bin/clang")),
    ];
    let found = candidates.into_iter().flatten().find(|c| c.is_file());
    if found.is_none() {
        eprintln!("{test}: skipped: no emsdk clang (set CLANG or EMSDK)");
    }
    found
}

fn build(home: &Home, fake: &Fake, dir: &Path, out: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["build", dir.to_str().unwrap(), "-o", out.to_str().unwrap()];
    args.extend_from_slice(extra);
    home.run(fake, &args)
}

#[test]
fn the_starter_builds_in_the_image_and_plays_clean() {
    let test = "the_starter_builds_in_the_image_and_plays_clean";
    if !have_image(test) {
        return;
    }
    let fake = start();
    let home = Home::new();
    let out = home.dir.path().join("starter/bot.wasm");
    let image = image();
    let built = build(
        &home,
        &fake,
        &repo().join("examples/starter"),
        &out,
        &["--image", &image],
    );
    expect(&built, 0);
    assert!(
        stdout(&built).contains("bytes, sha256 "),
        "{}",
        stdout(&built)
    );
    assert!(home.dir.path().join("starter/build.log").is_file());
    assert!(
        stderr(&built).contains("--network none --memory 2g"),
        "{}",
        stderr(&built)
    );

    let Some(arena) = arena(test) else { return };
    let played = home.run(
        &fake,
        &[
            "test",
            out.to_str().unwrap(),
            "--arena",
            arena.to_str().unwrap(),
            "--matches",
            "2",
        ],
    );
    expect(&played, 0);
    let text = stdout(&played);
    assert!(text.contains("2 match(es) of contest"), "{text}");
    assert!(text.contains("no fault in 10 module slot(s)"), "{text}");
    assert!(text.contains("fuel_max "), "{text}");

    let json = home.run(
        &fake,
        &[
            "test",
            out.to_str().unwrap(),
            "--arena",
            arena.to_str().unwrap(),
            "--matches",
            "2",
            "--ffa",
            "--json",
        ],
    );
    expect(&json, 0);
    let lines: Vec<serde_json::Value> = stdout(&json)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "two match objects and the summary");
    assert_eq!(lines[2]["summary"], true);
}

#[test]
fn a_local_build_is_the_image_build_byte_for_byte() {
    let test = "a_local_build_is_the_image_build_byte_for_byte";
    let Some(clang) = local_clang(test) else {
        return;
    };
    let fake = start();
    let home = Home::new();
    for example in ["starter", "chaser", "idle"] {
        let local = home.dir.path().join(format!("local/{example}.wasm"));
        let out = home
            .command(
                &fake,
                &[
                    "build",
                    repo().join("examples").join(example).to_str().unwrap(),
                    "-o",
                    local.to_str().unwrap(),
                    "--local",
                ],
            )
            .env("CLANG", &clang)
            .output()
            .unwrap();
        expect(&out, 0);
        if !have_image(test) {
            continue;
        }
        let boxed = home.dir.path().join(format!("image/{example}.wasm"));
        let image = image();
        expect(
            &build(
                &home,
                &fake,
                &repo().join("examples").join(example),
                &boxed,
                &["--image", &image],
            ),
            0,
        );
        assert_eq!(
            std::fs::read(&local).unwrap(),
            std::fs::read(&boxed).unwrap(),
            "{example}: the local build differs from the image's (is CLANG emsdk 6.0.13?)"
        );
    }
}

#[test]
fn a_bot_that_does_not_compile_or_traps_is_caught() {
    let test = "a_bot_that_does_not_compile_or_traps_is_caught";
    let Some(clang) = local_clang(test) else {
        return;
    };
    let fake = start();
    let home = Home::new();
    let broken = bot_dir(&[("bot.c", "int main( {")]);
    let out = home
        .command(
            &fake,
            &["build", broken.path().to_str().unwrap(), "--local"],
        )
        .env("CLANG", &clang)
        .output()
        .unwrap();
    expect(&out, 1);
    assert!(
        stderr(&out).contains("kfm: the compile failed"),
        "{}",
        stderr(&out)
    );
    assert!(!broken.path().join("out/bot.wasm").exists());

    // An import the host refuses: the build deletes the module.
    let importer = bot_dir(&[(
        "bot.c",
        "#define KFM_BOT_MAIN\n#include \"kfm_bot.h\"\nvoid ext(void);\n\
         void kfm_init(uint64_t s, uint32_t l) { (void)s; (void)l; ext(); }\n\
         uint64_t kfm_act(uint32_t t) { (void)t; return 0; }\n",
    )]);
    let out = home
        .command(
            &fake,
            &["build", importer.path().to_str().unwrap(), "--local"],
        )
        .env("CLANG", &clang)
        .output()
        .unwrap();
    expect(&out, 1);
    assert!(stderr(&out).contains("env.ext"), "{}", stderr(&out));
    assert!(!importer.path().join("out/bot.wasm").exists());

    let trapper = bot_dir(&[(
        "bot.c",
        "#define KFM_BOT_MAIN\n#include \"kfm_bot.h\"\n\
         void kfm_init(uint64_t s, uint32_t l) { (void)s; (void)l; }\n\
         uint64_t kfm_act(uint32_t t) { if (t > 30) __builtin_trap(); return 0; }\n",
    )]);
    let out = home
        .command(
            &fake,
            &["build", trapper.path().to_str().unwrap(), "--local"],
        )
        .env("CLANG", &clang)
        .output()
        .unwrap();
    expect(&out, 0);
    let Some(arena) = arena(test) else { return };
    let module = trapper.path().join("out/bot.wasm");
    let played = home.run(
        &fake,
        &[
            "test",
            module.to_str().unwrap(),
            "--arena",
            arena.to_str().unwrap(),
            "--matches",
            "1",
        ],
    );
    expect(&played, 1);
    assert!(
        stdout(&played).contains("FAULTS: traps "),
        "{}",
        stdout(&played)
    );
    assert!(
        stderr(&played).contains("the module faulted"),
        "{}",
        stderr(&played)
    );
}
