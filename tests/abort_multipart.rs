/// Integration tests for AbortMultipartUpload (Stage 5).
mod common;
#[path = "common/xml.rs"]
mod xml;

// == Internal
use common::spawn_server;
use xml::parse_xml_tag;

/// Abort a multipart upload: should return 204 and clean up part temp files.
#[tokio::test]
async fn test_abort_multipart_upload_204() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    // Create upload
    let create_resp = client
        .post(format!("{}/abortable.bin?uploads", base))
        .send()
        .await
        .expect("create");
    assert_eq!(create_resp.status(), 200);
    let upload_id = parse_xml_tag(&create_resp.text().await.unwrap(), "UploadId");

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
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let resp = client
        .delete(format!("{}/file.bin?uploadId=no-such-id", base))
        .send()
        .await
        .expect("abort");
    assert_eq!(resp.status(), 404);
}

/// Plain DeleteObject routing must not accidentally abort an in-progress upload.
#[tokio::test]
async fn test_delete_without_upload_id_does_not_abort_upload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let create_resp = client
        .post(format!("{}/file.bin?uploads", base))
        .send()
        .await
        .expect("create");
    let upload_id = parse_xml_tag(&create_resp.text().await.unwrap(), "UploadId");

    let part_resp = client
        .put(format!("{}/file.bin?partNumber=1&uploadId={}", base, upload_id))
        .body("data")
        .send()
        .await
        .expect("upload part");
    let part_etag = part_resp
        .headers()
        .get("etag")
        .expect("etag")
        .to_str()
        .unwrap()
        .to_owned();

    let delete_resp = client
        .delete(format!("{}/file.bin", base))
        .send()
        .await
        .expect("delete object");
    assert_eq!(delete_resp.status(), 204);

    let complete_xml = format!(
        r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        part_etag
    );
    let complete_resp = client
        .post(format!("{}/file.bin?uploadId={}", base, upload_id))
        .header("Content-Type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .expect("complete");
    assert_eq!(complete_resp.status(), 200);
}

/// After abort, the object is NOT created (no partial file on disk).
#[tokio::test]
async fn test_abort_does_not_create_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let create_resp = client
        .post(format!("{}/ghost.bin?uploads", base))
        .send()
        .await
        .expect("create");
    let upload_id = parse_xml_tag(&create_resp.text().await.unwrap(), "UploadId");

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
    let get_resp = reqwest::get(format!("{}/ghost.bin", base)).await.expect("GET");
    assert_eq!(get_resp.status(), 404);
}
