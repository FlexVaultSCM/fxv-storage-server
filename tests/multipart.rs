/// Integration tests for Multipart Upload (Stage 4).
///
/// Spins up a full fxv-storage-server instance and tests the full
/// CreateMultipartUpload -> UploadPart x N -> CompleteMultipartUpload flow.
mod common;
#[path = "common/xml.rs"]
mod xml;

// == Std
use std::fs;

// == Internal
use common::spawn_server;
use xml::parse_xml_tag;

// == External
use fxv_storage_server::{etag, multipart_state::MULTIPART_UPLOAD_DIR};
use md5::Digest;

// == tests

/// Full multipart flow: create, upload 2 parts, complete, verify content.
#[tokio::test]
async fn test_multipart_full_flow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    // 1. CreateMultipartUpload
    let create_resp = client
        .post(format!("{}/multipart.bin?uploads", base))
        .send()
        .await
        .expect("CreateMultipartUpload");
    assert_eq!(create_resp.status(), 200);
    let create_body = create_resp.text().await.expect("body");
    let upload_id = parse_xml_tag(&create_body, "UploadId");
    assert!(!upload_id.is_empty());

    // 2. UploadPart 1
    let part1_resp = client
        .put(format!("{}/multipart.bin?partNumber=1&uploadId={}", base, upload_id))
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
        .put(format!("{}/multipart.bin?partNumber=2&uploadId={}", base, upload_id))
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
        fs::read_dir(&multipart_dir)
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
    let final_etag = parse_xml_tag(&complete_body, "ETag");
    assert!(final_etag.starts_with('"') && final_etag.ends_with('"'));
    assert!(final_etag.contains("-2"));

    // 5. GET the assembled file and verify content
    let get_resp = reqwest::get(format!("{}/multipart.bin", base)).await.expect("GET");
    assert_eq!(get_resp.status(), 200);
    assert_eq!(get_resp.bytes().await.expect("body").as_ref(), b"hello world");

    let rebuilt_store = fxv_storage_server::store::build_shared_store(dir.path())
        .await
        .expect("rebuild store");
    let store = rebuilt_store.read().await;
    assert_eq!(store.get("multipart.bin").expect("multipart entry").etag, final_etag);
}

