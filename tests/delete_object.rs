/// Integration tests for DeleteObject.
mod common;

// == Std
use std::fs;

// == Internal
use common::spawn_server;

#[tokio::test]
async fn test_delete_existing_object_204_and_removes_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("gone.txt"), b"to be deleted").expect("write");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let resp = client
        .delete(format!("{}/gone.txt", base))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);

    let get_resp = reqwest::get(format!("{}/gone.txt", base)).await.expect("get");
    assert_eq!(get_resp.status(), 404);
    assert!(!dir.path().join("gone.txt").exists());
}

#[tokio::test]
async fn test_delete_missing_object_is_still_204() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let resp = client
        .delete(format!("{}/missing.txt", base))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn test_delete_removes_cached_metadata_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("nested")).expect("mkdir");
    fs::write(dir.path().join("nested/file.txt"), b"cache me").expect("write");

    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();
    let cache_file = dir.path().join(".fxv-metadata-cache/nested/file.txt.json");
    assert!(cache_file.exists(), "startup indexing should create the cache entry");

    let resp = client
        .delete(format!("{}/nested/file.txt", base))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);
    assert!(!cache_file.exists(), "DeleteObject should clear the cached metadata");
}
