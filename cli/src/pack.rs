//! `kfm submit`'s archive (docs/ladder-api.md §9.1, §9.2): a bot directory
//! zipped as the ladder takes it, refused here first when the ladder would
//! refuse it.
//!
//! Regular files only, recursively, by relative path with `/`, in byte
//! order, with a fixed timestamp and mode (the same directory zips to the
//! same bytes). Skipped: the directories `.git`, `out` and `.kfm` at any
//! depth (the VCS, `kfm build`'s output, the tools' downloads), any `*.wasm`
//! file (the ladder builds from source and never takes a module), and
//! symbolic links and other non-regular files (the ladder refuses them).

use std::io::Write;
use std::path::{Component, Path};

use crate::error::{other, Context, Error, Result};

/// The ladder's caps (§9.2).
pub const MAX_ZIP_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FILES: usize = 256;
pub const MAX_UNPACKED_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_NAME_BYTES: usize = 255;
/// Directories never packed.
pub const SKIPPED_DIRS: [&str; 3] = [".git", "out", ".kfm"];
/// The sources the recipe compiles.
pub const SOURCE_EXTENSIONS: [&str; 3] = ["c", "cc", "cpp"];

/// A packed directory.
#[derive(Debug)]
pub struct Packed {
    pub zip: Vec<u8>,
    /// The files in the archive, in order.
    pub files: Vec<String>,
    /// What was left out and why (`out/ (directory)`, `bot.wasm`, `link (not a regular file)`).
    pub skipped: Vec<String>,
    /// The files' total size.
    pub unpacked: u64,
}

/// Whether `name` is something the recipe compiles.
pub fn is_source(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SOURCE_EXTENSIONS.contains(&e))
}

fn check_name(name: &str) -> Result<()> {
    if name.len() > MAX_NAME_BYTES {
        return Err(other(format!(
            "{name:?} is a path of {} bytes, over the ladder's {MAX_NAME_BYTES}",
            name.len()
        )));
    }
    if name
        .chars()
        .any(|c| c.is_control() || c == '\\' || c == ':')
    {
        return Err(other(format!(
            "{name:?} has a character the ladder refuses in a name (a control character, \\ or :)"
        )));
    }
    Ok(())
}

/// Every file under `dir` to pack, as (name, size), and what was skipped.
fn walk(
    dir: &Path,
    prefix: &str,
    files: &mut Vec<(String, u64)>,
    skipped: &mut Vec<String>,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .context(|| format!("cannot read {}", dir.display()))?
        .collect::<std::io::Result<_>>()
        .context(|| format!("cannot read {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let raw = entry.file_name();
        let name = raw.to_str().ok_or_else(|| {
            other(format!(
                "{} has a name that is not UTF-8",
                entry.path().display()
            ))
        })?;
        let rel = format!("{prefix}{name}");
        // symlink_metadata: a link is judged as a link, never followed.
        let meta = std::fs::symlink_metadata(entry.path())
            .context(|| format!("cannot read {}", entry.path().display()))?;
        if meta.is_dir() {
            if SKIPPED_DIRS.contains(&name) {
                skipped.push(format!("{rel}/ (directory)"));
            } else {
                walk(&entry.path(), &format!("{rel}/"), files, skipped)?;
            }
        } else if !meta.is_file() {
            skipped.push(format!("{rel} (not a regular file)"));
        } else if name.ends_with(".wasm") {
            skipped.push(format!("{rel} (a module: the ladder builds from source)"));
        } else {
            check_name(&rel)?;
            files.push((rel, meta.len()));
        }
    }
    Ok(())
}

/// Zips `dir` by the rules above, or says which rule of the ladder's it breaks.
pub fn pack(dir: &Path) -> Result<Packed> {
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    walk(dir, "", &mut found, &mut skipped)?;
    // Byte order of the whole relative path, as the recipe sorts.
    found.sort();
    if found.is_empty() {
        return Err(other(format!("{} has no file to submit", dir.display())));
    }
    if found.len() > MAX_FILES {
        return Err(other(format!(
            "{} has {} files, over the ladder's {MAX_FILES}",
            dir.display(),
            found.len()
        )));
    }
    let unpacked: u64 = found.iter().map(|(_, size)| size).sum();
    if unpacked > MAX_UNPACKED_BYTES {
        return Err(other(format!(
            "the files add up to {unpacked} bytes, over the ladder's {MAX_UNPACKED_BYTES} (16 MiB)"
        )));
    }
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    for (name, _) in &found {
        let data = std::fs::read(dir.join(name)).context(|| format!("cannot read {name}"))?;
        writer.start_file(name.as_str(), options).map_err(other)?;
        writer.write_all(&data).map_err(other)?;
    }
    let zip = writer.finish().map_err(other)?.into_inner();
    if zip.len() > MAX_ZIP_BYTES {
        return Err(other(format!(
            "the source zip is {} bytes, over the ladder's {MAX_ZIP_BYTES} (4 MiB); \
             leave data files out of the bot's directory",
            zip.len()
        )));
    }
    Ok(Packed {
        zip,
        files: found.into_iter().map(|(n, _)| n).collect(),
        skipped,
        unpacked,
    })
}

/// The manifest's `entry` normalised as the ladder does (§9.1): `.` and empty
/// components dropped, `.` for the root; a usage error for what it refuses.
pub fn normalize_entry(entry: &str) -> Result<String> {
    let refuse = |why: &str| Error::Usage(format!("--entry {entry:?}: {why}"));
    if entry.len() > MAX_NAME_BYTES {
        return Err(refuse("longer than 255 bytes"));
    }
    if entry.starts_with('/') || entry.contains('\\') || entry.contains(':') {
        return Err(refuse("must be a relative path with / separators"));
    }
    if entry.chars().any(char::is_control) {
        return Err(refuse("has a control character"));
    }
    let mut parts = Vec::new();
    for c in Path::new(entry).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return Err(refuse("may not contain ..")),
        }
    }
    Ok(if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    })
}

