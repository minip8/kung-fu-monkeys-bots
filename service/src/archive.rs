//! A submission's `source.zip`, treated as hostile (deploy/README.md "What
//! gVisor protects against": the worker that unpacks it runs outside the
//! sandbox). The same walk checks an upload, before anything is stored, and
//! unpacks the entry directory on the build worker:
//!
//! - at most [`MAX_ZIP_BYTES`] compressed (the route checks that first);
//! - no absolute path, drive letter, backslash, `..` component or NUL;
//! - nothing but regular files and directories (no symlink, device or fifo),
//!   nothing encrypted, no name twice, no file where a directory is;
//! - at most [`MAX_FILES`] files and [`MAX_UNPACKED_BYTES`] unpacked, counted
//!   while decompressing (a header's sizes may lie);
//!
//! and stops at the first violation with a sentence naming it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// The largest source archive, compressed (architecture §11: "at most 4 MiB of source").
pub const MAX_ZIP_BYTES: usize = 4 * 1024 * 1024;
/// The most regular files an archive may hold.
pub const MAX_FILES: usize = 256;
/// The most entries of any kind (files and directories).
pub const MAX_ENTRIES: usize = 1024;
/// The most bytes an archive may unpack to.
pub const MAX_UNPACKED_BYTES: u64 = 16 * 1024 * 1024;
/// The longest path inside an archive, in bytes.
pub const MAX_PATH_BYTES: usize = 255;

/// What an archive holds, once checked: its files by normalised path.
#[derive(Debug, Default)]
pub struct Contents {
    pub files: BTreeMap<String, Vec<u8>>,
}

impl Contents {
    /// The total unpacked size.
    pub fn bytes(&self) -> u64 {
        self.files.values().map(|f| f.len() as u64).sum()
    }

    /// The files under `entry` (a normalised directory, empty for the root),
    /// by their path relative to it.
    pub fn under<'a>(&'a self, entry: &'a str) -> impl Iterator<Item = (&'a str, &'a [u8])> + 'a {
        self.files.iter().filter_map(move |(path, data)| {
            let rel = if entry.is_empty() {
                Some(path.as_str())
            } else {
                path.strip_prefix(entry).and_then(|r| r.strip_prefix('/'))
            };
            rel.map(|r| (r, data.as_slice()))
        })
    }

    /// Checks that `entry` holds at least one `.c`, `.cc` or `.cpp` file
    /// directly (what tools/build.sh compiles).
    pub fn check_entry(&self, entry: &str) -> Result<(), String> {
        let shown = if entry.is_empty() { "." } else { entry };
        let any_source = self.under(entry).any(|(rel, _)| {
            !rel.contains('/')
                && [".c", ".cc", ".cpp"]
                    .iter()
                    .any(|ext| rel.len() > ext.len() && rel.ends_with(ext))
        });
        if any_source {
            Ok(())
        } else {
            Err(format!(
                "the entry directory {shown:?} holds no .c, .cc or .cpp file"
            ))
        }
    }
}

/// A path inside the archive as `/`-joined components with no `.`; refused
/// when it could leave the directory it is unpacked into.
fn normalise(name: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("{name:?} {why}"));
    if name.is_empty() {
        return bad("is an empty name");
    }
    if name.len() > MAX_PATH_BYTES {
        return bad(&format!("is longer than {MAX_PATH_BYTES} bytes"));
    }
    if name.starts_with('/') {
        return bad("is an absolute path");
    }
    if name.contains('\\') {
        return bad("contains a backslash");
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return bad("is an absolute path (a drive letter)");
    }
    if name.chars().any(char::is_control) {
        return bad("contains a control character");
    }
    let mut parts = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => return bad("contains a .. component"),
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return bad("names no file");
    }
    Ok(parts.join("/"))
}

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;

