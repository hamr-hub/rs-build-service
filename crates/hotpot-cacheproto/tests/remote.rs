//! RemoteCache 索引与 CAS 集成测试。

use hotpot_cacheproto::{Namespace, RemoteCache};
use hotpot_store::local::{LocalStore, StoreOptions};

async fn setup() -> (RemoteCache, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStore::open(dir.path().join("store"), StoreOptions::default()).unwrap();
    let cache = RemoteCache::open(&dir, store).await.unwrap();
    (cache, dir)
}

#[tokio::test]
async fn put_get_by_key() {
    let (cache, _dir) = setup().await;
    cache
        .put(Namespace::Sccache, "ab/cd", "", b"artifact bytes", None)
        .await
        .unwrap();
    let entry = cache
        .get(Namespace::Sccache, "ab/cd", "")
        .await
        .unwrap()
        .expect("hit");
    assert_eq!(entry.bytes, b"artifact bytes");
    assert!(entry.tag.is_none());
    assert!(
        cache
            .contains(Namespace::Sccache, "ab/cd", "")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn missing_is_none() {
    let (cache, _dir) = setup().await;
    assert!(
        cache
            .get(Namespace::Turbo, "nope", "")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn put_is_idempotent() {
    let (cache, _dir) = setup().await;
    let d1 = cache
        .put(Namespace::Turbo, "h1", "", b"same", None)
        .await
        .unwrap();
    let d2 = cache
        .put(Namespace::Turbo, "h1", "", b"same", None)
        .await
        .unwrap();
    assert_eq!(d1, d2);
}

#[tokio::test]
async fn tenants_are_isolated() {
    let (cache, _dir) = setup().await;
    cache
        .put(Namespace::Turbo, "h1", "team-a", b"a data", None)
        .await
        .unwrap();
    assert!(
        cache
            .get(Namespace::Turbo, "h1", "team-b")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cache
            .get(Namespace::Turbo, "h1", "team-a")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn tag_round_trips() {
    let (cache, _dir) = setup().await;
    cache
        .put(Namespace::Turbo, "h1", "", b"x", Some("signed-tag".into()))
        .await
        .unwrap();
    let entry = cache
        .get(Namespace::Turbo, "h1", "")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entry.tag.as_deref(), Some("signed-tag"));
}