/// Whether the entry directory holds a source directly (the ladder's rule,
/// §9.1: the recipe compiles only the files directly in it).
pub fn check_entry(files: &[String], entry: &str) -> Result<()> {
    let prefix = if entry == "." {
        String::new()
    } else {
        format!("{entry}/")
    };
    let has = files.iter().any(|f| {
        f.strip_prefix(&prefix)
            .is_some_and(|rest| !rest.contains('/') && is_source(rest))
    });
    if has {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "no .c, .cc or .cpp file directly in the entry directory {entry:?} \
             (the recipe compiles only those; pass --entry SUBDIR)"
        )))
    }
}

/// `manifest.toml` (§9.1).
pub fn manifest(language: &str, entry: &str) -> String {
    #[derive(serde::Serialize)]
    struct Manifest<'a> {
        language: &'a str,
        entry: &'a str,
    }
    toml::to_string(&Manifest { language, entry }).expect("two strings serialise")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn names_in(zip: &[u8]) -> Vec<String> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect()
    }

    #[test]
    fn only_sources_are_packed_in_byte_order() {
        let dir = tree(&[
            ("bot.cpp", "int x;"),
            ("Zeta.h", "//"),
            ("lib/util.h", "//"),
            ("out/bot.wasm", "x"),
            ("out/notes.txt", "x"),
            (".git/config", "x"),
            ("deep/.kfm/kfm_arena", "x"),
            ("deep/out/x.c", "x"),
            ("old.wasm", "x"),
            (".clang-format", "x"),
        ]);
        #[cfg(unix)]
        std::os::unix::fs::symlink("bot.cpp", dir.path().join("link.cpp")).unwrap();
        let packed = pack(dir.path()).unwrap();
        assert_eq!(
            packed.files,
            [".clang-format", "Zeta.h", "bot.cpp", "lib/util.h"]
        );
        assert_eq!(names_in(&packed.zip), packed.files);
        assert!(packed.skipped.iter().any(|s| s == ".git/ (directory)"));
        assert!(packed.skipped.iter().any(|s| s == "out/ (directory)"));
        assert!(packed.skipped.iter().any(|s| s == "deep/.kfm/ (directory)"));
        assert!(packed.skipped.iter().any(|s| s.starts_with("old.wasm")));
        #[cfg(unix)]
        assert!(packed.skipped.iter().any(|s| s.starts_with("link.cpp")));
        assert_eq!(packed.unpacked, 6 + 2 + 2 + 1);
    }

    #[test]
    fn the_same_tree_packs_to_the_same_bytes() {
        let dir = tree(&[("bot.c", "int x;"), ("a/b.h", "//")]);
        let one = pack(dir.path()).unwrap().zip;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.path().join("bot.c"), "int x;").unwrap();
        assert_eq!(pack(dir.path()).unwrap().zip, one);
    }

    #[test]
    fn caps_are_enforced_before_upload() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..=MAX_FILES {
            std::fs::write(dir.path().join(format!("f{i}.h")), "x").unwrap();
        }
        assert!(pack(dir.path())
            .unwrap_err()
            .to_string()
            .contains("over the ladder's 256"));

        // Random bytes do not compress: past 4 MiB zipped.
        let dir = tempfile::tempdir().unwrap();
        let mut noise = vec![0u8; MAX_ZIP_BYTES + 1024];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for b in &mut noise {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        std::fs::write(dir.path().join("data.bin"), noise).unwrap();
        assert!(pack(dir.path()).unwrap_err().to_string().contains("4 MiB"));

        let empty = tree(&[("out/bot.wasm", "x")]);
        assert!(pack(empty.path())
            .unwrap_err()
            .to_string()
            .contains("no file"));
    }

    #[test]
    fn names_the_ladder_refuses_are_refused() {
        let dir = tree(&[("a:b.c", "x")]);
        assert!(pack(dir.path()).is_err());
    }

    #[test]
    fn entries_are_normalised_and_checked() {
        assert_eq!(normalize_entry(".").unwrap(), ".");
        assert_eq!(normalize_entry("").unwrap(), ".");
        assert_eq!(normalize_entry("./src/").unwrap(), "src");
        assert_eq!(normalize_entry("bots//mine").unwrap(), "bots/mine");
        assert!(normalize_entry("../x").is_err());
        assert!(normalize_entry("/abs").is_err());
        assert!(normalize_entry("a\\b").is_err());
        let files = vec![
            "bot.cpp".to_owned(),
            "src/a.c".to_owned(),
            "src/x/b.cc".to_owned(),
        ];
        assert!(check_entry(&files, ".").is_ok());
        assert!(check_entry(&files, "src").is_ok());
        assert!(check_entry(&files, "src/x").is_ok());
        let err = check_entry(&["lib/a.h".to_owned(), "src/x/b.cc".to_owned()], "src").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn the_manifest_is_toml() {
        assert_eq!(manifest("cpp", "."), "language = \"cpp\"\nentry = \".\"\n");
        let parsed: toml::Table = toml::from_str(&manifest("cpp", "my \"bot\"")).unwrap();
        assert_eq!(parsed["entry"].as_str(), Some("my \"bot\""));
    }
}
