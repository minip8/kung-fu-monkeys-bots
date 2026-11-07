//! Configuration: environment variables only, never a file in the image
//! (architecture v0.63 §11 "M7 step 3 design"). `docs/ladder-api.md` §9 and
//! `service/README.md` list every variable.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use url::Url;

/// The default listen address: Caddy terminates TLS in front of it.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8090";
pub const DEFAULT_GITHUB_AUTH_URL: &str = "https://github.com/login/oauth/authorize";
pub const DEFAULT_GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
pub const DEFAULT_GOOGLE_ISSUER: &str = "https://accounts.google.com";

/// A configuration error: the variable and what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub variable: &'static str,
    pub problem: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.variable, self.problem)
    }
}

impl std::error::Error for ConfigError {}

/// One OAuth application (client id and secret) registered with a provider.
#[derive(Clone)]
pub struct OAuthApp {
    pub client_id: String,
    pub client_secret: String,
}

/// The S3-compatible object store (Cloudflare R2 in production, MinIO in
/// tests). Read and validated now; used from step 3b.
#[derive(Clone)]
pub struct S3Config {
    pub endpoint: Url,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
}

/// Everything `kfm-ladder serve` runs with.
#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    /// The service's public origin, without a trailing slash
    /// (`https://ladder.kfm.minip8.com`): OAuth redirect URIs and absolute links.
    pub public_url: String,
    /// The HMAC-SHA256 key that signs the session and OAuth state cookies.
    pub session_key: [u8; 32],
    pub github: OAuthApp,
    pub google: OAuthApp,
    pub github_auth_url: Url,
    pub github_token_url: Url,
    /// GitHub's REST API base, without a trailing slash.
    pub github_api_url: String,
    /// Google's OIDC issuer (the discovery base), exactly as its id tokens name it.
    pub google_issuer: String,
    pub s3: S3Config,
    /// The engine release the season runs (`v0.5.1`).
    pub engine_tag: String,
    /// The `kfm_arena` binary (workers, 3b); optional for `serve`.
    pub arena: Option<PathBuf>,
    pub sentry_dsn: Option<String>,
}

impl Config {
    /// Whether cookies carry `Secure`: only when the public URL is https (a
    /// plain-http development or test origin would never get them back).
    pub fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<Config, ConfigError> {
        Config::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the configuration through `lookup` (the environment, or a map in
    /// tests). An empty value counts as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        let get = |name: &str| lookup(name).filter(|v| !v.is_empty());
        let required = |name: &'static str| {
            get(name).ok_or_else(|| ConfigError {
                variable: name,
                problem: "is required and not set".to_owned(),
            })
        };
        let url = |name: &'static str, value: String| {
            parse_http_url(&value).map_err(|problem| ConfigError {
                variable: name,
                problem,
            })
        };
        let optional_url = |name: &'static str, default: &str| {
            url(name, get(name).unwrap_or_else(|| default.to_owned()))
        };

        let database_url = database_url_from(&lookup)?;
        let listen_text = get("KFM_LADDER_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
        let listen = listen_text.parse().map_err(|_| ConfigError {
            variable: "KFM_LADDER_LISTEN",
            problem: format!("{listen_text:?} is not an address and port (127.0.0.1:8090)"),
        })?;

        let public = url("KFM_LADDER_PUBLIC_URL", required("KFM_LADDER_PUBLIC_URL")?)?;
        if public.path() != "/" || public.query().is_some() || public.fragment().is_some() {
            return Err(ConfigError {
                variable: "KFM_LADDER_PUBLIC_URL",
                problem: "must be an origin only (scheme, host and port, no path)".to_owned(),
            });
        }
        let public_url = public.as_str().trim_end_matches('/').to_owned();

        let session_key =
            parse_session_key(&required("KFM_LADDER_SESSION_KEY")?).map_err(|problem| {
                ConfigError {
                    variable: "KFM_LADDER_SESSION_KEY",
                    problem,
                }
            })?;

        let github = OAuthApp {
            client_id: required("KFM_LADDER_GITHUB_CLIENT_ID")?,
            client_secret: required("KFM_LADDER_GITHUB_CLIENT_SECRET")?,
        };
        let google = OAuthApp {
            client_id: required("KFM_LADDER_GOOGLE_CLIENT_ID")?,
            client_secret: required("KFM_LADDER_GOOGLE_CLIENT_SECRET")?,
        };
        let github_auth_url = optional_url("KFM_LADDER_GITHUB_AUTH_URL", DEFAULT_GITHUB_AUTH_URL)?;
        let github_token_url =
            optional_url("KFM_LADDER_GITHUB_TOKEN_URL", DEFAULT_GITHUB_TOKEN_URL)?;
        let github_api_url = optional_url("KFM_LADDER_GITHUB_API_URL", DEFAULT_GITHUB_API_URL)?
            .as_str()
            .trim_end_matches('/')
            .to_owned();
        // The issuer is compared byte for byte with the id token's `iss`:
        // keep it as written (Google's has no trailing slash).
        let google_issuer =
            get("KFM_LADDER_GOOGLE_ISSUER").unwrap_or_else(|| DEFAULT_GOOGLE_ISSUER.to_owned());
        url("KFM_LADDER_GOOGLE_ISSUER", google_issuer.clone())?;

        let s3 = s3_from(&get)?;
        let engine_tag = engine_tag_from(&get)?;

        Ok(Config {
            database_url,
            listen,
            public_url,
            session_key,
            github,
            google,
            github_auth_url,
            github_token_url,
            github_api_url,
            google_issuer,
            s3,
            engine_tag,
            arena: get("KFM_LADDER_ARENA").map(PathBuf::from),
            sentry_dsn: get("SENTRY_DSN"),
        })
    }
}

