use crate::AppState;
use crate::etag;
use crate::metadata_cache::{self, ChecksumSet, ObjectMetadataCache};
use crate::multipart_state::UploadEntry;
use crate::s3_xml_compat::{
    CompleteMultipartUpload, CompleteMultipartUploadResult, InitiateMultipartUploadResult,
    err_internal, err_invalid_argument, err_invalid_part, err_malformed_xml, err_no_such_upload,
    from_xml_bytes, to_xml_bytes,
};
use crate::store::FileEntry;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use md5::{Digest, Md5};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

#[derive(Debug, Deserialize)]
pub struct PostParams {
    /// Present for CreateMultipartUpload: `POST /key?uploads`
    pub uploads: Option<String>,
    /// Present for CompleteMultipartUpload: `POST /key?uploadId=<id>`
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
}

/// POST /{*key} - dispatches to CreateMultipartUpload or CompleteMultipartUpload.
pub async fn post_dispatch(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(params): Query<PostParams>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if params.uploads.is_some() {
        create_multipart_upload(state, key, headers).await
    } else if let Some(upload_id) = params.upload_id {
        complete_multipart_upload(state, key, upload_id, body).await
    } else {
        err_invalid_argument("A valid uploads or uploadId query parameter must be provided.")
    }
}

/// CreateMultipartUpload: allocate an upload ID and return it in XML.
async fn create_multipart_upload(state: AppState, key: String, headers: HeaderMap) -> Response {
    let upload_id = uuid::Uuid::new_v4().to_string();
    let custom_headers = metadata_cache::extract_user_metadata_headers(&headers);
    {
        let mut uploads = state.uploads.write().await;
        uploads.insert(
            upload_id.clone(),
            UploadEntry {
                key: key.clone(),
                custom_headers,
                parts: HashMap::new(),
            },
        );
    }

    let result = InitiateMultipartUploadResult {
        key,
        upload_id: upload_id.clone(),
    };
    let xml = match to_xml_bytes(&result) {
        Ok(b) => b,
        Err(e) => {
            warn!("XML serialise error: {}", e);
            return err_internal();
        }
    };

    debug!("CreateMultipartUpload -> uploadId {}", upload_id);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(xml))
        .expect("build 200")
}

