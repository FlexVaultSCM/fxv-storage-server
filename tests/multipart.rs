/// Integration tests for Multipart Upload (Stage 4).
///
/// Spins up a full fxv-storage-server instance and tests the full
/// CreateMultipartUpload -> UploadPart x N -> CompleteMultipartUpload flow.
use fxv_storage_server::etag;
use fxv_storage_server::multipart_state::MULTIPART_UPLOAD_DIR;
use md5::Digest;
use std::net::SocketAddr;
use std::path::PathBuf;

// == helpers

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

/// Parse `<UploadId>...</UploadId>` from CreateMultipartUpload XML response.
fn parse_upload_id(xml: &str) -> String {
    let start = xml.find("<UploadId>").expect("UploadId tag") + "<UploadId>".len();
    let end = xml.find("</UploadId>").expect("/UploadId tag");
    xml[start..end].to_owned()
}

/// Parse `<ETag>...</ETag>` from CompleteMultipartUpload XML response.
fn parse_etag_from_xml(xml: &str) -> String {
    let start = xml.find("<ETag>").expect("ETag tag") + "<ETag>".len();
    let end = xml.find("</ETag>").expect("/ETag tag");
    xml[start..end].to_owned()
}

// == tests

/// Full multipart flow: create, upload 2 parts, complete, verify content.
#[tokio::test]
async fn test_multipart_full_flow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    // 1. CreateMultipartUpload
    let create_resp = client
        .post(format!("{}/multipart.bin?uploads", base))
        .send()
        .await
        .expect("CreateMultipartUpload");
    assert_eq!(create_resp.status(), 200);
    let create_body = create_resp.text().await.expect("body");
    let upload_id = parse_upload_id(&create_body);
    assert!(!upload_id.is_empty());

    // 2. UploadPart 1
    let part1_resp = client
        .put(format!(
            "{}/multipart.bin?partNumber=1&uploadId={}",
            base, upload_id
        ))
        .body("hello ")
        .send()
        .await
        .expect("UploadPart 1");
    assert_eq!(part1_resp.status(), 200);
    let etag1 = part1_resp
        .headers()
        .get("etag")
        .expect("etag1")
        .to_str()
        .unwrap()
        .to_owned();

    // 3. UploadPart 2
    let part2_resp = client
        .put(format!(
            "{}/multipart.bin?partNumber=2&uploadId={}",
            base, upload_id
        ))
        .body("world")
        .send()
        .await
        .expect("UploadPart 2");
    assert_eq!(part2_resp.status(), 200);
    let etag2 = part2_resp
        .headers()
        .get("etag")
        .expect("etag2")
        .to_str()
        .unwrap()
        .to_owned();
    let multipart_dir = dir.path().join(MULTIPART_UPLOAD_DIR);
    assert!(multipart_dir.exists(), "multipart temp dir should exist");
    assert!(
        std::fs::read_dir(&multipart_dir)
            .expect("read multipart dir")
            .next()
            .is_some(),
        "multipart temp dir should contain uploaded part files"
    );

    // 4. CompleteMultipartUpload
    let complete_xml = format!(
        r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1, etag2
    );
    let complete_resp = client
        .post(format!("{}/multipart.bin?uploadId={}", base, upload_id))
        .header("Content-Type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .expect("CompleteMultipartUpload");
    assert_eq!(complete_resp.status(), 200);
    let complete_body = complete_resp.text().await.expect("complete body");
    let final_etag = parse_etag_from_xml(&complete_body);
    assert!(final_etag.starts_with('"') && final_etag.ends_with('"'));
    assert!(final_etag.contains("-2"));

    // 5. GET the assembled file and verify content
    let get_resp = reqwest::get(format!("{}/multipart.bin", base))
        .await
        .expect("GET");
    assert_eq!(get_resp.status(), 200);
    assert_eq!(
        get_resp.bytes().await.expect("body").as_ref(),
        b"hello world"
    );

    let rebuilt_store = fxv_storage_server::store::build_shared_store(dir.path())
        .await
        .expect("rebuild store");
    let store = rebuilt_store.read().await;
    assert_eq!(
        store.get("multipart.bin").expect("multipart entry").etag,
        final_etag
    );
}

