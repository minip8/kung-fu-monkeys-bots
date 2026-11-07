//! The object store: S3-compatible (Cloudflare R2 in production, MinIO in
//! development and tests; architecture v0.63 §11), addressed path-style
//! (`<endpoint>/<bucket>/<key>`), which both serve and which needs no DNS per
//! bucket. Keys: `sources/<submission>.zip`, `modules/<submission>.wasm`,
//! `logs/<submission>.txt` and (step 4) `replays/<match>.kfmr`.
//!
//! Built on `rust-s3` without its `fail-on-err` feature, so an HTTP status
//! comes back as data and this module decides what is an error (a missing
//! object on `get` is `None`, not a failure).

use std::fmt;
use std::time::Duration;

use s3::creds::Credentials;
use s3::{Bucket, BucketConfiguration, Region};
use uuid::Uuid;

use crate::config::S3Config;

/// How long one store request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub fn source_key(submission: Uuid) -> String {
    format!("sources/{submission}.zip")
}

pub fn module_key(submission: Uuid) -> String {
    format!("modules/{submission}.wasm")
}

pub fn log_key(submission: Uuid) -> String {
    format!("logs/{submission}.txt")
}

/// A store failure: the operation, the key and what went wrong. Logged,
/// never sent to a client.
#[derive(Debug, Clone)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "object store: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

/// One bucket of an S3-compatible store.
#[derive(Clone)]
pub struct Store {
    bucket: Box<Bucket>,
    config: S3Config,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("endpoint", &self.config.endpoint.as_str())
            .field("bucket", &self.config.bucket)
            .finish_non_exhaustive()
    }
}

fn region(config: &S3Config) -> Region {
    Region::Custom {
        region: config.region.clone(),
        endpoint: config.endpoint.as_str().trim_end_matches('/').to_owned(),
    }
}

fn credentials(config: &S3Config) -> Result<Credentials, StoreError> {
    Credentials::new(
        Some(&config.access_key),
        Some(&config.secret_key),
        None,
        None,
        None,
    )
    .map_err(|e| StoreError(format!("credentials: {e}")))
}

impl Store {
    /// A store over `config`'s bucket. Makes no request.
    pub fn new(config: &S3Config) -> Result<Store, StoreError> {
        let bucket = Bucket::new(&config.bucket, region(config), credentials(config)?)
            .map_err(|e| StoreError(format!("client: {e}")))?
            .with_path_style()
            .with_request_timeout(REQUEST_TIMEOUT)
            .map_err(|e| StoreError(format!("client: {e}")))?;
        Ok(Store {
            bucket,
            config: config.clone(),
        })
    }

    /// Creates the bucket if it does not exist (development and tests; in
    /// production the bucket is made once, by hand or by compose's rclone).
    pub async fn ensure_bucket(&self) -> Result<(), StoreError> {
        let created = Bucket::create_with_path_style(
            &self.config.bucket,
            region(&self.config),
            credentials(&self.config)?,
            BucketConfiguration::default(),
        )
        .await
        .map_err(|e| StoreError(format!("create bucket {}: {e}", self.config.bucket)))?;
        // 409: BucketAlreadyOwnedByYou (or BucketAlreadyExists).
        match created.response_code {
            200 | 409 => Ok(()),
            code => Err(StoreError(format!(
                "create bucket {}: HTTP {code}: {}",
                self.config.bucket, created.response_text
            ))),
        }
    }

    /// Writes `bytes` at `key`, replacing what was there.
    pub async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), StoreError> {
        let response = self
            .bucket
            .put_object_with_content_type(key, bytes, content_type)
            .await
            .map_err(|e| StoreError(format!("put {key}: {e}")))?;
        match response.status_code() {
            200..=299 => Ok(()),
            code => Err(StoreError(format!("put {key}: HTTP {code}"))),
        }
    }

    /// The object at `key`, or `None` when there is none.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let response = self
            .bucket
            .get_object(key)
            .await
            .map_err(|e| StoreError(format!("get {key}: {e}")))?;
        match response.status_code() {
            200 => Ok(Some(response.to_vec())),
            404 => Ok(None),
            code => Err(StoreError(format!("get {key}: HTTP {code}"))),
        }
    }

    /// Deletes the object at `key`; a missing object is not an error (S3's
    /// DELETE is idempotent).
    pub async fn delete(&self, key: &str) -> Result<(), StoreError> {
        let response = self
            .bucket
            .delete_object(key)
            .await
            .map_err(|e| StoreError(format!("delete {key}: {e}")))?;
        match response.status_code() {
            200..=299 | 404 => Ok(()),
            code => Err(StoreError(format!("delete {key}: HTTP {code}"))),
        }
    }

    /// A URL anyone holding it may GET `key` from for `ttl` (SigV4 query
    /// signing; computed locally, no request).
    pub async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, StoreError> {
        let secs = u32::try_from(ttl.as_secs()).unwrap_or(u32::MAX).max(1);
        self.bucket
            .presign_get(key, secs, None)
            .await
            .map_err(|e| StoreError(format!("presign {key}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_follow_the_design() {
        let id = Uuid::parse_str("0190f0f0-1c2d-7a00-8000-0123456789ab").unwrap();
        assert_eq!(
            source_key(id),
            "sources/0190f0f0-1c2d-7a00-8000-0123456789ab.zip"
        );
        assert_eq!(
            module_key(id),
            "modules/0190f0f0-1c2d-7a00-8000-0123456789ab.wasm"
        );
        assert_eq!(log_key(id), "logs/0190f0f0-1c2d-7a00-8000-0123456789ab.txt");
    }

    #[tokio::test]
    async fn presigning_is_local_and_path_style() {
        let store = Store::new(&S3Config {
            endpoint: "http://127.0.0.1:1".parse().unwrap(),
            bucket: "kfm".to_owned(),
            access_key: "ak".to_owned(),
            secret_key: "sk".to_owned(),
            region: "auto".to_owned(),
        })
        .unwrap();
        let url = store
            .presign_get("logs/x.txt", Duration::from_secs(600))
            .await
            .unwrap();
        assert!(
            url.starts_with("http://127.0.0.1:1/kfm/logs/x.txt?"),
            "{url}"
        );
        assert!(url.contains("X-Amz-Expires=600"), "{url}");
        assert!(url.contains("X-Amz-Signature="), "{url}");
    }
}
