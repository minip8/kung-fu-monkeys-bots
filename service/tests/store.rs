//! The object store against a real S3 API (MinIO): put, get, overwrite,
//! delete, a missing key, and a presigned GET fetched without credentials.
//! Skipped when KFM_LADDER_TEST_S3_ENDPOINT is unset.

mod common;

use std::time::Duration;

use common::test_store;
use kfm_ladder::store;
use uuid::Uuid;

#[tokio::test]
async fn objects_round_trip() {
    let Some(store) = test_store("objects_round_trip").await else {
        return;
    };
    let id = Uuid::now_v7();
    let key = store::log_key(id);
    assert_eq!(store.get(&key).await.unwrap(), None);
    store
        .put(&key, b"first line\n", "text/plain; charset=utf-8")
        .await
        .unwrap();
    assert_eq!(
        store.get(&key).await.unwrap().as_deref(),
        Some(&b"first line\n"[..])
    );
    // A put replaces.
    let bytes: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    store
        .put(&key, &bytes, "application/octet-stream")
        .await
        .unwrap();
    assert_eq!(store.get(&key).await.unwrap(), Some(bytes));
    store.delete(&key).await.unwrap();
    assert_eq!(store.get(&key).await.unwrap(), None);
    // Deleting what is gone is fine.
    store.delete(&key).await.unwrap();
}

#[tokio::test]
async fn a_presigned_url_needs_no_credentials() {
    let Some(store) = test_store("a_presigned_url_needs_no_credentials").await else {
        return;
    };
    let key = store::module_key(Uuid::now_v7());
    store
        .put(&key, b"\0asm\x01\0\0\0", "application/wasm")
        .await
        .unwrap();
    let url = store
        .presign_get(&key, Duration::from_secs(300))
        .await
        .unwrap();
    let response = reqwest::get(&url).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK, "{url}");
    assert_eq!(&response.bytes().await.unwrap()[..], b"\0asm\x01\0\0\0");
    // The same URL without its signature is refused.
    let unsigned = url.split('?').next().unwrap();
    let refused = reqwest::get(unsigned).await.unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);
    store.delete(&key).await.unwrap();
}
