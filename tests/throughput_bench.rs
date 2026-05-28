//! Throughput micro-benchmarks for the upload pipeline.
//!
//! Each #[ignore]'d test measures one layer in isolation and prints MB/s.
//! Run with:
//!   cargo test --release --test throughput_bench -- --ignored --nocapture --test-threads=1
//!
//! Layers measured, in increasing scope:
//!   1. Pure MD5 over an in-memory buffer (cpu-bound sanity check)
//!   2. std::fs file write with std::io::Write
//!   3. tokio::fs file write at different chunk sizes
//!   4. std::fs write + interleaved MD5
//!   5. PutObject over HTTP (whole upload pipeline)
//!   6. UploadPart over HTTP (single multipart part)
//!   7. Full multipart upload (CreateMultipartUpload + N x UploadPart + Complete)
//!   8. CompleteMultipartUpload only (assemble pre-uploaded parts)

mod common;

// == Std
use std::{
    fs,
    io::Write as _,
    sync::Arc,
    time::{Duration, Instant},
};

// == Internal
use common::spawn_server;

// == External
use md5::{Digest, Md5};
use tokio::io::AsyncWriteExt as _;

// Size of the payload used across throughput tests. Large enough that fixed
// per-test overhead is dwarfed by the measured body.
const PAYLOAD_BYTES: usize = 256 * 1024 * 1024; // 256 MiB
const PART_BYTES: usize = 16 * 1024 * 1024; // 16 MiB per multipart part

fn mb_per_sec(bytes: usize, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 { f64::INFINITY } else { (bytes as f64 / (1024.0 * 1024.0)) / secs }
}

fn print_result(label: &str, bytes: usize, elapsed: Duration) {
    println!(
        "  {:<48} {:>10.2} MB/s  ({} bytes in {:?})",
        label,
        mb_per_sec(bytes, elapsed),
        bytes,
        elapsed
    );
}

fn make_payload(size: usize) -> Vec<u8> {
    // Pseudo-random but deterministic so MD5 isn't trivially compressible /
    // disk caches don't cheat with zero-page tricks.
    let mut v = Vec::with_capacity(size);
    let mut x: u32 = 0x9E3779B1;
    for _ in 0..size {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        v.push((x >> 24) as u8);
    }
    v
}

// =============================================================================
// 1. Pure MD5
// =============================================================================

#[test]
#[ignore = "benchmark"]
fn bench_01_md5_only() {
    println!("\n=== bench_01_md5_only ===");
    let payload = make_payload(PAYLOAD_BYTES);

    // Single update on the entire buffer (best case).
    let start = Instant::now();
    let mut h = Md5::new();
    h.update(&payload);
    let _ = h.finalize();
    print_result("md5 single update over whole buffer", PAYLOAD_BYTES, start.elapsed());

    // 1 MiB chunk loop (mirrors assemble_parts).
    let start = Instant::now();
    let mut h = Md5::new();
    for chunk in payload.chunks(1024 * 1024) {
        h.update(chunk);
    }
    let _ = h.finalize();
    print_result("md5 1 MiB chunks", PAYLOAD_BYTES, start.elapsed());

    // 8 KiB chunk loop (what hyper might feed us per frame on loopback).
    let start = Instant::now();
    let mut h = Md5::new();
    for chunk in payload.chunks(8 * 1024) {
        h.update(chunk);
    }
    let _ = h.finalize();
    print_result("md5 8 KiB chunks", PAYLOAD_BYTES, start.elapsed());
}

// =============================================================================
// 2. Sync file write
//
// No fsync/sync_data: the server never forces data to physical media before
// responding (it writes to the page cache, then renames), so client-observed
// throughput is page-cache-bound. We measure the same thing here so these
// numbers are comparable to the tokio path and to the HTTP benchmarks below.
// =============================================================================