/// Completing with a wrong ETag for a part should return 400.
#[tokio::test]
async fn test_multipart_complete_wrong_etag_400() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let create_resp = client
        .post(format!("{}/file.bin?uploads", base))
        .send()
        .await
        .expect("create");
    let upload_id = parse_upload_id(&create_resp.text().await.unwrap());

    client
        .put(format!(
            "{}/file.bin?partNumber=1&uploadId={}",
            base, upload_id
        ))
        .body("data")
        .send()
        .await
        .expect("upload part");

    let complete_xml = format!(
        r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>"00000000000000000000000000000000"</ETag></Part></CompleteMultipartUpload>"#
    );
    let complete_resp = client
        .post(format!("{}/file.bin?uploadId={}", base, upload_id))
        .header("Content-Type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .expect("complete");
    assert_eq!(complete_resp.status(), 400);
}

/// Completing with a non-existent uploadId should return 404.
#[tokio::test]
async fn test_multipart_complete_unknown_upload_404() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let complete_xml = r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>"abc"</ETag></Part></CompleteMultipartUpload>"#;
    let resp = client
        .post(format!("{}/file.bin?uploadId=nonexistent-id", base))
        .header("Content-Type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .expect("complete");
    assert_eq!(resp.status(), 404);
}

/// UploadPart with invalid part number should return 400.
#[tokio::test]
async fn test_multipart_upload_part_invalid_number() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let create_resp = client
        .post(format!("{}/file.bin?uploads", base))
        .send()
        .await
        .expect("create");
    let upload_id = parse_upload_id(&create_resp.text().await.unwrap());

    // Part number 0 is invalid
    let resp = client
        .put(format!(
            "{}/file.bin?partNumber=0&uploadId={}",
            base, upload_id
        ))
        .body("data")
        .send()
        .await
        .expect("upload part");
    assert_eq!(resp.status(), 400);
}

/// Multipart completion should use the S3 multipart ETag formula rather than
/// the single-part PutObject ETag.
#[tokio::test]
async fn test_multipart_etag_uses_s3_formula() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let content = b"identical content for both methods";

    // PUT directly
    let put_resp = client
        .put(format!("{}/direct.bin", base))
        .body(content.as_ref())
        .send()
        .await
        .expect("PUT");
    assert_eq!(put_resp.status(), 200);
    let put_etag = put_resp
        .headers()
        .get("etag")
        .expect("etag")
        .to_str()
        .unwrap()
        .to_owned();

    // Multipart upload in one part
    let create_resp = client
        .post(format!("{}/multipart.bin?uploads", base))
        .send()
        .await
        .expect("create");
    let upload_id = parse_upload_id(&create_resp.text().await.unwrap());

    let part_resp = client
        .put(format!(
            "{}/multipart.bin?partNumber=1&uploadId={}",
            base, upload_id
        ))
        .body(content.as_ref())
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

    let complete_xml = format!(
        r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        part_etag
    );
    let complete_resp = client
        .post(format!("{}/multipart.bin?uploadId={}", base, upload_id))
        .header("Content-Type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .expect("complete");
    assert_eq!(complete_resp.status(), 200);
    let complete_body = complete_resp.text().await.unwrap();
    let multipart_etag = parse_etag_from_xml(&complete_body);

    let expected_put_etag = etag::compute_file_etag(&dir.path().join("direct.bin"))
        .await
        .expect("compute put etag");
    let expected_multipart_etag =
        etag::multipart_etag_from_part_digests(&[etag::Md5DigestBytes::from(md5::Md5::digest(
            content,
        ))]);

    assert_eq!(put_etag, expected_put_etag);
    assert_eq!(
        multipart_etag, expected_multipart_etag,
        "Multipart ETag should use the S3 multipart formula"
    );
    assert_ne!(put_etag, multipart_etag);
}

/// Metadata supplied at multipart initiation should be replayed on the completed object.
#[tokio::test]
async fn test_multipart_persists_user_metadata_headers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = reqwest::Client::new();

    let create_resp = client
        .post(format!("{}/meta.bin?uploads", base))
        .header("x-amz-meta-owner", "alice")
        .send()
        .await
        .expect("create");
    let upload_id = parse_upload_id(&create_resp.text().await.unwrap());

    let part_resp = client
        .put(format!(
            "{}/meta.bin?partNumber=1&uploadId={}",
            base, upload_id
        ))
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

    let complete_xml = format!(
        r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        part_etag
    );
    let complete_resp = client
        .post(format!("{}/meta.bin?uploadId={}", base, upload_id))
        .header("Content-Type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .expect("complete");
    assert_eq!(complete_resp.status(), 200);

    let get_resp = client
        .get(format!("{}/meta.bin", base))
        .send()
        .await
        .expect("GET");
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
}
