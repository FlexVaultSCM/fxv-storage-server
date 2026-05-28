// == Std
use std::{
    io,
    path::{Component, Path as FsPath, PathBuf},
    time::SystemTime,
};

// == Internal
use crate::{
    AppState, etag,
    metadata_cache::{self, ChecksumSet, ObjectMetadataCache},
    multipart_state::{MULTIPART_UPLOAD_DIR, PartEntry, SharedUploadState},
    s3_xml_compat::{S3ErrorKind, s3_error},
    store::{FileEntry, SharedStore},
};

// == External
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use md5::{Digest, Md5};
use serde::Deserialize;
use tracing::{debug, warn};

#[derive(Debug, Deserialize)]
pub struct PutParams {
    #[serde(rename = "partNumber")]
    pub part_number: Option<u32>,
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
}

/// PUT /{*key} - dispatches to PutObject or UploadPart based on query params.
pub async fn put_dispatch(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(params): Query<PutParams>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if params.part_number.is_some() || params.upload_id.is_some() {
        upload_part(state, key, params, body).await
    } else {
        put_object(state.store, key, headers, body).await
    }
}

/// PutObject - write a single file atomically.
async fn put_object(store: SharedStore, key: String, headers: HeaderMap, body: Body) -> Response {
    // Resolve the serve directory from the store
    let serve_dir = {
        let s = store.read().await;
        s.serve_dir().to_owned()
    };

    // Sanitise the key against path traversal
    let rel_path = match sanitize_key(&key) {
        Some(p) => p,
        None => return s3_error(S3ErrorKind::InvalidArgument("The specified object key is invalid.")),
    };

    let abs_path = serve_dir.join(&rel_path);
    let user_metadata_headers = metadata_cache::extract_user_metadata_headers(&headers);

    // Parse conditional headers.
    let if_match = headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Early conditional check under read lock: reject obviously-failing requests
    // before streaming the body.  This is a best-effort optimization only - the
    // definitive check happens again under the write lock below to close the TOCTOU
    // window between body receipt and store update.
    {
        let s = store.read().await;
        let existing = s.get(&key);
        match (existing, if_match.as_deref(), if_none_match.as_deref()) {
            (Some(_), _, Some("*")) => return s3_error(S3ErrorKind::PreconditionFailed),
            (Some(entry), Some(im), _) if entry.etag != im && im != "*" => {
                return s3_error(S3ErrorKind::PreconditionFailed);
            }
            (Some(_), Some(_), _) => {}
            (None, Some(_), _) => return s3_error(S3ErrorKind::PreconditionFailed),
            _ => {}
        }
    }

    // Create parent directories if needed
    if let Some(parent) = abs_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| {
                warn!("create_dir_all failed for {:?}: {}", parent, e);
                e
            })
            .ok();
        // If we failed to create dirs, the temp file write below will fail and return 500
    }

    // Write to a temp file in the same directory (so rename is atomic)
    let tmp_path = abs_path.with_extension(format!("{}.fxv_tmp", uuid::Uuid::new_v4().simple()));

    let write_result = write_body_to_temp(&tmp_path, body).await;

    let (tmp_size, etag, md5_bytes, modified) = match write_result {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to write temp file {:?}: {}", tmp_path, e);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return s3_error(S3ErrorKind::Internal);
        }
    };

    let checksums = ChecksumSet {
        md5: etag::md5_hex_from_digest_bytes(&md5_bytes),
        crc32: None,
        crc64nvme: None,
    };
    let object_metadata = ObjectMetadataCache::from_current_state(
        modified,
        etag.clone(),
        checksums.clone(),
        user_metadata_headers.clone(),
    );

    // Acquire the write lock and perform the conditional re-check, rename, cache
    // write, and store upsert as a single atomic unit.
    //
    // Re-checking under the write lock closes the TOCTOU window: no other writer
    // can modify the store entry between this check and the upsert.
    //
    // save_metadata_cache is called while holding the write lock, which
    // serialises all metadata cache file writes - making the non-atomic
    // tokio::fs::write safe.
    {
        let mut s = store.write().await;

        // Definitive conditional check - the store entry may have changed since
        // the early check above.
        let existing = s.get(&key);
        match (existing, if_match.as_deref(), if_none_match.as_deref()) {
            (Some(_), _, Some("*")) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return s3_error(S3ErrorKind::PreconditionFailed);
            }
            (Some(entry), Some(im), _) if entry.etag != im && im != "*" => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return s3_error(S3ErrorKind::PreconditionFailed);
            }
            (None, Some(_), _) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return s3_error(S3ErrorKind::PreconditionFailed);
            }
            _ => {}
        }

        // Atomic rename onto the final path (fast: same-filesystem inode update).
        if let Err(e) = tokio::fs::rename(&tmp_path, &abs_path).await {
            warn!("rename {:?} -> {:?} failed: {}", tmp_path, abs_path, e);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return s3_error(S3ErrorKind::Internal);
        }

        // Persist object metadata to the disk cache. Safe to use tokio::fs::write here
        // because we are the only writer: the write lock is held for the
        // duration and all runtime call-sites of save_metadata_cache hold it.
        metadata_cache::save_metadata_cache(&serve_dir, &rel_path, &object_metadata).await;

        // Update the in-memory index.
        s.upsert(
            key.clone(),
            FileEntry {
                abs_path: abs_path.clone(),
                size: tmp_size,
                modified,
                etag: etag.clone(),
                checksums: checksums.clone(),
                custom_headers: user_metadata_headers.clone(),
            },
        );
    }

    debug!("PUT {} -> 200 (ETag {})", key, etag);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, etag)
        .body(Body::empty())
        .expect("build 200")
}