#[test]
#[ignore = "benchmark"]
fn bench_02_std_write() {
    println!("\n=== bench_02_std_write ===");
    let dir = tempfile::tempdir().expect("tempdir");
    let payload = make_payload(PAYLOAD_BYTES);

    for &chunk in &[8 * 1024usize, 64 * 1024, 1024 * 1024, 16 * 1024 * 1024] {
        let path = dir.path().join(format!("std_{}.bin", chunk));
        let start = Instant::now();
        let mut f = fs::File::create(&path).expect("create");
        for c in payload.chunks(chunk) {
            f.write_all(c).expect("write");
        }
        drop(f);
        print_result(
            &format!("std::fs write, chunk={} KiB", chunk / 1024),
            PAYLOAD_BYTES,
            start.elapsed(),
        );
    }
}

// =============================================================================
// 3. Tokio fs write
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark"]
async fn bench_03_tokio_write() {
    println!("\n=== bench_03_tokio_write ===");
    let dir = tempfile::tempdir().expect("tempdir");
    let payload = make_payload(PAYLOAD_BYTES);

    for &chunk in &[8 * 1024usize, 64 * 1024, 1024 * 1024, 16 * 1024 * 1024] {
        let path = dir.path().join(format!("tokio_{}.bin", chunk));
        let start = Instant::now();
        let mut f = tokio::fs::File::create(&path).await.expect("create");
        for c in payload.chunks(chunk) {
            f.write_all(c).await.expect("write");
        }
        f.flush().await.expect("flush");
        drop(f);
        print_result(
            &format!("tokio::fs write, chunk={} KiB", chunk / 1024),
            PAYLOAD_BYTES,
            start.elapsed(),
        );
    }
}

// =============================================================================
// 4. Interleaved MD5 + write
// =============================================================================

#[test]
#[ignore = "benchmark"]
fn bench_04_std_write_plus_md5() {
    println!("\n=== bench_04_std_write_plus_md5 ===");
    let dir = tempfile::tempdir().expect("tempdir");
    let payload = make_payload(PAYLOAD_BYTES);

    for &chunk in &[64 * 1024usize, 1024 * 1024] {
        let path = dir.path().join(format!("hash_{}.bin", chunk));
        let start = Instant::now();
        let mut f = fs::File::create(&path).expect("create");
        let mut h = Md5::new();
        for c in payload.chunks(chunk) {
            h.update(c);
            f.write_all(c).expect("write");
        }
        // No fsync (see bench_02): matches the server's page-cache write path,
        // so this is a fair ceiling for the receive -> hash -> write pipeline.
        let _ = h.finalize();
        drop(f);
        print_result(
            &format!("std::fs write + md5, chunk={} KiB", chunk / 1024),
            PAYLOAD_BYTES,
            start.elapsed(),
        );
    }
}

// =============================================================================
// 5. Single PutObject over HTTP
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark"]
async fn bench_05_put_object() {
    println!("\n=== bench_05_put_object ===");
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();
    let payload = Arc::new(make_payload(PAYLOAD_BYTES));

    // Warm-up
    let _ = client
        .put(format!("{}/warmup.bin", base))
        .body(b"warmup".to_vec())
        .send()
        .await
        .expect("warmup");

    for run in 0..3 {
        let key = format!("put-{}.bin", run);
        let start = Instant::now();
        let resp = client
            .put(format!("{}/{}", base, key))
            .body(payload.as_ref().clone())
            .send()
            .await
            .expect("PUT");
        assert_eq!(resp.status(), 200);
        print_result(&format!("PutObject run {}", run), PAYLOAD_BYTES, start.elapsed());
    }
}

