use crate::store::SharedStore;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct PostParams {
    /// Present for CreateMultipartUpload: `POST /key?uploads`
    pub uploads: Option<String>,
    /// Present for CompleteMultipartUpload: `POST /key?uploadId=<id>`
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
}

/// POST /{*key} — dispatches to CreateMultipartUpload or CompleteMultipartUpload.
pub async fn post_dispatch(
    State(_store): State<SharedStore>,
    Path(_key): Path<String>,
    Query(params): Query<PostParams>,
    _headers: HeaderMap,
    _body: Body,
) -> Response {
    if params.uploads.is_some() {
        // CreateMultipartUpload — implemented in Stage 4
        Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .body(Body::empty())
            .expect("build 501")
    } else if params.upload_id.is_some() {
        // CompleteMultipartUpload — implemented in Stage 4
        Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .body(Body::empty())
            .expect("build 501")
    } else {
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .expect("build 400")
    }
}

/// DELETE /{*key} — AbortMultipartUpload (Stage 5).
pub async fn delete_dispatch(
    State(_store): State<SharedStore>,
    Path(_key): Path<String>,
    _headers: HeaderMap,
) -> Response {
    Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .body(Body::empty())
        .expect("build 501")
}
