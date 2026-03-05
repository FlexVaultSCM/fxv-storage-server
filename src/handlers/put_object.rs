use crate::store::SharedStore;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct PutParams {
    #[serde(rename = "partNumber")]
    pub part_number: Option<u32>,
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
}

/// PUT /{*key} — dispatches to PutObject or UploadPart based on query params.
pub async fn put_dispatch(
    State(_store): State<SharedStore>,
    Path(_key): Path<String>,
    Query(params): Query<PutParams>,
    _headers: HeaderMap,
    _body: Body,
) -> Response {
    if params.part_number.is_some() || params.upload_id.is_some() {
        // UploadPart — implemented in Stage 4
        Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .body(Body::empty())
            .expect("build 501")
    } else {
        // PutObject — implemented in Stage 3
        Response::builder()
            .status(StatusCode::NOT_IMPLEMENTED)
            .body(Body::empty())
            .expect("build 501")
    }
}
