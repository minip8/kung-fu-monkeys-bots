//! `kfm build`: the recipe of `tools/build.sh` and docs/build.md, in the
//! pinned build image (the default, the ladder's exact toolchain) or with a
//! local clang (`--local`), then the module checks.
//!
//! The image run is tools/build_in_sandbox.sh's: the command of architecture
//! §8 "The build sandbox" with its flags verbatim, the source mounted
//! read-only and a fresh staging directory as `/out`. The local run is
//! tools/build.sh reimplemented flag for flag (the same compiler lookup, the
//! same sources in byte order, the same command), against the SDK headers
//! embedded in this binary, so a user without a checkout builds the module
//! the ladder builds when their clang is the image's (emsdk 6.0.13).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::config;
use crate::error::{other, Context, Error, Result};
use crate::wasm;

/// The bot build image's repository; the tag is this CLI's release.
pub const IMAGE_REPO: &str = "ghcr.io/minip8/kfm-bot-build";
/// This CLI's release, pinned at build time (build.rs).
pub const BOTS_TAG: &str = env!("KFM_PINNED_BOTS_TAG");
/// The SDK as this release ships it: what `--local` compiles against.
const SDK_C: &[u8] = include_bytes!("../../sdk/c/kfm_bot.h");
const SDK_CPP: &[u8] = include_bytes!("../../sdk/cpp/kfm_bot.hpp");

/// `ghcr.io/minip8/kfm-bot-build:<this release>`.
pub fn default_image() -> String {
    format!("{IMAGE_REPO}:{BOTS_TAG}")
}

/// The flags of docs/build.md §1 before the language's, with the SDK at `sdk`.
pub fn common_flags(sdk: &Path) -> Vec<String> {
    let mut flags: Vec<String> = [
        "--target=wasm32",
        "-mcpu=mvp",
        "-mmutable-globals",
        "--no-wasm-opt",
        "-O2",
        "-nostdlib",
        "-ffreestanding",
        "-Wall",
        "-Wextra",
        "-Werror",
    ]
    .map(str::to_owned)
    .to_vec();
    for sub in ["c", "cpp"] {
        flags.push("-I".to_owned());
        flags.push(sdk.join(sub).to_string_lossy().into_owned());
    }
    flags.extend(
        [
            "-Wl,--no-entry",
            "-Wl,--export=kfm_abi_version",
            "-Wl,--export=kfm_obs_buffer",
            "-Wl,--export=kfm_init",
            "-Wl,--export=kfm_act",
            "-Wl,--export=__stack_pointer",
            "-Wl,--allow-undefined",
            "-Wl,-z,stack-size=65536",
            "-Wl,--strip-all",
        ]
        .map(str::to_owned),
    );
    flags
}

/// The sources the recipe compiles: the `.c`, `.cc` and `.cpp` regular files
/// directly in `dir` (no links, no subdirectories), in byte order of their
/// names. (C files, C++ files.)
pub fn sources(dir: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry.context(|| format!("cannot read {}", dir.display()))?;
        let meta = std::fs::symlink_metadata(entry.path())
            .context(|| format!("cannot read {}", entry.path().display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if meta.is_file() && crate::pack::is_source(name) {
            names.push(name.to_owned());
        }
    }
    names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let (c, cxx): (Vec<_>, Vec<_>) = names.into_iter().partition(|n| n.ends_with(".c"));
    Ok((
        c.into_iter().map(|n| dir.join(n)).collect(),
        cxx.into_iter().map(|n| dir.join(n)).collect(),
    ))
}

/// The compiler command for `sources` (tools/build.sh): C11 for a directory
/// of C only, else C++20 over every file with `-x c++`, through `clang++`
/// beside `cc` (or the same driver in g++ mode).
pub fn command(cc: &Path, sdk: &Path, out: &Path, c: &[PathBuf], cxx: &[PathBuf]) -> Vec<String> {
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let mut cmd = Vec::new();
    if cxx.is_empty() {
        cmd.push(path(cc));
        cmd.extend(common_flags(sdk));
        cmd.extend(["-std=c11".to_owned(), "-o".to_owned(), path(out)]);
        cmd.extend(c.iter().map(|p| path(p)));
    } else {
        let clangxx = cc.with_file_name(if cfg!(windows) {
            "clang++.exe"
        } else {
            "clang++"
        });
        if is_executable(&clangxx) {
            cmd.push(path(&clangxx));
        } else {
            cmd.push(path(cc));
            cmd.push("--driver-mode=g++".to_owned());
        }
        cmd.extend(common_flags(sdk));
        cmd.extend(
            [
                "-std=c++20",
                "-fno-exceptions",
                "-fno-rtti",
                "-fno-threadsafe-statics",
                "-o",
            ]
            .map(str::to_owned),
        );
        cmd.push(path(out));
        cmd.extend(["-x".to_owned(), "c++".to_owned()]);
        cmd.extend(c.iter().chain(cxx).map(|p| path(p)));
    }
    cmd
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// `name` on `PATH`, as `command -v` finds it.
pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidates = if cfg!(windows) {
            vec![dir.join(format!("{name}.exe")), dir.join(name)]
        } else {
            vec![dir.join(name)]
        };
        candidates.into_iter().find(|p| is_executable(p))
    })
}