impl fmt::Debug for Config {
    // Secrets never reach a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("public_url", &self.public_url)
            .field("github_client_id", &self.github.client_id)
            .field("google_client_id", &self.google.client_id)
            .field("github_auth_url", &self.github_auth_url.as_str())
            .field("github_token_url", &self.github_token_url.as_str())
            .field("github_api_url", &self.github_api_url)
            .field("google_issuer", &self.google_issuer)
            .field("s3_endpoint", &self.s3.endpoint.as_str())
            .field("s3_bucket", &self.s3.bucket)
            .field("s3_region", &self.s3.region)
            .field("engine_tag", &self.engine_tag)
            .field("arena", &self.arena)
            .field("sentry", &self.sentry_dsn.is_some())
            .finish_non_exhaustive()
    }
}

/// What `kfm-ladder worker build|match` runs with (step 3b): never the
/// session key, the OAuth apps, the public URL or the listen address, so a
/// worker host holds no portal secret. `serve`'s [`Config`] is separate.
#[derive(Clone)]
pub struct WorkerConfig {
    pub database_url: String,
    pub s3: S3Config,
    pub engine_tag: String,
    /// The `kfm_arena` binary (validation and match jobs).
    pub arena: Option<PathBuf>,
    /// `KFM_BUILD_IMAGE`: the bot build image the build worker runs
    /// (`ghcr.io/minip8/kfm-bot-build:v0.1.0`).
    pub build_image: Option<String>,
    /// `KFM_LADDER_BUILD_DIR`: a directory at the same path on the host and in
    /// the worker's container, under which a build's source and output
    /// directories are made for the sibling `docker run`.
    pub build_dir: Option<PathBuf>,
    pub sentry_dsn: Option<String>,
}

