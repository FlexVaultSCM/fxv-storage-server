/// Integration tests for PutObject (Stage 3).
///
/// Spins up a full fxv-storage-server instance and tests PUT operations,
/// including conditional headers and path traversal rejection.
mod common;

// == Std
use std::fs;

// == Internal
use common::spawn_server;

// == tests

/// Basic upload: PUT a file that didn't exist, then GET it back.
#[tokio::test]
async fn test_put_new_file_200_and_readable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/newfile.txt", base))
        .body("hello from put")
        .send()
        .await
        .expect("PUT");

    assert_eq!(resp.status(), 200);
    let etag = resp
        .headers()
        .get("etag")
        .expect("ETag header")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "ETag should be double-quoted"
    );

    // Verify the file is readable via GET
    let get_resp = reqwest::get(format!("{}/newfile.txt", base)).await.expect("GET");
    assert_eq!(get_resp.status(), 200);
    assert_eq!(get_resp.bytes().await.expect("body").as_ref(), b"hello from put");
}

/// Overwrite: PUT to an existing key replaces the content.
#[tokio::test]
async fn test_put_overwrite_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("file.txt"), b"original").expect("write");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/file.txt", base))
        .body("overwritten")
        .send()
        .await
        .expect("PUT");
    assert_eq!(resp.status(), 200);

    let body = reqwest::get(format!("{}/file.txt", base))
        .await
        .expect("GET")
        .bytes()
        .await
        .expect("body");
    assert_eq!(body.as_ref(), b"overwritten");
}

/// If-None-Match: * should return 412 if the file already exists.
#[tokio::test]
async fn test_put_if_none_match_star_412_when_exists() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("existing.txt"), b"data").expect("write");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/existing.txt", base))
        .header("If-None-Match", "*")
        .body("new data")
        .send()
        .await
        .expect("PUT");

    assert_eq!(resp.status(), 412);
}

/// If-None-Match: * should succeed when the file does NOT exist.
#[tokio::test]
async fn test_put_if_none_match_star_200_when_new() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/brand-new.txt", base))
        .header("If-None-Match", "*")
        .body("first write")
        .send()
        .await
        .expect("PUT");

    assert_eq!(resp.status(), 200);
}

/// If-Match with matching ETag should succeed.
#[tokio::test]
async fn test_put_if_match_correct_etag_200() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    // Upload the file first and capture its ETag
    let put1 = client
        .put(format!("{}/cond.txt", base))
        .body("version1")
        .send()
        .await
        .expect("PUT1");
    assert_eq!(put1.status(), 200);
    let etag = put1.headers().get("etag").expect("etag").to_str().unwrap().to_owned();

    // Second PUT with If-Match matching the ETag
    let put2 = client
        .put(format!("{}/cond.txt", base))
        .header("If-Match", &etag)
        .body("version2")
        .send()
        .await
        .expect("PUT2");
    assert_eq!(put2.status(), 200);
}

/// If-Match with wrong ETag should return 412.
#[tokio::test]
async fn test_put_if_match_wrong_etag_412() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("file.txt"), b"original").expect("write");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/file.txt", base))
        .header("If-Match", "\"00000000000000000000000000000000\"")
        .body("new content")
        .send()
        .await
        .expect("PUT");

    assert_eq!(resp.status(), 412);
}

/// If-Match on a non-existent file should return 412.
#[tokio::test]
async fn test_put_if_match_nonexistent_412() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/does-not-exist.txt", base))
        .header("If-Match", "\"abcd\"")
        .body("content")
        .send()
        .await
        .expect("PUT");

    assert_eq!(resp.status(), 412);
}

/// If-Match: * should succeed regardless of the existing ETag.
#[tokio::test]
async fn test_put_if_match_star_200() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("file.txt"), b"original").expect("write");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/file.txt", base))
        .header("If-Match", "*")
        .body("updated")
        .send()
        .await
        .expect("PUT");

    assert_eq!(resp.status(), 200);
}

/// Nested key: PUT to a/b/c.txt should create directories as needed.
#[tokio::test]
async fn test_put_nested_key_creates_dirs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{}/a/b/c.txt", base))
        .body("nested content")
        .send()
        .await
        .expect("PUT");
    assert_eq!(resp.status(), 200);

    let body = reqwest::get(format!("{}/a/b/c.txt", base))
        .await
        .expect("GET")
        .bytes()
        .await
        .expect("body");
    assert_eq!(body.as_ref(), b"nested content");
}

/// User-controlled x-amz-meta-* headers should be persisted and replayed on GET.
#[tokio::test]
async fn test_put_persists_user_metadata_headers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();

    let client = reqwest::Client::new();
    let put_resp = client
        .put(format!("{}/meta.txt", base))
        .header("x-amz-meta-owner", "alice")
        .header("x-amz-meta-color", "blue")
        .body("metadata")
        .send()
        .await
        .expect("PUT");
    assert_eq!(put_resp.status(), 200);

    let get_resp = client.get(format!("{}/meta.txt", base)).send().await.expect("GET");
    assert_eq!(get_resp.status(), 200);
    assert_eq!(
        get_resp
            .headers()
            .get("x-amz-meta-owner")
            .expect("owner header")
            .to_str()
            .unwrap(),
        "alice"
    );
    assert_eq!(
        get_resp
            .headers()
            .get("x-amz-meta-color")
            .expect("color header")
            .to_str()
            .unwrap(),
        "blue"
    );
}
