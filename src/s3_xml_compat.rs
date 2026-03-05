/// S3-compatible XML types for multipart upload API.
///
/// These structs are serialised/deserialised with `quick-xml` + `serde`.
/// Only the fields needed for our implementation are included.
use serde::{Deserialize, Serialize};

// ── CreateMultipartUpload response ──────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename = "InitiateMultipartUploadResult")]
pub struct InitiateMultipartUploadResult {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "UploadId")]
    pub upload_id: String,
}

// ── CompleteMultipartUpload request ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename = "CompleteMultipartUpload")]
pub struct CompleteMultipartUpload {
    #[serde(rename = "Part")]
    pub parts: Vec<CompletePart>,
}

#[derive(Debug, Deserialize)]
pub struct CompletePart {
    #[serde(rename = "PartNumber")]
    pub part_number: u32,
    #[serde(rename = "ETag")]
    pub etag: String,
}

// ── CompleteMultipartUpload response ────────────────────────────────────────

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
pub fn from_xml_bytes<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, quick_xml::DeError> {
    quick_xml::de::from_reader(bytes)
}
