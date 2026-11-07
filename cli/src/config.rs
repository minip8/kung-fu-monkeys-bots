//! The configuration file and where things live (cli/README.md
//! "Configuration"): `config.toml` in the platform's config directory
//! (`~/.config/kfm/` on Linux, `~/Library/Application Support/kfm/` on macOS,
//! `%APPDATA%\kfm\config\` on Windows), written with mode 0600; the cache
//! (downloaded `kfm_arena` runners, the SDK for `kfm build --local`) in the
//! platform's cache directory. `KFM_CONFIG` names another file and
//! `KFM_CACHE_DIR` another cache.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{other, Context, Result};

pub const DEFAULT_SERVER: &str = "https://bots.kfm.minip8.com";

/// What `config.toml` holds. Every key is optional; unknown keys are kept
/// out (a newer CLI's keys are dropped on the next write).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The ladder's origin; the default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The API token `kfm login` stored, for `server`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Its id, so `kfm logout` can revoke it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    /// Its name on the portal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_name: Option<String>,
}

fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "kfm")
        .ok_or_else(|| other("no home directory to keep kfm's configuration in"))
}

/// The configuration file: `$KFM_CONFIG`, else `<config dir>/config.toml`.
pub fn config_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KFM_CONFIG").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

/// The cache directory: `$KFM_CACHE_DIR`, else the platform's.
pub fn cache_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KFM_CACHE_DIR").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(project_dirs()?.cache_dir().to_path_buf())
}

/// A server URL without its trailing slashes.
pub fn normalize_server(server: &str) -> String {
    server.trim().trim_end_matches('/').to_owned()
}

impl Config {
    /// The file at `path`; an empty configuration when there is none.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| {
                other(format!(
                    "{} is not a kfm configuration: {e}",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(other(format!("cannot read {}: {e}", path.display()))),
        }
    }

    /// Writes the file readable by its owner only (0600 on Unix), replacing
    /// it in one rename so a crash never leaves half a token.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
        if let Some(dir) = dir {
            std::fs::create_dir_all(dir).context(|| format!("cannot create {}", dir.display()))?;
        }
        let text = format!(
            "# kfm's configuration (cli/README.md \"Configuration\"); `kfm login` writes it.\n{}",
            toml::to_string(self).map_err(other)?
        );
        let tmp = path.with_extension("toml.tmp");
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&tmp)
                .context(|| format!("cannot write {}", tmp.display()))?;
            #[cfg(unix)]
            {
                // A file left by an older write keeps its mode through open().
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .context(|| format!("cannot restrict {}", tmp.display()))?;
            }
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
                .context(|| format!("cannot write {}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path).context(|| format!("cannot write {}", path.display()))
    }

    /// The configured server, normalised, or the default.
    pub fn server_or_default(&self) -> String {
        normalize_server(self.server.as_deref().unwrap_or(DEFAULT_SERVER))
    }
}

/// The server and token a command uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub server: String,
    pub token: Option<String>,
    /// The token's id when it is the file's (not `KFM_TOKEN`'s).
    pub token_id: Option<String>,
    /// Where the token came from, for messages.
    pub token_from_env: bool,
}

/// `--server` (or `KFM_SERVER`, which clap folds into the flag) over the
/// file's `server` over the default; `KFM_TOKEN` over the file's token. The
/// file's token is used only for the file's server: a token is never sent to
/// another origin than the one it was made for.
pub fn resolve(server: Option<&str>, env_token: Option<String>, file: &Config) -> Effective {
    let configured = file.server_or_default();
    let server = server
        .map(normalize_server)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| configured.clone());
    if let Some(token) = env_token.filter(|t| !t.trim().is_empty()) {
        return Effective {
            server,
            token: Some(token.trim().to_owned()),
            token_id: None,
            token_from_env: true,
        };
    }
    let same = server == configured;
    Effective {
        token: file.token.clone().filter(|_| same),
        token_id: file.token_id.clone().filter(|_| same),
        server,
        token_from_env: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored() -> Config {
        Config {
            server: Some("https://ladder.example.org/".into()),
            token: Some("kfm_secret".into()),
            token_id: Some("id-1".into()),
            token_name: Some("kfm on box".into()),
        }
    }

    #[test]
    fn the_file_round_trips_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        stored().save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), stored());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Rewriting a file someone loosened tightens it again.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            Config::default().save(&path).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(Config::load(&path).unwrap(), Config::default());
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = [").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn the_server_defaults_and_flags_win() {
        let none = resolve(None, None, &Config::default());
        assert_eq!(none.server, DEFAULT_SERVER);
        assert_eq!(none.token, None);
        let file = resolve(None, None, &stored());
        assert_eq!(file.server, "https://ladder.example.org");
        assert_eq!(file.token.as_deref(), Some("kfm_secret"));
        assert_eq!(file.token_id.as_deref(), Some("id-1"));
        let same = resolve(Some("https://ladder.example.org"), None, &stored());
        assert_eq!(same.token.as_deref(), Some("kfm_secret"));
    }

    #[test]
    fn a_token_never_goes_to_another_server() {
        let elsewhere = resolve(Some("http://127.0.0.1:8080/"), None, &stored());
        assert_eq!(elsewhere.server, "http://127.0.0.1:8080");
        assert_eq!(elsewhere.token, None);
        assert_eq!(elsewhere.token_id, None);
    }

    #[test]
    fn kfm_token_overrides_the_file() {
        let env = resolve(None, Some(" kfm_env ".into()), &stored());
        assert_eq!(env.token.as_deref(), Some("kfm_env"));
        assert_eq!(env.token_id, None);
        assert!(env.token_from_env);
        let elsewhere = resolve(Some("http://x"), Some("kfm_env".into()), &stored());
        assert_eq!(elsewhere.token.as_deref(), Some("kfm_env"));
        assert!(resolve(None, Some("  ".into()), &stored()).token.as_deref() == Some("kfm_secret"));
    }
}
