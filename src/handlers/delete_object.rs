// == Std
use std::io;

// == Internal
use crate::{
    AppState,
    conditional::{DeleteCondition, evaluate_delete},
    metadata_cache,
    s3_xml_compat::{S3ErrorKind, s3_error},
    store::SharedStore,
};

// == External
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use serde::Deserialize;
use tracing::{debug, warn};

#[derive(Debug, Deserialize)]
pub struct DeleteParams {
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
}

/// DELETE /{*key} - dispatches to DeleteObject or AbortMultipartUpload.
pub async fn delete_dispatch(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(params): Query<DeleteParams>,
    headers: HeaderMap,
) -> Response {
    match params.upload_id {
        Some(upload_id) => crate::handlers::multipart::abort_multipart_upload(state, key, upload_id).await,
        None => match if_match_header(&headers) {
            Ok(if_match) => delete_object(state.store, key, if_match).await,
            Err(message) => s3_error(S3ErrorKind::InvalidArgument(message)),
        },
    }
}

/// The request's `If-Match` value, if it has one. A condition that is present but can't be read (not visible
/// ASCII) or is given more than once is an error rather than no condition: treating it as absent would turn a
/// conditional delete into an unconditional one.
fn if_match_header(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    let mut values = headers.get_all(header::IF_MATCH).iter();
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(value), None) => value
            .to_str()
            .map(|value| Some(value.to_owned()))
            .map_err(|_| "The If-Match header is invalid."),
        (Some(_), Some(_)) => Err("Only one If-Match header is supported."),
    }
}

/// DeleteObject for the non-versioned store. Without `If-Match`, missing objects still return 204,
/// matching S3's idempotent delete behavior. With it, the delete is conditional as S3's conditional
/// deletes are: 412 if the ETag doesn't match, 404 if there is no object (see `evaluate_delete`).
async fn delete_object(store: SharedStore, key: String, if_match: Option<String>) -> Response {
    let rel_path = match crate::handlers::put_object::sanitize_key(&key) {
        Some(path) => path,
        None => return s3_error(S3ErrorKind::InvalidArgument("The specified object key is invalid.")),
    };
    // The store is keyed by the normalized path, so every spelling of it names one object.
    let key = crate::store::object_key(&rel_path);

    // Evaluated under the write lock, so no write can change the object between the check and the delete.
    let mut store = store.write().await;
    let etag = store.get(&key).map(|entry| entry.etag.as_str());
    match evaluate_delete(etag, if_match.as_deref()) {
        DeleteCondition::Proceed => {}
        DeleteCondition::NotFound => return s3_error(S3ErrorKind::NoSuchKey),
        DeleteCondition::PreconditionFailed => return s3_error(S3ErrorKind::PreconditionFailed),
    }
    let serve_dir = store.serve_dir().to_owned();
    let abs_path = serve_dir.join(&rel_path);

    match tokio::fs::remove_file(&abs_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            warn!("DeleteObject remove {:?} failed: {}", abs_path, e);
            return s3_error(S3ErrorKind::Internal);
        }
    }

    if let Err(e) = metadata_cache::remove_metadata_cache(&serve_dir, &rel_path).await
        && e.kind() != io::ErrorKind::NotFound
    {
        warn!("DeleteObject metadata cleanup for {:?} failed: {}", rel_path, e);
    }
    store.remove(&key);

    debug!("DeleteObject {} -> 204", key);
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("build 204")
}
