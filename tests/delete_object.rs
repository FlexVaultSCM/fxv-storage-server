/// Integration tests for DeleteObject.
use std::net::SocketAddr;
use std::path::PathBuf;

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

#[tokio::test]
async fn test_delete_existing_object_204_and_removes_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("gone.txt"), b"to be deleted").expect("write");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let resp = client
        .delete(format!("{}/gone.txt", base))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);

    let get_resp = reqwest::get(format!("{}/gone.txt", base))
        .await
        .expect("get");
    assert_eq!(get_resp.status(), 404);
    assert!(!dir.path().join("gone.txt").exists());
}

#[tokio::test]
async fn test_delete_missing_object_is_still_204() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
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
    std::fs::create_dir_all(dir.path().join("nested")).expect("mkdir");
    std::fs::write(dir.path().join("nested/file.txt"), b"cache me").expect("write");

    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();
    let cache_file = dir.path().join(".fxv-metadata-cache/nested/file.txt.json");
    assert!(
        cache_file.exists(),
        "startup indexing should create the cache entry"
    );

    let resp = client
        .delete(format!("{}/nested/file.txt", base))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);
    assert!(
        !cache_file.exists(),
        "DeleteObject should clear the cached metadata"
    );
}