fn output_of(cc: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(cc)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A usable compiler targets wasm32 and finds `wasm-ld` (beside it or on PATH).
fn targets_wasm32(cc: &Path) -> bool {
    let targets = output_of(cc, &["-print-targets"]).unwrap_or_default();
    if !targets.contains("wasm32") {
        return false;
    }
    let ld = output_of(cc, &["--target=wasm32", "-print-prog-name=wasm-ld"]).unwrap_or_default();
    which(ld.trim()).is_some()
}

/// The compiler: `$CLANG`, else `clang` on PATH when it targets wasm32 (with
/// wasm-ld), else `$EMSDK/upstream/bin/clang`, else `~/emsdk/upstream/bin/clang`.
pub fn find_clang() -> Result<PathBuf> {
    if let Some(cc) = std::env::var_os("CLANG").filter(|c| !c.is_empty()) {
        return Ok(PathBuf::from(cc));
    }
    let exe = if cfg!(windows) { "clang.exe" } else { "clang" };
    let mut candidates = Vec::new();
    candidates.extend(which("clang"));
    if let Some(emsdk) = std::env::var_os("EMSDK").filter(|e| !e.is_empty()) {
        candidates.push(PathBuf::from(emsdk).join("upstream/bin").join(exe));
    }
    if let Some(home) = directories::BaseDirs::new() {
        candidates.push(home.home_dir().join("emsdk/upstream/bin").join(exe));
    }
    candidates
        .into_iter()
        .find(|c| is_executable(c) && targets_wasm32(c))
        .ok_or_else(|| {
            other(
                "no clang with a wasm32 target and wasm-ld (set CLANG or EMSDK; the ladder's is \
                 emsdk 6.0.13: emsdk install 6.0.13 && emsdk activate 6.0.13), or build in the \
                 image without --local",
            )
        })
}

/// The embedded SDK written out once per release under the cache:
/// `<cache>/sdk-<hash>/{c/kfm_bot.h,cpp/kfm_bot.hpp}` (the C++ header
/// includes `../c/kfm_bot.h`). `$KFM_SDK_DIR` names a checkout's `sdk/`.
pub fn sdk_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("KFM_SDK_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let mut hasher = Sha256::new();
    hasher.update(SDK_C);
    hasher.update(SDK_CPP);
    let hash = hex::encode(hasher.finalize());
    let dir = config::cache_dir()?.join(format!("sdk-{}", &hash[..12]));
    for (rel, data) in [("c/kfm_bot.h", SDK_C), ("cpp/kfm_bot.hpp", SDK_CPP)] {
        let path = dir.join(rel);
        if std::fs::read(&path).is_ok_and(|have| have == data) {
            continue;
        }
        let parent = path.parent().expect("a file in a directory");
        std::fs::create_dir_all(parent)
            .context(|| format!("cannot create {}", parent.display()))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, data).context(|| format!("cannot write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(dir)
}

/// Checks the module at `out` as tools/build.sh does (the host's load rules
/// and the recipe's exact exports); deletes it when it fails, since the only
/// output that counts is a checked module. Its size and SHA-256.
pub fn check_module(out: &Path) -> Result<(usize, String)> {
    let bytes = std::fs::read(out).context(|| format!("cannot read {}", out.display()))?;
    let checked = wasm::check(&bytes).and_then(|info| wasm::check_recipe(&info));
    if let Err(why) = checked {
        let _ = std::fs::remove_file(out);
        return Err(other(format!("{}: {why}", out.display())));
    }
    Ok((bytes.len(), hex::encode(Sha256::digest(&bytes))))
}

/// `kfm build --local`.
pub fn build_local(dir: &Path, out: &Path) -> Result<()> {
    let cc = find_clang()?;
    let (c, cxx) = sources(dir)?;
    if c.is_empty() && cxx.is_empty() {
        return Err(Error::Usage(format!(
            "no .c, .cc or .cpp source directly in {}",
            dir.display()
        )));
    }
    let sdk = sdk_dir()?;
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .context(|| format!("cannot create {}", parent.display()))?;
    }
    if let Some(version) = output_of(&cc, &["--version"]) {
        eprintln!("{}", version.lines().next().unwrap_or_default());
    }
    let cmd = command(&cc, &sdk, out, &c, &cxx);
    eprintln!("+ {}", cmd.join(" "));
    let status = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .status()
        .context(|| format!("cannot run {}", cmd[0]))?;
    if !status.success() {
        let _ = std::fs::remove_file(out);
        return Err(other("the compile failed"));
    }
    Ok(())
}

/// The container engine: `$DOCKER`, else `docker`.
fn docker() -> String {
    std::env::var("DOCKER")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "docker".to_owned())
}

/// The sandbox command's arguments after the engine (architecture §8, flags verbatim).
pub fn sandbox_args(src: &Path, stage: &Path, image: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "--network",
        "none",
        "--memory",
        "2g",
        "--cpus",
        "1",
        "--pids-limit",
        "256",
        "--read-only",
        "--tmpfs",
        "/tmp",
        "--user",
        "builder",
        "-v",
    ]
    .map(str::to_owned)
    .to_vec();
    args.push(format!("{}:/src:ro", src.display()));
    args.push("-v".to_owned());
    args.push(format!("{}:/out", stage.display()));
    args.push(image.to_owned());
    args.extend(["timeout", "120", "kfm-build"].map(str::to_owned));
    args
}

