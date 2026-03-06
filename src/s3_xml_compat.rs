/// S3-compatible XML types for multipart upload API.
///
/// These structs are serialised/deserialised with `quick-xml` + `serde`.
/// Only the fields needed for our implementation are included.
///
/// S3 XML error response format:
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/ErrorResponses.html>
use axum::{body::Body, http::StatusCode, response::Response};
use serde::{Deserialize, Serialize};

// ── S3 error responses ───────────────────────────────────────────────────────

/// Build an S3-compatible XML error `Response`.
///
/// ```xml
/// <?xml version="1.0" encoding="UTF-8"?>
/// <Error><Code>NoSuchKey</Code><Message>...</Message></Error>
/// ```
pub fn s3_error(status: StatusCode, code: &str, message: &str) -> Response {
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>{}</Code><Message>{}</Message></Error>"#,
        code, message
    );
    Response::builder()
        .status(status)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build error response")
}

// Pre-defined S3 error constructors matching the standard error codes.
pub fn err_no_such_key() -> Response {
    s3_error(
        StatusCode::NOT_FOUND,
        "NoSuchKey",
        "The specified key does not exist.",
    )
}
pub fn err_no_such_upload() -> Response {
    s3_error(
        StatusCode::NOT_FOUND,
        "NoSuchUpload",
        "The specified upload does not exist. The upload ID may be invalid, or the upload may have been aborted or completed.",
    )
}
pub fn err_precondition_failed() -> Response {
    s3_error(
        StatusCode::PRECONDITION_FAILED,
        "PreconditionFailed",
        "At least one of the pre-conditions you specified did not hold.",
    )
}
pub fn err_invalid_range() -> Response {
    s3_error(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "InvalidRange",
        "The requested range is not satisfiable.",
    )
}
pub fn err_invalid_argument(msg: &str) -> Response {
    s3_error(StatusCode::BAD_REQUEST, "InvalidArgument", msg)
}
pub fn err_invalid_part() -> Response {
    s3_error(
        StatusCode::BAD_REQUEST,
        "InvalidPart",
        "One or more of the specified parts could not be found. The part may not have been uploaded, or the specified entity tag may not match the part's entity tag.",
    )
}
pub fn err_malformed_xml() -> Response {
    s3_error(
        StatusCode::BAD_REQUEST,
        "MalformedXML",
        "The XML you provided was not well-formed or did not validate against our schema.",
    )
}
pub fn err_internal() -> Response {
    s3_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "InternalError",
        "We encountered an internal error. Please try again.",
    )
}

// ── CreateMultipartUpload response ──────────────────────────────────────────

/// Response body for the CreateMultipartUpload operation.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/API_CreateMultipartUpload.html>
#[derive(Debug, Serialize)]
#[serde(rename = "InitiateMultipartUploadResult")]
pub struct InitiateMultipartUploadResult {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "UploadId")]
    pub upload_id: String,
}

// ── CompleteMultipartUpload request ─────────────────────────────────────────

/// Request body for the CompleteMultipartUpload operation.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html>
#[derive(Debug, Deserialize)]
#[serde(rename = "CompleteMultipartUpload")]
pub struct CompleteMultipartUpload {
    #[serde(rename = "Part")]
    pub parts: Vec<CompletePart>,
}

/// A single part entry within a [`CompleteMultipartUpload`] request body.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompletedPart.html>
#[derive(Debug, Deserialize)]
pub struct CompletePart {
    #[serde(rename = "PartNumber")]
    pub part_number: u32,
    #[serde(rename = "ETag")]
    pub etag: String,
}

// ── CompleteMultipartUpload response ────────────────────────────────────────

/// Response body for the CompleteMultipartUpload operation.
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html>
#[derive(Debug, Serialize)]
#[serde(rename = "CompleteMultipartUploadResult")]
pub struct CompleteMultipartUploadResult {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "ETag")]
    pub etag: String,
}

// ── XML serialisation helpers ────────────────────────────────────────────────

/// Serialise a value to an XML byte string with an `<?xml ...?>` declaration.
pub fn to_xml_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, quick_xml::se::SeError> {
    let mut out = br#"<?xml version="1.0" encoding="UTF-8"?>"#.to_vec();
    let body = quick_xml::se::to_string(value)?;
    out.extend_from_slice(body.as_bytes());
    Ok(out)
}

/// Deserialise a value from an XML byte slice.
pub fn from_xml_bytes<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, quick_xml::DeError> {
    quick_xml::de::from_reader(bytes)
}
