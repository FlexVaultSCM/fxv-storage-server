/// S3-compatible XML types for multipart upload API.
///
/// These structs are serialised/deserialised with `quick-xml` + `serde`.
/// Only the fields needed for our implementation are included.
///
/// S3 XML error response format:
/// <https://docs.aws.amazon.com/AmazonS3/latest/API/ErrorResponses.html>
// == Std
use std::borrow::Cow;

// == External
use axum::{body::Body, http::StatusCode, response::Response};
use serde::{Deserialize, Serialize};

// == S3 error responses

/// Predefined S3 error variants used by the HTTP handlers.
pub enum S3ErrorKind<'a> {
    /// The requested object key does not exist.
    NoSuchKey,
    /// The requested multipart upload does not exist.
    NoSuchUpload,
    /// The request preconditions did not hold.
    PreconditionFailed,
    /// The requested byte range is not satisfiable.
    InvalidRange,
    /// The request arguments were invalid.
    InvalidArgument(&'a str),
    /// One or more referenced multipart parts were invalid.
    InvalidPart,
    /// The supplied XML body was malformed.
    MalformedXml,
    /// An unexpected internal error occurred.
    Internal,
}

impl S3ErrorKind<'_> {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::NoSuchKey | Self::NoSuchUpload => StatusCode::NOT_FOUND,
            Self::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            Self::InvalidRange => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::InvalidArgument(_) | Self::InvalidPart | Self::MalformedXml => StatusCode::BAD_REQUEST,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::NoSuchKey => "NoSuchKey",
            Self::NoSuchUpload => "NoSuchUpload",
            Self::PreconditionFailed => "PreconditionFailed",
            Self::InvalidRange => "InvalidRange",
            Self::InvalidArgument(_) => "InvalidArgument",
            Self::InvalidPart => "InvalidPart",
            Self::MalformedXml => "MalformedXML",
            Self::Internal => "InternalError",
        }
    }

    fn message(&self) -> Cow<'_, str> {
        match self {
            Self::NoSuchKey => Cow::Borrowed("The specified key does not exist."),
            Self::NoSuchUpload => Cow::Borrowed(
                "The specified upload does not exist. The upload ID may be invalid, or the upload may have been aborted or completed.",
            ),
            Self::PreconditionFailed => Cow::Borrowed("At least one of the pre-conditions you specified did not hold."),
            Self::InvalidRange => Cow::Borrowed("The requested range is not satisfiable."),
            Self::InvalidArgument(message) => Cow::Borrowed(message),
            Self::InvalidPart => Cow::Borrowed(
                "One or more of the specified parts could not be found. The part may not have been uploaded, or the specified entity tag may not match the part's entity tag.",
            ),
            Self::MalformedXml => {
                Cow::Borrowed("The XML you provided was not well-formed or did not validate against our schema.")
            }
            Self::Internal => Cow::Borrowed("We encountered an internal error. Please try again."),
        }
    }
}

/// Build an S3-compatible XML error `Response`.
///
/// ```xml
/// <?xml version="1.0" encoding="UTF-8"?>
/// <Error><Code>NoSuchKey</Code><Message>...</Message></Error>
/// ```
pub fn s3_error(kind: S3ErrorKind<'_>) -> Response {
    let code = kind.code();
    let message = kind.message();
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>{}</Code><Message>{}</Message></Error>"#,
        code, message
    );
    Response::builder()
        .status(kind.status_code())
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build error response")
}

// == CreateMultipartUpload response

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

// == CompleteMultipartUpload request

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

// == CompleteMultipartUpload response

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

// == XML serialization helpers

/// Serialise a value to an XML byte string with an `<?xml ...?>` declaration.
pub fn to_xml_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, quick_xml::se::SeError> {
    let mut out = br#"<?xml version="1.0" encoding="UTF-8"?>"#.to_vec();
    let body = quick_xml::se::to_string(value)?;
    out.extend_from_slice(body.as_bytes());
    Ok(out)
}

/// Deserialise a value from an XML byte slice.
pub fn from_xml_bytes<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, quick_xml::DeError> {
    quick_xml::de::from_reader(bytes)
}