/// UploadPart - store a single part for a multipart upload.
async fn upload_part(state: AppState, key: String, params: PutParams, body: Body) -> Response {
    let part_number = match params.part_number {
        Some(n) if (1..=10_000).contains(&n) => n,
        _ => return s3_error(S3ErrorKind::InvalidArgument("Part number must be between 1 and 10000.")),
    };
    let upload_id = match params.upload_id {
        Some(id) => id,
        None => return s3_error(S3ErrorKind::InvalidArgument("A valid uploadId must be provided.")),
    };

    // Verify the upload exists and targets this key
    {
        let uploads = state.uploads.read().await;
        match uploads.get(&upload_id) {
            Some(entry) if entry.key == key => {}
            Some(_) => {
                return s3_error(S3ErrorKind::InvalidArgument(
                    "The upload ID is not associated with this key.",
                ));
            }
            None => return s3_error(S3ErrorKind::NoSuchUpload),
        }
    }

    // Determine temp directory: same serve_dir as the store
    let serve_dir = state.store.read().await.serve_dir().to_owned();
    let multipart_dir = serve_dir.join(MULTIPART_UPLOAD_DIR);
    let _ = tokio::fs::create_dir_all(&multipart_dir).await;
    let tmp_path = multipart_dir.join(format!("part-{}-{}.fxv_tmp", upload_id, part_number));

    let write_result = write_body_to_temp(&tmp_path, body).await;
    let (size, etag, md5_bytes, _) = match write_result {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to write part temp file {:?}: {}", tmp_path, e);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return s3_error(S3ErrorKind::Internal);
        }
    };

    // Store part metadata
    {
        let mut uploads = state.uploads.write().await;
        if let Some(entry) = uploads.get_mut(&upload_id) {
            // Remove any previous temp file for this part number
            if let Some(old) = entry.parts.remove(&part_number) {
                let _ = tokio::fs::remove_file(&old.abs_path).await;
            }
            entry.parts.insert(
                part_number,
                PartEntry {
                    abs_path: tmp_path,
                    size,
                    etag: etag.clone(),
                    md5_bytes,
                },
            );
        }
    }

    debug!("UploadPart {} part {} -> ETag {}", upload_id, part_number, etag);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, etag)
        .body(Body::empty())
        .expect("build 200")
}