/// Reads and checks every entry of `bytes`; the first violation is the error.
pub fn check(bytes: &[u8]) -> Result<Contents, String> {
    if bytes.len() > MAX_ZIP_BYTES {
        return Err(format!(
            "source.zip is {} bytes, over the {} byte cap",
            bytes.len(),
            MAX_ZIP_BYTES
        ));
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("source.zip is not a zip archive ({e})"))?;
    if archive.len() > MAX_ENTRIES {
        return Err(format!(
            "source.zip has {} entries, over the {MAX_ENTRIES} cap",
            archive.len()
        ));
    }
    let mut contents = Contents::default();
    let mut dirs = BTreeSet::new();
    let mut total: u64 = 0;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|e| format!("entry {index} cannot be read ({e})"))?;
        let raw_name = file
            .name()
            .map_err(|e| format!("entry {index} has an unreadable name ({e})"))?
            .into_owned();
        let path = normalise(&raw_name)?;
        if file.encrypted() {
            return Err(format!("{raw_name:?} is encrypted"));
        }
        if file.is_symlink() {
            return Err(format!("{raw_name:?} is a symbolic link"));
        }
        let mode = file.unix_mode().map(|m| m & S_IFMT);
        if file.is_dir() {
            if !matches!(mode, None | Some(0) | Some(S_IFDIR)) {
                return Err(format!("{raw_name:?} is not a regular file"));
            }
            dirs.insert(path);
            continue;
        }
        if !matches!(mode, None | Some(0) | Some(S_IFREG)) {
            return Err(format!("{raw_name:?} is not a regular file"));
        }
        if contents.files.len() >= MAX_FILES {
            return Err(format!("source.zip has more than {MAX_FILES} files"));
        }
        let room = MAX_UNPACKED_BYTES - total;
        let mut data = Vec::new();
        (&mut file)
            .take(room + 1)
            .read_to_end(&mut data)
            .map_err(|e| format!("{raw_name:?} cannot be unpacked ({e})"))?;
        if data.len() as u64 > room {
            return Err(format!(
                "source.zip unpacks to more than {MAX_UNPACKED_BYTES} bytes"
            ));
        }
        total += data.len() as u64;
        if contents.files.insert(path, data).is_some() {
            return Err(format!("{raw_name:?} appears twice"));
        }
    }
    // A file may not stand where another entry needs a directory.
    for path in contents.files.keys() {
        let mut prefix = path.as_str();
        while let Some((parent, _)) = prefix.rsplit_once('/') {
            if contents.files.contains_key(parent) {
                return Err(format!("{parent:?} is a file and a directory"));
            }
            prefix = parent;
        }
        if dirs.contains(path) {
            return Err(format!("{path:?} is a file and a directory"));
        }
    }
    Ok(contents)
}

/// Checks `bytes` again and writes the files under `entry` into `dest` (an
/// empty directory), by their path relative to `entry`: directories 0755,
/// files 0644, created fresh (never through an existing path). Returns the
/// number of files written.
pub fn unpack(bytes: &[u8], entry: &str, dest: &Path) -> Result<usize, String> {
    let contents = check(bytes)?;
    contents.check_entry(entry)?;
    let mut written = 0;
    for (rel, data) in contents.under(entry) {
        // Every component was checked by normalise(): no "..", no root.
        let target: PathBuf = rel.split('/').fold(dest.to_path_buf(), |p, c| p.join(c));
        if let Some(parent) = target.parent() {
            create_dirs(dest, parent).map_err(|e| format!("unpacking {rel:?}: {e}"))?;
        }
        write_new(&target, data).map_err(|e| format!("unpacking {rel:?}: {e}"))?;
        written += 1;
    }
    Ok(written)
}

fn create_dirs(root: &Path, dir: &Path) -> std::io::Result<()> {
    if dir == root || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_dirs(root, parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o755);
    }
    match builder.create(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        other => other,
    }
}