// =============================================================================
// 6. Single UploadPart over HTTP
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark"]
async fn bench_06_upload_part() {
    println!("\n=== bench_06_upload_part ===");
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();
    let payload = Arc::new(make_payload(PAYLOAD_BYTES));

    let create_resp = client
        .post(format!("{}/single-part.bin?uploads", base))
        .send()
        .await
        .expect("create");
    let body = create_resp.text().await.expect("body");
    let upload_id = parse_upload_id(&body);

    for run in 0..3 {
        let part = run + 1;
        let start = Instant::now();
        let resp = client
            .put(format!("{}/single-part.bin?partNumber={}&uploadId={}", base, part, upload_id))
            .body(payload.as_ref().clone())
            .send()
            .await
            .expect("upload part");
        assert_eq!(resp.status(), 200);
        print_result(
            &format!("UploadPart run {} (single {} MiB part)", run, PAYLOAD_BYTES / (1024 * 1024)),
            PAYLOAD_BYTES,
            start.elapsed(),
        );
    }
}

// =============================================================================
// 7. Full multipart upload
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark"]
async fn bench_07_multipart_full() {
    println!("\n=== bench_07_multipart_full ===");
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();
    let payload = Arc::new(make_payload(PAYLOAD_BYTES));

    let n_parts = PAYLOAD_BYTES.div_ceil(PART_BYTES);
    println!(
        "  payload={} MiB, part={} MiB, n_parts={}",
        PAYLOAD_BYTES / (1024 * 1024),
        PART_BYTES / (1024 * 1024),
        n_parts
    );

    // Sequential parts.
    {
        let key = "multi-seq.bin";
        let create = client
            .post(format!("{}/{}?uploads", base, key))
            .send()
            .await
            .expect("create");
        let upload_id = parse_upload_id(&create.text().await.expect("body"));

        let start_total = Instant::now();
        let start_parts = Instant::now();
        let mut etags = Vec::with_capacity(n_parts);
        for i in 0..n_parts {
            let begin = i * PART_BYTES;
            let end = ((i + 1) * PART_BYTES).min(PAYLOAD_BYTES);
            let body = payload[begin..end].to_vec();
            let resp = client
                .put(format!("{}/{}?partNumber={}&uploadId={}", base, key, i + 1, upload_id))
                .body(body)
                .send()
                .await
                .expect("upload part");
            assert_eq!(resp.status(), 200);
            etags.push(resp.headers().get("etag").unwrap().to_str().unwrap().to_owned());
        }
        let parts_elapsed = start_parts.elapsed();

        let start_complete = Instant::now();
        let xml = build_complete_xml(&etags);
        let resp = client
            .post(format!("{}/{}?uploadId={}", base, key, upload_id))
            .header("Content-Type", "application/xml")
            .body(xml)
            .send()
            .await
            .expect("complete");
        assert_eq!(resp.status(), 200);
        let complete_elapsed = start_complete.elapsed();
        let total_elapsed = start_total.elapsed();

        print_result("multipart sequential: UploadPart phase", PAYLOAD_BYTES, parts_elapsed);
        print_result("multipart sequential: CompleteMultipartUpload phase", PAYLOAD_BYTES, complete_elapsed);
        print_result("multipart sequential: end-to-end", PAYLOAD_BYTES, total_elapsed);
    }

    // Concurrent parts (8 in flight).
    {
        let key = "multi-conc.bin";
        let create = client
            .post(format!("{}/{}?uploads", base, key))
            .send()
            .await
            .expect("create");
        let upload_id = parse_upload_id(&create.text().await.expect("body"));

        let start_total = Instant::now();
        let start_parts = Instant::now();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(8));
        let mut handles = Vec::with_capacity(n_parts);
        for i in 0..n_parts {
            let begin = i * PART_BYTES;
            let end = ((i + 1) * PART_BYTES).min(PAYLOAD_BYTES);
            let body = payload[begin..end].to_vec();
            let url = format!("{}/{}?partNumber={}&uploadId={}", base, key, i + 1, upload_id);
            let client = client.clone();
            let permit_src = semaphore.clone();
            handles.push(tokio::spawn(async move {
                let _permit = permit_src.acquire_owned().await.expect("permit");
                let resp = client.put(url).body(body).send().await.expect("upload part");
                assert_eq!(resp.status(), 200);
                (i, resp.headers().get("etag").unwrap().to_str().unwrap().to_owned())
            }));
        }
        let mut etags: Vec<(usize, String)> = Vec::with_capacity(n_parts);
        for h in handles {
            etags.push(h.await.expect("part task"));
        }
        etags.sort_by_key(|(i, _)| *i);
        let etags: Vec<String> = etags.into_iter().map(|(_, e)| e).collect();
        let parts_elapsed = start_parts.elapsed();

        let start_complete = Instant::now();
        let xml = build_complete_xml(&etags);
        let resp = client
            .post(format!("{}/{}?uploadId={}", base, key, upload_id))
            .header("Content-Type", "application/xml")
            .body(xml)
            .send()
            .await
            .expect("complete");
        assert_eq!(resp.status(), 200);
        let complete_elapsed = start_complete.elapsed();
        let total_elapsed = start_total.elapsed();

        print_result("multipart concurrent(8): UploadPart phase", PAYLOAD_BYTES, parts_elapsed);
        print_result("multipart concurrent(8): CompleteMultipartUpload phase", PAYLOAD_BYTES, complete_elapsed);
        print_result("multipart concurrent(8): end-to-end", PAYLOAD_BYTES, total_elapsed);
    }
}