/// CompleteMultipartUpload: assemble parts atomically, update store.
async fn complete_multipart_upload(
    state: AppState,
    key: String,
    upload_id: String,
    body: Body,
) -> Response {
    // Collect request body
    let body_bytes = match collect_body(body).await {
        Ok(b) => b,
        Err(e) => {
            warn!("Failed to read CompleteMultipartUpload body: {}", e);
            return err_malformed_xml();
        }
    };

    // Parse XML request
    let request: CompleteMultipartUpload = match from_xml_bytes(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to parse CompleteMultipartUpload XML: {}", e);
            return err_malformed_xml();
        }
    };

    // Validate upload exists for this key
    let upload_entry = {
        let uploads = state.uploads.read().await;
        match uploads.get(&upload_id) {
            Some(e) if e.key == key => e.clone(),
            Some(_) => {
                return err_invalid_argument("The upload ID is not associated with this key.");
            }
            None => return err_no_such_upload(),
        }
    };

    // Validate that all requested parts exist and have matching ETags
    let mut ordered_parts = Vec::with_capacity(request.parts.len());
    for cp in &request.parts {
        match upload_entry.parts.get(&cp.part_number) {
            Some(pe) if pe.etag == cp.etag => ordered_parts.push((cp.part_number, pe.clone())),
            Some(_) | None => return err_invalid_part(),
        }
    }
    ordered_parts.sort_by_key(|(n, _)| *n);

    // Determine destination path
    let serve_dir = state.store.read().await.serve_dir().to_owned();
    let rel_path = match crate::handlers::put_object::sanitize_key(&key) {
        Some(p) => p,
        None => return err_invalid_argument("The specified object key is invalid."),
    };
    let abs_path = serve_dir.join(&rel_path);

    if let Some(parent) = abs_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    // Assemble into a temp file while preserving the S3 multipart ETag formula.
    let tmp_path = abs_path.with_extension(format!("{}.fxv_tmp", uuid::Uuid::new_v4().simple()));

    let assemble_result = assemble_parts(&ordered_parts, &tmp_path).await;
    let (total_size, final_etag, md5_hex, modified) = match assemble_result {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to assemble parts: {}", e);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return err_internal();
        }
    };

    let checksums = ChecksumSet {
        md5: md5_hex,
        crc32: None,
        crc64nvme: None,
    };
    let object_metadata = ObjectMetadataCache::from_current_state(
        modified,
        final_etag.clone(),
        checksums.clone(),
        upload_entry.custom_headers.clone(),
    );

    // Acquire the write lock: rename, save metadata cache, and upsert store as
    // a single atomic unit. Holding the write lock for save_metadata_cache
    // serialises all runtime cache file writes (see invariant note in
    // put_object.rs).
    {
        let mut s = state.store.write().await;

        if let Err(e) = tokio::fs::rename(&tmp_path, &abs_path).await {
            warn!("rename {:?} -> {:?} failed: {}", tmp_path, abs_path, e);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return err_internal();
        }

        metadata_cache::save_metadata_cache(&serve_dir, &rel_path, &object_metadata).await;

        s.upsert(
            key.clone(),
            FileEntry {
                abs_path: abs_path.clone(),
                size: total_size,
                modified,
                etag: final_etag.clone(),
                checksums: checksums.clone(),
                custom_headers: upload_entry.custom_headers.clone(),
            },
        );
    }

    // Clean up part temp files and remove upload state
    {
        let mut uploads = state.uploads.write().await;
        if let Some(entry) = uploads.remove(&upload_id) {
            for (_, part) in entry.parts {
                let _ = tokio::fs::remove_file(&part.abs_path).await;
            }
        }
    }

    let result_xml = CompleteMultipartUploadResult {
        key: key.clone(),
        etag: final_etag.clone(),
    };
    let xml = match to_xml_bytes(&result_xml) {
        Ok(b) => b,
        Err(e) => {
            warn!("XML serialise error: {}", e);
            return err_internal();
        }
    };

    debug!("CompleteMultipartUpload {} -> ETag {}", key, final_etag);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml")
        .header(header::ETAG, final_etag)
        .body(Body::from(xml))
        .expect("build 200")
}

/// Collect all body frames into a single byte buffer.
async fn collect_body(body: Body) -> std::io::Result<Bytes> {
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Concatenate part files into `dst`, computing the final S3 multipart ETag.
/// Returns `(total_bytes, etag, md5_hex, mtime)`.
async fn assemble_parts(
    parts: &[(u32, crate::multipart_state::PartEntry)],
    dst: &std::path::Path,
) -> std::io::Result<(u64, String, String, SystemTime)> {
    let mut file = tokio::fs::File::create(dst).await?;
    let mut md5_hasher = Md5::new();
    let mut total: u64 = 0;
    let mut part_digests = Vec::with_capacity(parts.len());

    for (_, part) in parts {
        let data = tokio::fs::read(&part.abs_path).await?;
        debug_assert_eq!(part.size, data.len() as u64);
        total += part.size;
        md5_hasher.update(&data);
        file.write_all(&data).await?;
        part_digests.push(part.md5_bytes);
    }

    file.flush().await?;
    drop(file);

    let meta = tokio::fs::metadata(dst).await?;
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let etag = etag::multipart_etag_from_part_digests(&part_digests);
    let md5_hex = etag::md5_hex_from_digest_bytes(&md5_hasher.finalize().into());

    Ok((total, etag, md5_hex, modified))
}

/// AbortMultipartUpload: clean up all temp files and remove upload state.
pub(crate) async fn abort_multipart_upload(
    state: AppState,
    key: String,
    upload_id: String,
) -> Response {
    let mut uploads = state.uploads.write().await;
    match uploads.remove(&upload_id) {
        Some(entry) if entry.key == key => {
            // Clean up part temp files
            for (_, part) in entry.parts {
                let _ = tokio::fs::remove_file(&part.abs_path).await;
            }
            debug!("AbortMultipartUpload {} -> 204", upload_id);
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .expect("build 204")
        }
        Some(entry) => {
            // Key mismatch - re-insert the entry and return error
            uploads.insert(upload_id, entry);
            err_invalid_argument("The upload ID is not associated with this key.")
        }
        None => err_no_such_upload(),
    }
}