/// Copies `from` to our stderr and into `log` as it comes.
fn tee(
    mut from: impl Read + Send + 'static,
    log: Arc<Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = from.read(&mut buf) {
            if n == 0 {
                break;
            }
            let _ = std::io::stderr().write_all(&buf[..n]);
            log.lock()
                .expect("the log lock")
                .extend_from_slice(&buf[..n]);
        }
    })
}

/// `kfm build` in the image: the module to `out`, the build's output to
/// `log_path` as well as the terminal.
pub fn build_in_image(dir: &Path, out: &Path, image: &str, log_path: &Path) -> Result<()> {
    let src = dir
        .canonicalize()
        .context(|| format!("no such bot directory: {}", dir.display()))?;
    let stage = tempfile::Builder::new()
        .prefix("kfm-build-")
        .tempdir()
        .context(|| "cannot make a staging directory".to_owned())?;
    // builder (uid 10001) writes into it; the module is copied out after.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(stage.path(), std::fs::Permissions::from_mode(0o777))
            .context(|| "cannot open the staging directory to the build user".to_owned())?;
    }
    let engine = docker();
    let args = sandbox_args(&src, stage.path(), image);
    eprintln!("+ {engine} {}", args.join(" "));
    let mut child = Command::new(&engine)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                other(format!(
                    "{engine} is not installed or not on PATH: install Docker, or build with a \
                     local clang (kfm build --local)"
                ))
            } else {
                other(format!("cannot run {engine}: {e}"))
            }
        })?;
    let log = Arc::new(Mutex::new(Vec::new()));
    let a = tee(child.stdout.take().expect("piped"), log.clone());
    let b = tee(child.stderr.take().expect("piped"), log.clone());
    let status = child.wait().context(|| format!("cannot run {engine}"))?;
    let _ = a.join();
    let _ = b.join();
    if let Some(parent) = log_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(log_path, &*log.lock().expect("the log lock"))
        .context(|| format!("cannot write {}", log_path.display()))?;
    if !status.success() {
        let limit = if status.code() == Some(124) {
            ": the build hit the 120 s limit"
        } else {
            ""
        };
        return Err(other(format!(
            "the build failed ({status}){limit}; log in {}",
            log_path.display()
        )));
    }
    let built = stage.path().join("bot.wasm");
    if !built.is_file() {
        return Err(other("the build left no bot.wasm"));
    }
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::copy(&built, out).context(|| format!("cannot write {}", out.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_c_command_is_the_recipe_byte_for_byte() {
        let cmd = command(
            Path::new("/e/clang"),
            Path::new("/sdk"),
            Path::new("out.wasm"),
            &[PathBuf::from("bot/a.c"), PathBuf::from("bot/b.c")],
            &[],
        );
        assert_eq!(
            cmd.join(" "),
            "/e/clang --target=wasm32 -mcpu=mvp -mmutable-globals --no-wasm-opt -O2 -nostdlib \
             -ffreestanding -Wall -Wextra -Werror -I /sdk/c -I /sdk/cpp -Wl,--no-entry \
             -Wl,--export=kfm_abi_version -Wl,--export=kfm_obs_buffer -Wl,--export=kfm_init \
             -Wl,--export=kfm_act -Wl,--export=__stack_pointer -Wl,--allow-undefined \
             -Wl,-z,stack-size=65536 -Wl,--strip-all -std=c11 -o out.wasm bot/a.c bot/b.c"
        );
    }

    #[test]
    fn a_cpp_bot_compiles_every_file_as_cpp20() {
        let cmd = command(
            Path::new("/nowhere/clang"),
            Path::new("/sdk"),
            Path::new("o.wasm"),
            &[PathBuf::from("d/x.c")],
            &[PathBuf::from("d/a.cpp")],
        );
        let text = cmd.join(" ");
        assert!(
            text.starts_with("/nowhere/clang --driver-mode=g++ --target=wasm32"),
            "{text}"
        );
        assert!(
            text.ends_with(
                "-Wl,--strip-all -std=c++20 -fno-exceptions -fno-rtti -fno-threadsafe-statics \
                 -o o.wasm -x c++ d/x.c d/a.cpp"
            ),
            "{text}"
        );
    }

    #[test]
    fn sources_are_the_direct_files_in_byte_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.cpp", "Z.c", "a.cc", "notes.txt", "x.h"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inner.c"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("b.cpp", dir.path().join("link.cpp")).unwrap();
        let (c, cxx) = sources(dir.path()).unwrap();
        let names = |v: &[PathBuf]| {
            v.iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&c), ["Z.c"]);
        assert_eq!(names(&cxx), ["a.cc", "b.cpp"]);
    }

    #[test]
    fn the_sandbox_flags_are_the_architectures() {
        let args = sandbox_args(Path::new("/s"), Path::new("/o"), "img:v1");
        assert_eq!(
            args.join(" "),
            "run --rm --network none --memory 2g --cpus 1 --pids-limit 256 --read-only \
             --tmpfs /tmp --user builder -v /s:/src:ro -v /o:/out img:v1 timeout 120 kfm-build"
        );
    }

    #[test]
    fn the_image_is_pinned_to_this_release() {
        assert!(default_image().starts_with("ghcr.io/minip8/kfm-bot-build:v"));
    }

    #[test]
    fn the_embedded_sdk_is_the_repositorys() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../sdk");
        assert_eq!(std::fs::read(repo.join("c/kfm_bot.h")).unwrap(), SDK_C);
        assert_eq!(
            std::fs::read(repo.join("cpp/kfm_bot.hpp")).unwrap(),
            SDK_CPP
        );
    }
}
