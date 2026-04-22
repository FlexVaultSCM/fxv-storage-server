use aws_sdk_s3::Client;
/// S3 client compatibility tests.
///
/// Spins up a real fxv-storage-server instance and drives it with the
/// official `aws-sdk-s3` Rust client using path-style addressing.
///
/// With path-style, the SDK sends:
///   PUT  http://host/BUCKET/key
///   GET  http://host/BUCKET/key
///   etc.
///
/// Our server exposes a single `/{*key}` wildcard, so "BUCKET/key" is
/// captured as the key and the file lands at `<serve_dir>/BUCKET/key`.
/// That is intentional - this test validates wire-level compatibility,
/// not bucket semantics.
use aws_sdk_s3::config::{Builder as S3ConfigBuilder, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use std::net::SocketAddr;
use std::path::PathBuf;

const BUCKET: &str = "test-bucket";

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

fn s3_client(endpoint_url: &str) -> Client {
    let creds = Credentials::new("test-access-key", "test-secret-key", None, None, "static");
    let config = S3ConfigBuilder::new()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(creds)
        .endpoint_url(endpoint_url)
        .force_path_style(true)
        .build();
    Client::from_conf(config)
}

// == PutObject

#[tokio::test]
async fn s3_put_object_and_get_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = s3_client(&base);

    // PutObject
    let put_resp = client
        .put_object()
        .bucket(BUCKET)
        .key("hello.txt")
        .body(ByteStream::from_static(b"hello from s3 client"))
        .send()
        .await
        .expect("PutObject");

    let etag = put_resp.e_tag().expect("ETag in PutObject response");
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "ETag should be double-quoted, got: {}",
        etag
    );

    // GetObject
    let get_resp = client
        .get_object()
        .bucket(BUCKET)
        .key("hello.txt")
        .send()
        .await
        .expect("GetObject");

    let body = get_resp
        .body
        .collect()
        .await
        .expect("collect body")
        .into_bytes();
    assert_eq!(body.as_ref(), b"hello from s3 client");
}

// == DeleteObject

#[tokio::test]
async fn s3_delete_object_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = s3_client(&base);

    client
        .put_object()
        .bucket(BUCKET)
        .key("delete-me.txt")
        .body(ByteStream::from_static(b"bye"))
        .send()
        .await
        .expect("PutObject");

    client
        .delete_object()
        .bucket(BUCKET)
        .key("delete-me.txt")
        .send()
        .await
        .expect("DeleteObject");

    let result = client
        .get_object()
        .bucket(BUCKET)
        .key("delete-me.txt")
        .send()
        .await;
    assert!(result.is_err(), "Object should not exist after delete");
}

// == GetObject: 404 for missing key

/// The server now returns S3-format XML error bodies, so the SDK maps 404 -> NoSuchKey.
#[tokio::test]
async fn s3_get_object_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = s3_client(&base);

    let result = client
        .get_object()
        .bucket(BUCKET)
        .key("nonexistent.txt")
        .send()
        .await;

    assert!(result.is_err(), "Expected error for missing key");
    let err = result.unwrap_err();
    let svc_err = err.as_service_error().expect("expected service error");
    assert!(
        svc_err.is_no_such_key(),
        "Expected NoSuchKey, got: {:?}",
        svc_err
    );
}

// == GetObject: ETag conditional (If-None-Match)

#[tokio::test]
async fn s3_get_object_if_none_match_304() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = s3_client(&base);

    // Upload the object
    let put = client
        .put_object()
        .bucket(BUCKET)
        .key("cond.txt")
        .body(ByteStream::from_static(b"data"))
        .send()
        .await
        .expect("PutObject");
    let etag = put.e_tag().expect("etag").to_owned();

    // GET with matching If-None-Match -> SDK should surface a NotModified error
    let result = client
        .get_object()
        .bucket(BUCKET)
        .key("cond.txt")
        .if_none_match(&etag)
        .send()
        .await;

    // The SDK may surface 304 as a NotModified service error or as an Ok with no body.
    // Either is acceptable for compatibility.
    match result {
        Err(e) => {
            // Verify it's a 304-related error
            let raw = e.raw_response().expect("raw response");
            assert_eq!(
                raw.status().as_u16(),
                304,
                "Expected 304 Not Modified, got: {}",
                raw.status()
            );
        }
        Ok(resp) => {
            // Some SDK versions return Ok with empty body for 304
            let body = resp.body.collect().await.expect("body").into_bytes();
            assert!(body.is_empty(), "304 response should have empty body");
        }
    }
}

// == GetObject: Range request