/// Helper: store upload state shared reference type alias.
#[allow(dead_code)]
pub(crate) type Uploads = SharedUploadState;

/// Stream `body` into `tmp_path`, computing MD5 and capturing metadata.
/// Returns `(file_size, etag, digest_bytes, modified_time)`.
///
/// A blocking writer task owns the file handle and MD5 hasher. The async side
/// drains hyper frames from the network and pushes them through a bounded
/// channel. This pipelines network receive against disk-write + hash so the
/// TCP receive buffer never has to drain to disk before more bytes can arrive -
/// without this, single-stream throughput is gated by sequential
/// receive -> hash -> write cycles.
async fn write_body_to_temp(
    tmp_path: &FsPath,
    body: Body,
) -> io::Result<(u64, String, etag::Md5DigestBytes, SystemTime)> {
    use http_body_util::BodyExt;

    // Channel depth governs how many frames can be in flight between the
    // network reader and the disk writer. Eight is enough to keep the writer
    // fed while bounding peak memory to a few hundred KiB on typical loopback
    // frame sizes.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);

    let writer_path = tmp_path.to_owned();
    let writer = tokio::task::spawn_blocking(move || -> io::Result<(u64, etag::Md5DigestBytes)> {
        use std::io::Write;
        let mut file = std::fs::File::create(&writer_path)?;
        let mut hasher = Md5::new();
        let mut total: u64 = 0;
        while let Some(chunk) = rx.blocking_recv() {
            hasher.update(&chunk);
            file.write_all(&chunk)?;
            total += chunk.len() as u64;
        }
        file.flush()?;
        let digest: etag::Md5DigestBytes = hasher.finalize().into();
        Ok((total, digest))
    });

    let mut body = body;
    let recv_result: io::Result<()> = async {
        while let Some(chunk) = body.frame().await {
            let frame = chunk.map_err(|e| io::Error::other(e.to_string()))?;
            if let Ok(data) = frame.into_data() {
                tx.send(data)
                    .await
                    .map_err(|_| io::Error::other("writer task closed"))?;
            }
        }
        Ok(())
    }
    .await;

    // Signal EOF to the writer, then always wait for it to flush+finalize
    // before surfacing any earlier receive error - this guarantees the temp
    // file is fully closed before the caller unlinks or renames it.
    drop(tx);
    let writer_result = writer
        .await
        .map_err(|e| io::Error::other(format!("writer join: {}", e)))?;

    let (total_bytes, digest) = match writer_result {
        Err(err) => return Err(err),
        Ok(result) => {
            recv_result?;
            result
        }
    };

    let meta = tokio::fs::metadata(tmp_path).await?;
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let etag = etag::etag_from_digest_bytes(&digest);

    Ok((total_bytes, etag, digest, modified))
}

/// Validate a key string and return a `PathBuf` that is safe to join with the serve directory.
/// Returns `None` if the key contains path traversal components.
pub fn sanitize_key(key: &str) -> Option<PathBuf> {
    let p = PathBuf::from(key.trim_start_matches('/'));
    for component in p.components() {
        match component {
            Component::Normal(_) => {}
            _ => return None, // Reject .., /, RootDir, Prefix
        }
    }
    if p.as_os_str().is_empty() {
        return None;
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_key_valid() {
        // Verify normal relative object keys are accepted.
        assert!(sanitize_key("a/b/c.txt").is_some());
        assert!(sanitize_key("file.txt").is_some());
        assert!(sanitize_key("deep/nested/path/file.bin").is_some());
    }

    #[test]
    fn test_sanitize_key_traversal_rejected() {
        // Verify traversal attempts are rejected.
        assert!(sanitize_key("../secret").is_none());
        assert!(sanitize_key("a/../../etc/passwd").is_none());

        // Verify a leading slash is normalized away before validation.
        assert!(sanitize_key("/absolute").is_some());
    }

    #[test]
    fn test_sanitize_key_empty_rejected() {
        // Verify empty keys are rejected after trimming leading slashes.
        assert!(sanitize_key("").is_none());
        assert!(sanitize_key("/").is_none());
    }
}
