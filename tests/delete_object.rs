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

/// Puts `content` at `key` and returns the ETag the server gives it.
async fn put_and_get_etag(client: &reqwest::Client, base: &str, key: &str, content: &'static [u8]) -> String {
    let resp = client
        .put(format!("{}/{}", base, key))
        .body(content)
        .send()
        .await
        .expect("put");
    assert_eq!(resp.status(), 200);
    resp.headers()
        .get("etag")
        .expect("etag header")
        .to_str()
        .expect("etag is ASCII")
        .to_string()
}

#[tokio::test]
async fn test_conditional_delete_with_matching_etag_deletes() {
    // Verify If-Match with the object's ETag deletes it, quoted or not.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    for (key, quoted) in [("quoted.txt", true), ("unquoted.txt", false)] {
        let etag = put_and_get_etag(&client, &base, key, b"content").await;
        let if_match = if quoted {
            etag
        } else {
            etag.trim_matches('"').to_string()
        };
        let resp = client
            .delete(format!("{}/{}", base, key))
            .header("If-Match", if_match)
            .send()
            .await
            .expect("delete");
        assert_eq!(resp.status(), 204, "{key}");
        assert!(!dir.path().join(key).exists(), "{key}");
    }
}

#[tokio::test]
async fn test_conditional_delete_of_a_replaced_object_is_412_and_keeps_it() {
    // Verify an ETag read before the object was replaced no longer deletes it.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let old_etag = put_and_get_etag(&client, &base, "replaced.txt", b"first").await;
    put_and_get_etag(&client, &base, "replaced.txt", b"second").await;

    let resp = client
        .delete(format!("{}/replaced.txt", base))
        .header("If-Match", old_etag)
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 412);
    assert_eq!(fs::read(dir.path().join("replaced.txt")).expect("read"), b"second");
}

#[tokio::test]
async fn test_conditional_delete_of_a_missing_object_is_404() {
    // Verify a conditional delete of a missing object is 404, for an ETag and for `*`.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    for if_match in ["\"d41d8cd98f00b204e9800998ecf8427e\"", "*"] {
        let resp = client
            .delete(format!("{}/missing.txt", base))
            .header("If-Match", if_match)
            .send()
            .await
            .expect("delete");
        assert_eq!(resp.status(), 404, "{if_match}");
    }
}

#[tokio::test]
async fn test_conditional_delete_wildcard_deletes_an_existing_object() {
    // Verify If-Match: * deletes an object that exists.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    put_and_get_etag(&client, &base, "any.txt", b"content").await;
    let resp = client
        .delete(format!("{}/any.txt", base))
        .header("If-Match", "*")
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);
    assert!(!dir.path().join("any.txt").exists());
}

#[tokio::test]
async fn test_unreadable_or_repeated_if_match_is_400_and_keeps_the_object() {
    // Verify a condition that can't be evaluated is refused, not treated as no condition.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let etag = put_and_get_etag(&client, &base, "guarded.txt", b"content").await;

    let unreadable = reqwest::header::HeaderValue::from_bytes(b"\"\xff\"").expect("obs-text header value");
    let resp = client
        .delete(format!("{}/guarded.txt", base))
        .header("If-Match", unreadable)
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 400);

    let resp = client
        .delete(format!("{}/guarded.txt", base))
        .header("If-Match", etag.as_str())
        .header("If-Match", "\"another\"")
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 400);

    assert!(dir.path().join("guarded.txt").exists());
}

#[tokio::test]
async fn test_concurrent_conditional_deletes_of_one_etag_delete_once() {
    // Verify the ETag check and the delete are one step: of many deletes of the same ETag, exactly one
    // succeeds and the rest find nothing to delete.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let etag = put_and_get_etag(&client, &base, "contended.txt", b"content").await;
    let mut deletes = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let request = client
            .delete(format!("{}/contended.txt", base))
            .header("If-Match", etag.as_str());
        deletes.spawn(async move { request.send().await.expect("delete").status().as_u16() });
    }
    let statuses: Vec<u16> = deletes.join_all().await;

    assert_eq!(
        statuses.iter().filter(|status| **status == 204).count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses.iter().all(|status| *status == 204 || *status == 404),
        "{statuses:?}"
    );
}

#[tokio::test]
async fn test_every_spelling_of_a_key_names_one_object() {
    // Verify a key with a leading slash and one without are one object for GET, conditional PUT and
    // conditional DELETE, so a condition is never evaluated against another spelling's entry.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let first_etag = put_and_get_etag(&client, &base, "/dup.txt", b"first").await;
    let get = reqwest::get(format!("{}/dup.txt", base)).await.expect("get");
    assert_eq!(get.status(), 200);
    assert_eq!(get.bytes().await.expect("body").as_ref(), b"first");

    let resp = client
        .put(format!("{}/dup.txt", base))
        .header("If-Match", first_etag.as_str())
        .body("second")
        .send()
        .await
        .expect("put");
    assert_eq!(resp.status(), 200);
    let second_etag = resp.headers()["etag"].to_str().expect("etag is ASCII").to_string();

    let resp = client
        .delete(format!("{}//dup.txt", base))
        .header("If-Match", first_etag.as_str())
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 412);
    let resp = client
        .delete(format!("{}//dup.txt", base))
        .header("If-Match", second_etag.as_str())
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204);
    assert!(!dir.path().join("dup.txt").exists());
}

#[tokio::test]
async fn test_an_unquoted_etag_matches_in_every_conditional_request() {
    // Verify GET, PUT and DELETE compare ETags the same way, quoted or not.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();

    let etag = put_and_get_etag(&client, &base, "unquoted.txt", b"first").await;
    let unquoted = etag.trim_matches('"').to_string();

    let get = client
        .get(format!("{}/unquoted.txt", base))
        .header("If-None-Match", unquoted.as_str())
        .send()
        .await
        .expect("get");
    assert_eq!(get.status(), 304);

    let put = client
        .put(format!("{}/unquoted.txt", base))
        .header("If-Match", unquoted.as_str())
        .body("second")
        .send()
        .await
        .expect("put");
    assert_eq!(put.status(), 200);
    let unquoted = put.headers()["etag"]
        .to_str()
        .expect("etag is ASCII")
        .trim_matches('"')
        .to_string();

    let delete = client
        .delete(format!("{}/unquoted.txt", base))
        .header("If-Match", unquoted.as_str())
        .send()
        .await
        .expect("delete");
    assert_eq!(delete.status(), 204);
}