impl WorkerConfig {
    pub fn from_env() -> Result<WorkerConfig, ConfigError> {
        WorkerConfig::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<WorkerConfig, ConfigError> {
        let get = |name: &str| lookup(name).filter(|v| !v.is_empty());
        Ok(WorkerConfig {
            database_url: database_url_from(&lookup)?,
            s3: s3_from(&get)?,
            engine_tag: engine_tag_from(&get)?,
            arena: get("KFM_LADDER_ARENA").map(PathBuf::from),
            build_image: get("KFM_BUILD_IMAGE"),
            build_dir: get("KFM_LADDER_BUILD_DIR").map(PathBuf::from),
            sentry_dsn: get("SENTRY_DSN"),
        })
    }
}

fn required_from(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, ConfigError> {
    get(name).ok_or_else(|| ConfigError {
        variable: name,
        problem: "is required and not set".to_owned(),
    })
}

fn s3_from(get: &impl Fn(&str) -> Option<String>) -> Result<S3Config, ConfigError> {
    let endpoint = required_from(get, "KFM_LADDER_S3_ENDPOINT")?;
    Ok(S3Config {
        endpoint: parse_http_url(&endpoint).map_err(|problem| ConfigError {
            variable: "KFM_LADDER_S3_ENDPOINT",
            problem,
        })?,
        bucket: required_from(get, "KFM_LADDER_S3_BUCKET")?,
        access_key: required_from(get, "KFM_LADDER_S3_ACCESS_KEY")?,
        secret_key: required_from(get, "KFM_LADDER_S3_SECRET_KEY")?,
        region: required_from(get, "KFM_LADDER_S3_REGION")?,
    })
}

fn engine_tag_from(get: &impl Fn(&str) -> Option<String>) -> Result<String, ConfigError> {
    let tag = required_from(get, "KFM_LADDER_ENGINE_TAG")?;
    if !is_release_tag(&tag) {
        return Err(ConfigError {
            variable: "KFM_LADDER_ENGINE_TAG",
            problem: format!("{tag:?} is not a release tag (v0.5.1)"),
        });
    }
    Ok(tag)
}

impl fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("s3_endpoint", &self.s3.endpoint.as_str())
            .field("s3_bucket", &self.s3.bucket)
            .field("engine_tag", &self.engine_tag)
            .field("arena", &self.arena)
            .field("build_image", &self.build_image)
            .field("build_dir", &self.build_dir)
            .field("sentry", &self.sentry_dsn.is_some())
            .finish_non_exhaustive()
    }
}

/// `DATABASE_URL` alone (what `kfm-ladder migrate` needs).
pub fn database_url_from(lookup: impl Fn(&str) -> Option<String>) -> Result<String, ConfigError> {
    let value = lookup("DATABASE_URL")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ConfigError {
            variable: "DATABASE_URL",
            problem: "is required and not set".to_owned(),
        })?;
    if !(value.starts_with("postgres://") || value.starts_with("postgresql://")) {
        return Err(ConfigError {
            variable: "DATABASE_URL",
            problem: "must be a postgres:// URL".to_owned(),
        });
    }
    Ok(value)
}

fn parse_http_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|e| format!("{value:?} is not a URL ({e})"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(format!("{value:?} is not an http(s) URL"));
    }
    if url.host().is_none() {
        return Err(format!("{value:?} has no host"));
    }
    Ok(url)
}

fn parse_session_key(value: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(value.trim()).map_err(|_| "must be 64 hex characters".to_owned())?;
    bytes
        .try_into()
        .map_err(|_| "must be 64 hex characters (32 bytes)".to_owned())
}