/// Completing with a wrong ETag for a part should return 400.
#[tokio::test]
async fn test_multipart_complete_wrong_etag_400() {
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

    client
        .put(format!("{}/file.bin?partNumber=1&uploadId={}", base, upload_id))
        .body("data")
        .send()
        .await
        .expect("upload part");

    let complete_xml = r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>"00000000000000000000000000000000"</ETag></Part></CompleteMultipartUpload>"#
        .to_string();
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
    let server = spawn_server(dir.path());
    let base = server.url();
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
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let create_resp = client
        .post(format!("{}/file.bin?uploads", base))
        .send()
        .await
        .expect("create");
    let upload_id = parse_xml_tag(&create_resp.text().await.unwrap(), "UploadId");

    // Part number 0 is invalid
    let resp = client
        .put(format!("{}/file.bin?partNumber=0&uploadId={}", base, upload_id))
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
    let server = spawn_server(dir.path());
    let base = server.url();
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
    let upload_id = parse_xml_tag(&create_resp.text().await.unwrap(), "UploadId");

    let part_resp = client
        .put(format!("{}/multipart.bin?partNumber=1&uploadId={}", base, upload_id))
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
    let multipart_etag = parse_xml_tag(&complete_body, "ETag");

    let expected_put_etag = etag::compute_file_etag(&dir.path().join("direct.bin"))
        .await
        .expect("compute put etag");
    let expected_multipart_etag =
        etag::multipart_etag_from_part_digests(&[etag::Md5DigestBytes::from(md5::Md5::digest(content))]);

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
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let create_resp = client
        .post(format!("{}/meta.bin?uploads", base))
        .header("x-amz-meta-owner", "alice")
        .send()
        .await
        .expect("create");
    let upload_id = parse_xml_tag(&create_resp.text().await.unwrap(), "UploadId");

    let part_resp = client
        .put(format!("{}/meta.bin?partNumber=1&uploadId={}", base, upload_id))
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

    let get_resp = client.get(format!("{}/meta.bin", base)).send().await.expect("GET");
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

/// Starts a multipart upload of `key` with one part holding `body`, returning the upload ID and the part's ETag.
async fn upload_one_part(client: &reqwest::Client, base: &str, key: &str, body: &'static str) -> (String, String) {
    let create_resp = client
        .post(format!("{}/{}?uploads", base, key))
        .send()
        .await
        .expect("CreateMultipartUpload");
    let upload_id = parse_xml_tag(&create_resp.text().await.unwrap(), "UploadId");
    let part_resp = client
        .put(format!("{}/{}?partNumber=1&uploadId={}", base, key, upload_id))
        .body(body)
        .send()
        .await
        .expect("UploadPart");
    let etag = part_resp
        .headers()
        .get("etag")
        .expect("etag")
        .to_str()
        .unwrap()
        .to_owned();
    (upload_id, etag)
}

/// Completes the upload of `key` with its one part, sending `condition` as a header where given.
async fn complete_one_part(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    (upload_id, etag): &(String, String),
    condition: Option<(&str, &str)>,
) -> reqwest::Response {
    let complete_xml = format!(
        r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag
    );
    let mut request = client
        .post(format!("{}/{}?uploadId={}", base, key, upload_id))
        .header("Content-Type", "application/xml")
        .body(complete_xml);
    if let Some((name, value)) = condition {
        request = request.header(name, value);
    }
    request.send().await.expect("CompleteMultipartUpload")
}

/// A complete with `If-None-Match: *` creates a missing object, and is refused with 412 onto an existing one, which
/// it leaves as it was. The refused upload is kept, so it can still be aborted.
#[tokio::test]
async fn test_multipart_complete_if_none_match_creates_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let first = upload_one_part(&client, &base, "created.bin", "first").await;
    let resp = complete_one_part(&client, &base, "created.bin", &first, Some(("If-None-Match", "*"))).await;
    assert_eq!(resp.status(), 200);

    let second = upload_one_part(&client, &base, "created.bin", "second").await;
    let resp = complete_one_part(&client, &base, "created.bin", &second, Some(("If-None-Match", "*"))).await;
    assert_eq!(resp.status(), 412);
    let body = resp.text().await.unwrap();
    assert_eq!(parse_xml_tag(&body, "Code"), "PreconditionFailed");

    let get_resp = reqwest::get(format!("{}/created.bin", base)).await.expect("GET");
    assert_eq!(get_resp.bytes().await.unwrap().as_ref(), b"first");

    let abort_resp = client
        .delete(format!("{}/created.bin?uploadId={}", base, second.0))
        .send()
        .await
        .expect("AbortMultipartUpload");
    assert_eq!(abort_resp.status(), 204);
    let multipart_dir = dir.path().join(MULTIPART_UPLOAD_DIR);
    assert_eq!(
        fs::read_dir(&multipart_dir).expect("read multipart dir").count(),
        0,
        "the aborted upload's parts are removed"
    );
}

/// A complete with `If-Match` replaces an object whose ETag matches, and is refused with 412 onto one whose ETag
/// differs or that doesn't exist.
#[tokio::test]
async fn test_multipart_complete_if_match_requires_the_etag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let missing = upload_one_part(&client, &base, "replaced.bin", "never").await;
    let resp = complete_one_part(&client, &base, "replaced.bin", &missing, Some(("If-Match", "\"abc\""))).await;
    assert_eq!(resp.status(), 412);

    let first = upload_one_part(&client, &base, "replaced.bin", "first").await;
    let resp = complete_one_part(&client, &base, "replaced.bin", &first, None).await;
    assert_eq!(resp.status(), 200);
    let first_etag = resp.headers().get("etag").unwrap().to_str().unwrap().to_owned();

    let stale = upload_one_part(&client, &base, "replaced.bin", "stale").await;
    let resp = complete_one_part(&client, &base, "replaced.bin", &stale, Some(("If-Match", "\"abc\""))).await;
    assert_eq!(resp.status(), 412);

    let second = upload_one_part(&client, &base, "replaced.bin", "second").await;
    let resp = complete_one_part(&client, &base, "replaced.bin", &second, Some(("If-Match", &first_etag))).await;
    assert_eq!(resp.status(), 200);
    let get_resp = reqwest::get(format!("{}/replaced.bin", base)).await.expect("GET");
    assert_eq!(get_resp.bytes().await.unwrap().as_ref(), b"second");
}