#[tokio::test]
async fn s3_get_object_range() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = s3_client(&base);

    client
        .put_object()
        .bucket(BUCKET)
        .key("range.bin")
        .body(ByteStream::from_static(b"0123456789"))
        .send()
        .await
        .expect("PutObject");

    let get_resp = client
        .get_object()
        .bucket(BUCKET)
        .key("range.bin")
        .range("bytes=2-5")
        .send()
        .await
        .expect("GetObject with range");

    assert_eq!(get_resp.content_length(), Some(4));
    let body = get_resp.body.collect().await.expect("body").into_bytes();
    assert_eq!(body.as_ref(), b"2345");
}

// == PutObject: If-None-Match: *

#[tokio::test]
async fn s3_put_object_if_none_match_star_prevents_overwrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = s3_client(&base);

    // First write succeeds
    client
        .put_object()
        .bucket(BUCKET)
        .key("exclusive.txt")
        .body(ByteStream::from_static(b"first"))
        .send()
        .await
        .expect("first PutObject");

    // Second write with If-None-Match: * should fail with 412
    let result = client
        .put_object()
        .bucket(BUCKET)
        .key("exclusive.txt")
        .if_none_match("*")
        .body(ByteStream::from_static(b"second"))
        .send()
        .await;

    assert!(result.is_err(), "Expected 412 error");
    let raw = result
        .unwrap_err()
        .raw_response()
        .expect("raw response")
        .status()
        .as_u16();
    assert_eq!(raw, 412, "Expected 412 Precondition Failed");
}

// == Multipart Upload: full flow

#[tokio::test]
async fn s3_multipart_upload_full_flow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = s3_client(&base);

    // 1. CreateMultipartUpload
    let create = client
        .create_multipart_upload()
        .bucket(BUCKET)
        .key("multipart.bin")
        .send()
        .await
        .expect("CreateMultipartUpload");
    let upload_id = create.upload_id().expect("upload_id").to_owned();
    assert!(!upload_id.is_empty());

    // 2. UploadPart 1
    let part1 = client
        .upload_part()
        .bucket(BUCKET)
        .key("multipart.bin")
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from_static(b"hello "))
        .send()
        .await
        .expect("UploadPart 1");
    let etag1 = part1.e_tag().expect("etag1").to_owned();

    // 3. UploadPart 2
    let part2 = client
        .upload_part()
        .bucket(BUCKET)
        .key("multipart.bin")
        .upload_id(&upload_id)
        .part_number(2)
        .body(ByteStream::from_static(b"world"))
        .send()
        .await
        .expect("UploadPart 2");
    let etag2 = part2.e_tag().expect("etag2").to_owned();

    // 4. CompleteMultipartUpload
    let completed = CompletedMultipartUpload::builder()
        .parts(
            CompletedPart::builder()
                .part_number(1)
                .e_tag(&etag1)
                .build(),
        )
        .parts(
            CompletedPart::builder()
                .part_number(2)
                .e_tag(&etag2)
                .build(),
        )
        .build();

    let complete = client
        .complete_multipart_upload()
        .bucket(BUCKET)
        .key("multipart.bin")
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("CompleteMultipartUpload");

    let final_etag = complete.e_tag().expect("final etag");
    assert!(
        final_etag.starts_with('"') && final_etag.ends_with('"'),
        "ETag should be double-quoted"
    );
    assert!(final_etag.contains("-2"));

    // 5. Verify assembled content
    let body = client
        .get_object()
        .bucket(BUCKET)
        .key("multipart.bin")
        .send()
        .await
        .expect("GetObject after multipart")
        .body
        .collect()
        .await
        .expect("body")
        .into_bytes();
    assert_eq!(body.as_ref(), b"hello world");
}

// == AbortMultipartUpload

#[tokio::test]
async fn s3_abort_multipart_upload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = spawn_server(dir.path().to_owned()).await;
    let client = s3_client(&base);

    // Create upload
    let create = client
        .create_multipart_upload()
        .bucket(BUCKET)
        .key("abortable.bin")
        .send()
        .await
        .expect("CreateMultipartUpload");
    let upload_id = create.upload_id().expect("upload_id").to_owned();

    // Upload a part
    client
        .upload_part()
        .bucket(BUCKET)
        .key("abortable.bin")
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from_static(b"data"))
        .send()
        .await
        .expect("UploadPart");

    // Abort
    client
        .abort_multipart_upload()
        .bucket(BUCKET)
        .key("abortable.bin")
        .upload_id(&upload_id)
        .send()
        .await
        .expect("AbortMultipartUpload");

    // Object should not exist
    let result = client
        .get_object()
        .bucket(BUCKET)
        .key("abortable.bin")
        .send()
        .await;
    assert!(result.is_err(), "Object should not exist after abort");
}