fn is_release_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn base() -> HashMap<&'static str, String> {
        [
            ("DATABASE_URL", "postgres://u:p@127.0.0.1/db"),
            ("KFM_LADDER_PUBLIC_URL", "https://ladder.example.org/"),
            ("KFM_LADDER_SESSION_KEY", &"ab".repeat(32)),
            ("KFM_LADDER_GITHUB_CLIENT_ID", "gh-id"),
            ("KFM_LADDER_GITHUB_CLIENT_SECRET", "gh-secret"),
            ("KFM_LADDER_GOOGLE_CLIENT_ID", "g-id"),
            ("KFM_LADDER_GOOGLE_CLIENT_SECRET", "g-secret"),
            ("KFM_LADDER_S3_ENDPOINT", "https://s3.example.org"),
            ("KFM_LADDER_S3_BUCKET", "kfm"),
            ("KFM_LADDER_S3_ACCESS_KEY", "ak"),
            ("KFM_LADDER_S3_SECRET_KEY", "sk"),
            ("KFM_LADDER_S3_REGION", "auto"),
            ("KFM_LADDER_ENGINE_TAG", "v0.5.1"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_owned()))
        .collect()
    }

    fn load(map: &HashMap<&'static str, String>) -> Result<Config, ConfigError> {
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn a_full_environment_loads_with_the_defaults() {
        let config = load(&base()).unwrap();
        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
        assert_eq!(config.public_url, "https://ladder.example.org");
        assert!(config.secure_cookies());
        assert_eq!(config.session_key, [0xab; 32]);
        assert_eq!(config.github_api_url, "https://api.github.com");
        assert_eq!(config.google_issuer, DEFAULT_GOOGLE_ISSUER);
        assert!(config.arena.is_none());
        assert!(config.sentry_dsn.is_none());
        assert!(!format!("{config:?}").contains("gh-secret"));
    }

    #[test]
    fn every_required_variable_is_named_when_missing() {
        for name in base().keys() {
            let mut map = base();
            map.remove(name);
            let err = load(&map).err().unwrap();
            assert_eq!(&err.variable, name);
            assert!(err.to_string().contains(name));
        }
    }

    #[test]
    fn a_worker_needs_no_portal_secret() {
        let mut map = base();
        for portal_only in [
            "KFM_LADDER_PUBLIC_URL",
            "KFM_LADDER_SESSION_KEY",
            "KFM_LADDER_GITHUB_CLIENT_ID",
            "KFM_LADDER_GITHUB_CLIENT_SECRET",
            "KFM_LADDER_GOOGLE_CLIENT_ID",
            "KFM_LADDER_GOOGLE_CLIENT_SECRET",
        ] {
            map.remove(portal_only);
        }
        map.insert("KFM_LADDER_LISTEN", "not an address".to_owned());
        map.insert(
            "KFM_BUILD_IMAGE",
            "ghcr.io/minip8/kfm-bot-build:v0.1.0".to_owned(),
        );
        map.insert(
            "KFM_LADDER_BUILD_DIR",
            "/var/lib/kfm-ladder/build".to_owned(),
        );
        let worker = WorkerConfig::from_lookup(|k| map.get(k).cloned()).unwrap();
        assert_eq!(worker.engine_tag, "v0.5.1");
        assert_eq!(
            worker.build_image.as_deref(),
            Some("ghcr.io/minip8/kfm-bot-build:v0.1.0")
        );
        assert_eq!(
            worker.build_dir.as_deref(),
            Some(std::path::Path::new("/var/lib/kfm-ladder/build"))
        );
        for needed in [
            "DATABASE_URL",
            "KFM_LADDER_S3_BUCKET",
            "KFM_LADDER_ENGINE_TAG",
        ] {
            let mut partial = map.clone();
            partial.remove(needed);
            let err = WorkerConfig::from_lookup(|k| partial.get(k).cloned())
                .err()
                .unwrap();
            assert_eq!(err.variable, needed);
        }
    }

    #[test]
    fn migrate_needs_only_the_database() {
        let url = database_url_from(|k| (k == "DATABASE_URL").then(|| "postgres://x/y".to_owned()));
        assert_eq!(url.unwrap(), "postgres://x/y");
    }

    #[test]
    fn bad_values_are_refused() {
        for (name, value) in [
            ("KFM_LADDER_SESSION_KEY", "abcd"),
            ("KFM_LADDER_SESSION_KEY", &"zz".repeat(32)),
            ("KFM_LADDER_PUBLIC_URL", "ladder.example.org"),
            ("KFM_LADDER_PUBLIC_URL", "https://ladder.example.org/portal"),
            ("KFM_LADDER_LISTEN", "localhost"),
            ("KFM_LADDER_ENGINE_TAG", "0.5.1"),
            ("KFM_LADDER_ENGINE_TAG", "v0.5"),
            ("DATABASE_URL", "mysql://x"),
            ("KFM_LADDER_GOOGLE_ISSUER", "ftp://x"),
        ] {
            let mut map = base();
            map.insert(name, value.to_owned());
            let err = load(&map).err().unwrap();
            assert_eq!(err.variable, name, "{value}");
        }
    }
}
