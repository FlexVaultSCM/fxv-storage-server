/// Integration tests for AbortMultipartUpload (Stage 5).

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

fn parse_upload_id(xml: &str) -> String {
    let start = xml.find("<UploadId>").expect("UploadId tag") + "<UploadId>".len();
    let end = xml.find("</UploadId>").expect("/UploadId tag");
    xml[start..end].to_owned()
}

/// Abort a multipart upload: should return 204 and clean up part temp files.
#[tokio::test]
async fn test_abort_multipart_upload_204() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    // Create upload
    let create_resp = client
        .post(format!("{}/abortable.bin?uploads", base))
        .send()
        .await
        .expect("create");
    assert_eq!(create_resp.status(), 200);
    let upload_id = parse_upload_id(&create_resp.text().await.unwrap());

    // Upload a part
    client
        .put(format!("{}/abortable.bin?partNumber=1&uploadId={}", base, upload_id))
        .body("some data")
        .send()
        .await
        .expect("upload part");

    // Abort
    let abort_resp = client
        .delete(format!("{}/abortable.bin?uploadId={}", base, upload_id))
        .send()
        .await
        .expect("abort");
    assert_eq!(abort_resp.status(), 204);

    // Attempting to complete should now return 404
    let complete_xml = r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>"abc"</ETag></Part></CompleteMultipartUpload>"#;
    let complete_resp = client
        .post(format!("{}/abortable.bin?uploadId={}", base, upload_id))
        .header("Content-Type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .expect("complete after abort");
    assert_eq!(complete_resp.status(), 404);
}

/// Aborting a non-existent upload ID returns 404.
#[tokio::test]
async fn test_abort_nonexistent_upload_404() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let resp = client
        .delete(format!("{}/file.bin?uploadId=no-such-id", base))
        .send()
        .await
        .expect("abort");
    assert_eq!(resp.status(), 404);
}

/// Aborting without uploadId returns 400.
#[tokio::test]
async fn test_abort_without_upload_id_400() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let resp = client
        .delete(format!("{}/file.bin", base))
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 400);
}

/// After abort, the object is NOT created (no partial file on disk).
#[tokio::test]
async fn test_abort_does_not_create_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let create_resp = client
        .post(format!("{}/ghost.bin?uploads", base))
        .send()
        .await
        .expect("create");
    let upload_id = parse_upload_id(&create_resp.text().await.unwrap());

    client
        .put(format!("{}/ghost.bin?partNumber=1&uploadId={}", base, upload_id))
        .body("phantom data")
        .send()
        .await
        .expect("upload part");

    client
        .delete(format!("{}/ghost.bin?uploadId={}", base, upload_id))
        .send()
        .await
        .expect("abort");

    // The file should not be accessible
    let get_resp = reqwest::get(format!("{}/ghost.bin", base))
        .await
        .expect("GET");
    assert_eq!(get_resp.status(), 404);
}
