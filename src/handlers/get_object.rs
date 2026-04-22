// == Std
use std::{io::SeekFrom, time::SystemTime};

// == Internal
use crate::{
    AppState,
    conditional::{self, ConditionalResult},
    metadata_cache, range,
    s3_xml_compat::{S3ErrorKind, s3_error},
};

// == External
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use tokio::io::AsyncSeekExt;
use tracing::debug;

/// GET /{*key} - S3 GetObject
pub async fn get_object(State(state): State<AppState>, Path(key): Path<String>, headers: HeaderMap) -> Response {
    let store = state.store.read().await;
    let entry = match store.get(&key) {
        Some(e) => e.clone(),
        None => return s3_error(S3ErrorKind::NoSuchKey),
    };

    // Parse conditional headers
    let if_match = headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let if_modified_since = headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_http_date);
    let if_unmodified_since = headers
        .get(header::IF_UNMODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_http_date);
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Evaluate conditionals
    let cond = conditional::evaluate(
        &entry.etag,
        entry.modified,
        if_match.as_deref(),
        if_none_match.as_deref(),
        if_modified_since,
        if_unmodified_since,
    );

    match cond {
        ConditionalResult::NotModified => {
            return with_custom_headers(
                Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, &entry.etag)
                    .header(header::LAST_MODIFIED, fmt_http_date(entry.modified))
                    .body(Body::empty())
                    .expect("build 304"),
                &entry,
            );
        }
        ConditionalResult::PreconditionFailed => return s3_error(S3ErrorKind::PreconditionFailed),
        ConditionalResult::Proceed => {}
    }

    // Parse range header
    let byte_range = match range::parse_range(range_header.as_deref(), entry.size) {
        Ok(r) => r,
        Err(_) => {
            // 416 Range Not Satisfiable - include Content-Range: bytes */size per RFC 7233
            let mut resp = s3_error(S3ErrorKind::InvalidRange);
            resp.headers_mut().insert(
                header::CONTENT_RANGE,
                format!("bytes */{}", entry.size).parse().expect("content-range"),
            );
            return resp;
        }
    };

    // Open the file
    let mut file = match tokio::fs::File::open(&entry.abs_path).await {
        Ok(f) => f,
        Err(_) => return s3_error(S3ErrorKind::Internal),
    };

    let last_modified_str = fmt_http_date(entry.modified);

    match byte_range {
        None => {
            // Full response
            debug!("GET {} -> 200 ({} bytes)", key, entry.size);
            let stream = tokio_util::io::ReaderStream::new(file);
            with_custom_headers(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, entry.size.to_string())
                    .header(header::ETAG, &entry.etag)
                    .header(header::LAST_MODIFIED, last_modified_str)
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(Body::from_stream(stream))
                    .expect("build 200"),
                &entry,
            )
        }
        Some(range) => {
            // Partial response
            debug!(
                "GET {} -> 206 (bytes {}-{}/{})",
                key, range.start, range.end, entry.size
            );
            if file.seek(SeekFrom::Start(range.start)).await.is_err() {
                return s3_error(S3ErrorKind::Internal);
            }
            let limited = tokio::io::AsyncReadExt::take(file, range.len());
            let stream = tokio_util::io::ReaderStream::new(limited);
            with_custom_headers(
                Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, range.len().to_string())
                    .header(header::CONTENT_RANGE, range.content_range_header(entry.size))
                    .header(header::ETAG, &entry.etag)
                    .header(header::LAST_MODIFIED, last_modified_str)
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(Body::from_stream(stream))
                    .expect("build 206"),
                &entry,
            )
        }
    }
}

fn with_custom_headers(mut response: Response, entry: &crate::store::FileEntry) -> Response {
    metadata_cache::apply_custom_headers(response.headers_mut(), &entry.custom_headers);
    response
}

/// Parse an HTTP-date string (RFC 7231 / RFC 1123) into a `SystemTime`.
fn parse_http_date(s: &str) -> Option<SystemTime> {
    httpdate::parse_http_date(s).ok()
}

/// Format a `SystemTime` as an HTTP-date string.
pub fn fmt_http_date(t: SystemTime) -> String {
    httpdate::fmt_http_date(t)
}
