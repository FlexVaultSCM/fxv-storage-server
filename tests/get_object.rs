/// Integration tests for GetObject (Stage 2).
///
/// Spins up a full fxv-storage-server instance, serves files from a temp
/// directory, and verifies correct HTTP behaviour via reqwest.

use std::net::SocketAddr;
use std::path::PathBuf;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Spawn a server on an ephemeral port and return its base URL.
async fn spawn_server(serve_dir: PathBuf) -> String {
    let store = fxv_storage_server::store::build_shared_store(&serve_dir)
        .await
        .expect("build store");

    let app = fxv_storage_server::build_app(store);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    format!("http://{}", addr)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_existing_file_200() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("hello.txt"), b"hello world").expect("write");

    let base = spawn_server(dir.path().to_owned()).await;
    let resp = reqwest::get(format!("{}/hello.txt", base))
        .await
        .expect("GET");

    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.expect("body");
    assert_eq!(body.as_ref(), b"hello world");
}

#[tokio::test]
async fn test_get_missing_file_404() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let resp = reqwest::get(format!("{}/missing.txt", base))
        .await
        .expect("GET");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_get_returns_etag_and_last_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("data.bin"), b"some data").expect("write");

    let base = spawn_server(dir.path().to_owned()).await;
    let resp = reqwest::get(format!("{}/data.bin", base))
        .await
        .expect("GET");

    assert_eq!(resp.status(), 200);
    let etag = resp.headers().get("etag").expect("etag header");
    assert!(etag.to_str().unwrap().starts_with('"'), "ETag must be quoted");
    assert!(resp.headers().contains_key("last-modified"), "Last-Modified must be present");
}

#[tokio::test]
async fn test_range_request_206() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("range.bin"), b"0123456789").expect("write");

    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/range.bin", base))
        .header("Range", "bytes=2-5")
        .send()
        .await
        .expect("GET range");

    assert_eq!(resp.status(), 206);
    let content_range = resp
        .headers()
        .get("content-range")
        .expect("content-range")
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(content_range, "bytes 2-5/10");
    let body = resp.bytes().await.expect("body");
    assert_eq!(body.as_ref(), b"2345");
}

#[tokio::test]
async fn test_if_none_match_304() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("cached.txt"), b"cached").expect("write");

    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    // First request to get the ETag
    let resp1 = client
        .get(format!("{}/cached.txt", base))
        .send()
        .await
        .expect("first GET");
    assert_eq!(resp1.status(), 200);
    let etag = resp1
        .headers()
        .get("etag")
        .expect("etag")
        .to_str()
        .unwrap()
        .to_owned();

    // Second request with the ETag → 304
    let resp2 = client
        .get(format!("{}/cached.txt", base))
        .header("If-None-Match", &etag)
        .send()
        .await
        .expect("second GET");
    assert_eq!(resp2.status(), 304);
}

#[tokio::test]
async fn test_if_match_412_on_mismatch() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("file.bin"), b"content").expect("write");

    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/file.bin", base))
        .header("If-Match", "\"wrongetag000000000000000000000000000000000000000000000000000000000000\"")
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), 412);
}

#[tokio::test]
async fn test_nested_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
    std::fs::write(dir.path().join("a/b/file.txt"), b"nested").expect("write");

    let base = spawn_server(dir.path().to_owned()).await;
    let resp = reqwest::get(format!("{}/a/b/file.txt", base))
        .await
        .expect("GET");
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.expect("body");
    assert_eq!(body.as_ref(), b"nested");
}

#[tokio::test]
async fn test_range_not_satisfiable_416() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("small.bin"), b"hi").expect("write");

    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/small.bin", base))
        .header("Range", "bytes=100-200")
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status(), 416);
}
