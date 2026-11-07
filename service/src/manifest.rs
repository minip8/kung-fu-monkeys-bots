//! A submission's `manifest.toml` (`docs/ladder-api.md` §9.1):
//!
//! ```toml
//! language = "cpp"     # the only ladder language; "c" is an alias that builds the same way
//! entry = "bots/mine"  # the directory inside source.zip holding the sources; default "."
//! ```
//!
//! Unknown keys are refused, so a typo never silently means the default.

use serde::Deserialize;

/// The largest manifest accepted, in bytes.
pub const MAX_MANIFEST_BYTES: usize = 4096;
/// The longest entry path, in bytes.
pub const MAX_ENTRY_BYTES: usize = 255;

/// The ladder language. `c` is accepted as an alias: the recipe
/// (tools/build.sh) compiles a directory of only `.c` files as C11 and one
/// with any C++ source as C++20 either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Cpp,
}

impl Language {
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Cpp => "cpp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub language: Language,
    /// The entry directory, normalised: components joined by `/`, no `.`,
    /// no leading or trailing `/`; empty for the archive's root.
    pub entry: String,
}

impl Manifest {
    /// The entry as shown to people: `.` for the root.
    pub fn entry_display(&self) -> &str {
        if self.entry.is_empty() {
            "."
        } else {
            &self.entry
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    language: String,
    entry: Option<String>,
}

/// Parses a manifest; the error is a sentence naming what is wrong.
pub fn parse(text: &str) -> Result<Manifest, String> {
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(format!("manifest.toml is over {MAX_MANIFEST_BYTES} bytes"));
    }
    let raw: Raw = toml::from_str(text).map_err(|e| {
        format!(
            "manifest.toml: {}",
            e.message().trim_end_matches('\n').replace('\n', " ")
        )
    })?;
    let language = match raw.language.as_str() {
        "cpp" | "c" => Language::Cpp,
        other => {
            return Err(format!(
                "manifest.toml: language {other:?} is not a ladder language (\"cpp\")"
            ))
        }
    };
    let entry = normalise_entry(raw.entry.as_deref().unwrap_or("."))?;
    Ok(Manifest { language, entry })
}

/// The manifest a portal form's fields stand for (the same rules as a file).
pub fn from_fields(language: &str, entry: &str) -> String {
    let entry = if entry.trim().is_empty() {
        "."
    } else {
        entry.trim()
    };
    format!(
        "language = {}\nentry = {}\n",
        toml_string(language.trim()),
        toml_string(entry)
    )
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn normalise_entry(entry: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("manifest.toml: entry {entry:?} {why}"));
    if entry.len() > MAX_ENTRY_BYTES {
        return bad("is too long");
    }
    if entry.starts_with('/') || entry.contains('\\') || entry.contains(':') {
        return bad("must be a relative path with / separators");
    }
    if entry.chars().any(char::is_control) {
        return bad("holds a control character");
    }
    let mut parts = Vec::new();
    for part in entry.split('/') {
        match part {
            "" | "." => {}
            ".." => return bad("must not contain .."),
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_manifest_parses() {
        let m = parse("language = \"cpp\"\nentry = \"bots/mine/\"\n").unwrap();
        assert_eq!(m.language, Language::Cpp);
        assert_eq!(m.entry, "bots/mine");
        assert_eq!(m.entry_display(), "bots/mine");
    }

    #[test]
    fn entry_defaults_to_the_root_and_c_is_an_alias() {
        let m = parse("language = \"c\"").unwrap();
        assert_eq!(m.language, Language::Cpp);
        assert_eq!(m.entry, "");
        assert_eq!(m.entry_display(), ".");
        assert_eq!(
            parse("language = \"cpp\"\nentry = \"./src/.\"")
                .unwrap()
                .entry,
            "src"
        );
    }

    #[test]
    fn bad_manifests_are_refused_with_a_reason() {
        for (text, needle) in [
            ("", "language"),
            ("language = \"rust\"", "not a ladder language"),
            ("language = 3", "expected a string"),
            ("language = \"cpp\"\nentrypoint = \"x\"", "entrypoint"),
            ("language = \"cpp\"\nentry = \"/abs\"", "relative"),
            ("language = \"cpp\"\nentry = \"a/../b\"", ".."),
            ("language = \"cpp\"\nentry = \"a\\\\b\"", "relative"),
            ("language = \"cpp\"\nentry = \"C:/x\"", "relative"),
            ("not toml at all", "manifest.toml"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(err.contains(needle), "{text:?}: {err}");
        }
        assert!(parse(&format!("language = \"cpp\"\n#{}", "x".repeat(5000))).is_err());
    }

    #[test]
    fn form_fields_make_a_manifest_that_parses() {
        let text = from_fields("cpp", " bots/\"odd\" ");
        let m = parse(&text).unwrap();
        assert_eq!(m.entry, "bots/\"odd\"");
        assert_eq!(parse(&from_fields("c", "")).unwrap().entry, "");
    }
}
