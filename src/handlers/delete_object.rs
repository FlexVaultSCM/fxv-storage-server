use crate::AppState;
use crate::etag;
use crate::s3_xml_compat::{err_internal, err_invalid_argument};
use crate::store::SharedStore;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
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
    _headers: HeaderMap,
) -> Response {
    match params.upload_id {
        Some(upload_id) => {
            crate::handlers::multipart::abort_multipart_upload(state, key, upload_id).await
        }
        None => delete_object(state.store, key).await,
    }
}

/// DeleteObject for the non-versioned store. Missing objects still return 204,
/// matching S3's idempotent delete behavior.
async fn delete_object(store: SharedStore, key: String) -> Response {
    let rel_path = match crate::handlers::put_object::sanitize_key(&key) {
        Some(path) => path,
        None => return err_invalid_argument("The specified object key is invalid."),
    };

    let mut store = store.write().await;
    let serve_dir = store.serve_dir().to_owned();
    let abs_path = serve_dir.join(&rel_path);

    match tokio::fs::remove_file(&abs_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            warn!("DeleteObject remove {:?} failed: {}", abs_path, e);
            return err_internal();
        }
    }

    if let Err(e) = etag::remove_cached_etag(&serve_dir, &rel_path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            "DeleteObject cache cleanup for {:?} failed: {}",
            rel_path, e
        );
    }

    store.remove(&key);

    debug!("DeleteObject {} -> 204", key);
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("build 204")
}