// =============================================================================
// 8. CompleteMultipartUpload (assembly) only
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark"]
async fn bench_08_complete_only() {
    println!("\n=== bench_08_complete_only ===");
    let dir = tempfile::tempdir().expect("tempdir");
    let server = spawn_server(dir.path());
    let base = server.url();
    let client = reqwest::Client::new();
    let payload = Arc::new(make_payload(PAYLOAD_BYTES));

    let key = "complete-only.bin";
    let create = client
        .post(format!("{}/{}?uploads", base, key))
        .send()
        .await
        .expect("create");
    let upload_id = parse_upload_id(&create.text().await.expect("body"));

    let n_parts = PAYLOAD_BYTES.div_ceil(PART_BYTES);
    let mut etags = Vec::with_capacity(n_parts);
    for i in 0..n_parts {
        let begin = i * PART_BYTES;
        let end = ((i + 1) * PART_BYTES).min(PAYLOAD_BYTES);
        let body = payload[begin..end].to_vec();
        let resp = client
            .put(format!("{}/{}?partNumber={}&uploadId={}", base, key, i + 1, upload_id))
            .body(body)
            .send()
            .await
            .expect("upload part");
        assert_eq!(resp.status(), 200);
        etags.push(resp.headers().get("etag").unwrap().to_str().unwrap().to_owned());
    }

    let xml = build_complete_xml(&etags);
    let start = Instant::now();
    let resp = client
        .post(format!("{}/{}?uploadId={}", base, key, upload_id))
        .header("Content-Type", "application/xml")
        .body(xml)
        .send()
        .await
        .expect("complete");
    assert_eq!(resp.status(), 200);
    print_result(
        &format!("CompleteMultipartUpload over {} pre-uploaded parts", n_parts),
        PAYLOAD_BYTES,
        start.elapsed(),
    );
}

// =============================================================================
// Helpers
// =============================================================================

fn parse_upload_id(xml: &str) -> String {
    let start_tag = "<UploadId>";
    let end_tag = "</UploadId>";
    let start = xml.find(start_tag).expect("UploadId start") + start_tag.len();
    let end = xml.find(end_tag).expect("UploadId end");
    xml[start..end].to_owned()
}

fn build_complete_xml(etags: &[String]) -> String {
    let mut s = String::from(r#"<?xml version="1.0"?><CompleteMultipartUpload>"#);
    for (i, etag) in etags.iter().enumerate() {
        s.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            i + 1,
            etag
        ));
    }
    s.push_str("</CompleteMultipartUpload>");
    s
}