/// An upload whose create-only complete was refused is kept, so a complete without the condition still finishes it.
#[tokio::test]
async fn test_multipart_refused_complete_can_be_completed_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let first = upload_one_part(&client, &base, "again.bin", "first").await;
    assert_eq!(
        complete_one_part(&client, &base, "again.bin", &first, None)
            .await
            .status(),
        200
    );

    let second = upload_one_part(&client, &base, "again.bin", "second").await;
    let refused = complete_one_part(&client, &base, "again.bin", &second, Some(("If-None-Match", "*"))).await;
    assert_eq!(refused.status(), 412);
    assert_eq!(
        complete_one_part(&client, &base, "again.bin", &second, None)
            .await
            .status(),
        200
    );

    let get_resp = reqwest::get(format!("{}/again.bin", base)).await.expect("GET");
    assert_eq!(get_resp.bytes().await.unwrap().as_ref(), b"second");
}

/// A complete whose condition can't be evaluated is refused, and leaves the object as it was: a quoted `"*"` or another
/// `If-None-Match` value with 501 NotImplemented, as S3 refuses them, and a repeated condition with 400.
#[tokio::test]
async fn test_multipart_complete_refuses_conditions_it_cannot_evaluate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let first = upload_one_part(&client, &base, "kept.bin", "first").await;
    assert_eq!(
        complete_one_part(&client, &base, "kept.bin", &first, None)
            .await
            .status(),
        200
    );

    let second = upload_one_part(&client, &base, "kept.bin", "second").await;
    for value in ["\"*\"", "\"abc\"", "*, \"abc\""] {
        let resp = complete_one_part(&client, &base, "kept.bin", &second, Some(("If-None-Match", value))).await;
        assert_eq!(resp.status(), 501, "{value}");
        assert_eq!(parse_xml_tag(&resp.text().await.unwrap(), "Code"), "NotImplemented");
    }
    let complete_xml = format!(
        r#"<?xml version="1.0"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        second.1
    );
    let repeated = client
        .post(format!("{}/kept.bin?uploadId={}", base, second.0))
        .header("If-None-Match", "\"abc\"")
        .header("If-None-Match", "*")
        .body(complete_xml)
        .send()
        .await
        .expect("CompleteMultipartUpload");
    assert_eq!(repeated.status(), 400);

    let get_resp = reqwest::get(format!("{}/kept.bin", base)).await.expect("GET");
    assert_eq!(get_resp.bytes().await.unwrap().as_ref(), b"first");
}

/// Of several create-only completes of different uploads to one key at once, exactly one creates the object, the rest
/// are refused with 412, and the object has the bytes of the one that was accepted. The check under the store's
/// write lock decides between completes that pass the early check together.
#[tokio::test]
async fn test_multipart_concurrent_create_only_completes_create_once() {
    const UPLOADS: usize = 8;
    // Large enough that assembling the parts takes long enough for the completes to overlap.
    const PART_SIZE: usize = 4 * 1024 * 1024;

    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let mut uploads = Vec::new();
    for upload in 0..UPLOADS {
        let create_resp = client
            .post(format!("{}/contended.bin?uploads", base))
            .send()
            .await
            .expect("CreateMultipartUpload");
        let upload_id = parse_xml_tag(&create_resp.text().await.unwrap(), "UploadId");
        let part_resp = client
            .put(format!("{}/contended.bin?partNumber=1&uploadId={}", base, upload_id))
            .body(vec![upload as u8; PART_SIZE])
            .send()
            .await
            .expect("UploadPart");
        let etag = part_resp.headers().get("etag").unwrap().to_str().unwrap().to_owned();
        uploads.push((upload_id, etag));
    }

    let completes = uploads.into_iter().enumerate().map(|(upload, upload_and_etag)| {
        let client = client.clone();
        let base = base.clone();
        tokio::spawn(async move {
            let resp = complete_one_part(
                &client,
                &base,
                "contended.bin",
                &upload_and_etag,
                Some(("If-None-Match", "*")),
            )
            .await;
            (upload, resp.status().as_u16())
        })
    });
    let mut created = Vec::new();
    for complete in completes.collect::<Vec<_>>() {
        let (upload, status) = complete.await.unwrap();
        match status {
            200 => created.push(upload),
            412 => {}
            other => panic!("upload {upload}: unexpected status {other}"),
        }
    }
    assert_eq!(created.len(), 1, "exactly one complete creates the object: {created:?}");

    let get_resp = reqwest::get(format!("{}/contended.bin", base)).await.expect("GET");
    let body = get_resp.bytes().await.unwrap();
    assert_eq!(body.len(), PART_SIZE);
    assert!(
        body.iter().all(|&byte| byte == created[0] as u8),
        "the object is the accepted upload's"
    );
}
