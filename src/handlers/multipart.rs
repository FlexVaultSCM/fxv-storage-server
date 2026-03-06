use crate::AppState;
use crate::etag;
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
    _headers: HeaderMap,
    body: Body,
) -> Response {
    if params.uploads.is_some() {
        create_multipart_upload(state, key).await
    } else if let Some(upload_id) = params.upload_id {
        complete_multipart_upload(state, key, upload_id, body).await
    } else {
        err_invalid_argument("A valid uploads or uploadId query parameter must be provided.")
    }
}

/// CreateMultipartUpload: allocate an upload ID and return it in XML.
async fn create_multipart_upload(state: AppState, key: String) -> Response {
    let upload_id = uuid::Uuid::new_v4().to_string();
    {
        let mut uploads = state.uploads.write().await;
        uploads.insert(
            upload_id.clone(),
            UploadEntry {
                key: key.clone(),
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
            Some(e) if e.key == key => e
                .parts
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect::<HashMap<u32, _>>(),
            Some(_) => {
                return err_invalid_argument("The upload ID is not associated with this key.");
            }
            None => return err_no_such_upload(),
        }
    };

    // Validate that all requested parts exist and have matching ETags
    let mut ordered_parts = Vec::with_capacity(request.parts.len());
    for cp in &request.parts {
        match upload_entry.get(&cp.part_number) {
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

    // Assemble into a temp file, computing BLAKE3 over the full content
    let tmp_path = abs_path.with_extension(format!("{}.fxv_tmp", uuid::Uuid::new_v4().simple()));

    let assemble_result = assemble_parts(
        &ordered_parts
            .iter()
            .map(|(_, p)| p.abs_path.clone())
            .collect::<Vec<_>>(),
        &tmp_path,
    )
    .await;
    let (total_size, final_etag, modified) = match assemble_result {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to assemble parts: {}", e);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return err_internal();
        }
    };

    let mtime_secs = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Acquire the write lock: rename, save ETag cache, and upsert store as a
    // single atomic unit.  Holding the write lock for save_cached_etag serialises
    // all runtime cache file writes (see invariant note in put_object.rs).
    {
        let mut s = state.store.write().await;

        if let Err(e) = tokio::fs::rename(&tmp_path, &abs_path).await {
            warn!("rename {:?} -> {:?} failed: {}", tmp_path, abs_path, e);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return err_internal();
        }

        etag::save_cached_etag(&serve_dir, &rel_path, mtime_secs, &final_etag).await;

        s.upsert(
            key.clone(),
            FileEntry {
                abs_path: abs_path.clone(),
                size: total_size,
                modified,
                etag: final_etag.clone(),
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

/// Concatenate part files into `dst`, computing BLAKE3 of the combined content.
/// Returns `(total_bytes, etag, mtime)`.
async fn assemble_parts(
    part_paths: &[std::path::PathBuf],
    dst: &std::path::Path,
) -> std::io::Result<(u64, String, SystemTime)> {
    let mut file = tokio::fs::File::create(dst).await?;
    let mut hasher = blake3::Hasher::new();
    let mut total: u64 = 0;

    for part_path in part_paths {
        let data = tokio::fs::read(part_path).await?;
        hasher.update(&data);
        total += data.len() as u64;
        file.write_all(&data).await?;
    }

    file.flush().await?;
    drop(file);

    let meta = tokio::fs::metadata(dst).await?;
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let etag = etag::etag_from_hash(hasher.finalize());

    Ok((total, etag, modified))
}

/// DELETE /{*key} - AbortMultipartUpload.
pub async fn delete_dispatch(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(params): Query<DeleteParams>,
    _headers: HeaderMap,
) -> Response {
    match params.upload_id {
        Some(upload_id) => abort_multipart_upload(state, key, upload_id).await,
        None => err_invalid_argument("A valid uploadId query parameter must be provided."),
    }
}

#[derive(Debug, Deserialize)]
pub struct DeleteParams {
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
}

/// AbortMultipartUpload: clean up all temp files and remove upload state.
async fn abort_multipart_upload(state: AppState, key: String, upload_id: String) -> Response {
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