fn write_new(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    options.open(path)?.write_all(data)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    /// A zip of `(name, contents)` pairs; a name ending in `/` is a directory.
    pub(crate) fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, data) in files {
            if name.ends_with('/') {
                writer.add_directory(*name, options).unwrap();
            } else {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    fn refused(bytes: &[u8]) -> String {
        check(bytes).expect_err("the archive should be refused")
    }

    #[test]
    fn a_plain_tree_is_accepted() {
        let zip = zip_of(&[
            ("bot/", b""),
            ("bot/bot.cpp", b"int x;"),
            ("bot/inc/h.hpp", b"#pragma once"),
            ("./README.md", b"hi"),
        ]);
        let contents = check(&zip).unwrap();
        assert_eq!(
            contents.files.keys().collect::<Vec<_>>(),
            ["README.md", "bot/bot.cpp", "bot/inc/h.hpp"]
        );
        assert_eq!(contents.bytes(), 6 + 12 + 2);
        assert!(contents.check_entry("bot").is_ok());
        let err = contents.check_entry("").unwrap_err();
        assert!(err.contains("\".\" holds no"), "{err}");
        assert!(contents.check_entry("bot/inc").is_err());
        assert!(contents.check_entry("nope").is_err());
        let under: Vec<_> = contents.under("bot").map(|(r, _)| r).collect();
        assert_eq!(under, ["bot.cpp", "inc/h.hpp"]);
    }

    #[test]
    fn every_dangerous_name_is_refused() {
        for (name, needle) in [
            ("/etc/passwd", "absolute"),
            ("C:/x.c", "drive letter"),
            ("a\\b.c", "backslash"),
            ("../x.c", ".."),
            ("a/../../x.c", ".."),
            ("a/\u{1}.c", "control"),
        ] {
            let err = refused(&zip_of(&[(name, b"x")]));
            assert!(err.contains(needle), "{name}: {err}");
        }
        let long = format!("{}.c", "a".repeat(300));
        assert!(refused(&zip_of(&[(&long, b"x")])).contains("longer"));
    }

    #[test]
    fn symlinks_and_special_files_are_refused() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink("link.c", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        let zip = writer.finish().unwrap().into_inner();
        assert!(refused(&zip).contains("symbolic link"));

        // A fifo: the writer only sets permission bits, so patch the file
        // type (S_IFIFO) into the central directory's external attributes.
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().unix_permissions(0o644);
        writer.start_file("fifo.c", options).unwrap();
        let mut zip = writer.finish().unwrap().into_inner();
        let at = zip
            .windows(4)
            .position(|w| w == b"PK\x01\x02")
            .expect("a central directory header");
        zip[at + 40..at + 42].copy_from_slice(&0o010644u16.to_le_bytes());
        let err = refused(&zip);
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn counts_and_sizes_are_capped() {
        let names: Vec<String> = (0..=MAX_FILES).map(|i| format!("f{i}.c")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        assert!(refused(&zip_of(&files)).contains("more than 256 files"));
        assert!(check(&zip_of(&files[..MAX_FILES])).is_ok());

        // A bomb: 17 MiB of zeros compresses to a few KiB.
        let zeros = vec![0u8; 17 * 1024 * 1024];
        let zip = zip_of(&[("big.c", &zeros)]);
        assert!(zip.len() < 100_000);
        assert!(refused(&zip).contains("more than 16777216 bytes"));
        // Spread over several files too.
        let half = vec![0u8; 9 * 1024 * 1024];
        assert!(refused(&zip_of(&[("a.c", &half), ("b.c", &half)])).contains("more than"));

        let over = vec![0u8; MAX_ZIP_BYTES + 1];
        assert!(refused(&over).contains("over the 4194304 byte cap"));
        assert!(refused(b"PK not really").contains("not a zip"));
    }

    #[test]
    fn duplicates_and_file_directory_clashes_are_refused() {
        let err = refused(&zip_of(&[("a.c", b"1"), ("./a.c", b"2")]));
        assert!(err.contains("twice"), "{err}");
        let err = refused(&zip_of(&[("a", b"1"), ("a/b.c", b"2")]));
        assert!(err.contains("file and a directory"), "{err}");
    }

    #[test]
    fn unpack_writes_the_entry_only() {
        let dir = std::env::temp_dir().join(format!("kfm-unpack-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        let zip = zip_of(&[
            ("x/bot.c", b"int a;"),
            ("x/sub/h.h", b"#define B"),
            ("other.c", b"no"),
        ]);
        assert_eq!(unpack(&zip, "x", &dir).unwrap(), 2);
        assert_eq!(std::fs::read(dir.join("bot.c")).unwrap(), b"int a;");
        assert_eq!(std::fs::read(dir.join("sub/h.h")).unwrap(), b"#define B");
        assert!(!dir.join("other.c").exists());
        // An entry without sources is refused before anything is written.
        let empty = dir.join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(unpack(&zip, "x/sub", &empty).is_err());
        assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
