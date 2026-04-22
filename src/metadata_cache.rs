use crate::{errors::*, etag};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, warn};

pub const METADATA_CACHE_DIR: &str = ".fxv-metadata-cache";
const METADATA_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderEntry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksumSet {
    pub md5: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crc32: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crc64nvme: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectMetadataCache {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    pub last_modified_unix_secs: i64,
    pub etag: String,
    pub checksums: ChecksumSet,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_headers: Vec<HeaderEntry>,
}

impl ObjectMetadataCache {
    pub fn from_current_state(
        modified: SystemTime,
        etag: String,
        checksums: ChecksumSet,
        custom_headers: Vec<HeaderEntry>,
    ) -> Self {
        Self {
            schema_version: METADATA_SCHEMA_VERSION,
            last_modified_unix_secs: system_time_to_secs(modified),
            etag,
            checksums,
            custom_headers,
        }
    }
}

fn schema_version() -> u32 {
    METADATA_SCHEMA_VERSION
}

fn cache_path(serve_dir: &Path, rel_path: &Path) -> PathBuf {
    let mut cache_file = serve_dir.join(METADATA_CACHE_DIR).join(rel_path);
    let file_name = cache_file.file_name().expect("metadata cache file name").to_os_string();
    let mut json_name = file_name;
    json_name.push(".json");
    cache_file.set_file_name(json_name);
    cache_file
}

pub fn system_time_to_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub async fn remove_metadata_cache(serve_dir: &Path, rel_path: &Path) -> std::io::Result<()> {
    tokio::fs::remove_file(cache_path(serve_dir, rel_path)).await
}

pub async fn save_metadata_cache(serve_dir: &Path, rel_path: &Path, metadata: &ObjectMetadataCache) {
    let cache_file = cache_path(serve_dir, rel_path);
    if let Some(parent) = cache_file.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        warn!("Failed to create metadata cache dir {:?}: {}", parent, e);
        return;
    }

    let contents = match serde_json::to_vec_pretty(metadata) {
        Ok(contents) => contents,
        Err(e) => {
            warn!("Failed to serialize metadata cache {:?}: {}", cache_file, e);
            return;
        }
    };

    if let Err(e) = tokio::fs::write(&cache_file, &contents).await {
        warn!("Failed to write metadata cache {:?}: {}", cache_file, e);
    }
}

async fn load_metadata_cache(serve_dir: &Path, rel_path: &Path, file_mtime_secs: i64) -> Option<ObjectMetadataCache> {
    let cache_file = cache_path(serve_dir, rel_path);
    let contents = tokio::fs::read(&cache_file).await.ok()?;
    let metadata: ObjectMetadataCache = serde_json::from_slice(&contents).ok()?;

    if metadata.last_modified_unix_secs == file_mtime_secs {
        Some(metadata)
    } else {
        None
    }
}

pub async fn get_or_compute_metadata(serve_dir: &Path, rel_path: &Path) -> Result<ObjectMetadataCache> {
    let abs_path = serve_dir.join(rel_path);
    let meta = tokio::fs::metadata(&abs_path).await?;
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let mtime_secs = system_time_to_secs(modified);

    if let Some(metadata) = load_metadata_cache(serve_dir, rel_path, mtime_secs).await {
        debug!("Metadata cache hit for {:?}", rel_path);
        return Ok(metadata);
    }

    let md5_hex = etag::compute_file_md5_hex(&abs_path).await?;
    let etag = etag::etag_from_md5_hex(&md5_hex);

    let metadata = ObjectMetadataCache::from_current_state(
        modified,
        etag,
        ChecksumSet {
            md5: md5_hex,
            crc32: None,
            crc64nvme: None,
        },
        Vec::new(),
    );
    save_metadata_cache(serve_dir, rel_path, &metadata).await;
    Ok(metadata)
}

pub fn extract_user_metadata_headers(headers: &HeaderMap) -> Vec<HeaderEntry> {
    headers
        .iter()
        .filter(|(key, _)| key.as_str().starts_with("x-amz-meta-"))
        .filter_map(|(key, value)| {
            value.to_str().ok().map(|value| HeaderEntry {
                key: key.as_str().to_owned(),
                value: value.to_owned(),
            })
        })
        .collect()
}

pub fn validate_header_entry(key: &str, value: &str) -> Result<HeaderEntry> {
    HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid header name '{}': {}", key, e),
        )
    })?;
    HeaderValue::from_str(value).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid header value for '{}': {}", key, e),
        )
    })?;

    Ok(HeaderEntry {
        key: key.to_owned(),
        value: value.to_owned(),
    })
}

pub fn upsert_custom_header(custom_headers: &mut Vec<HeaderEntry>, key: &str, value: &str) -> Result<()> {
    let entry = validate_header_entry(key, value)?;
    if let Some(existing) = custom_headers
        .iter_mut()
        .find(|header| header.key.eq_ignore_ascii_case(key))
    {
        *existing = entry;
    } else {
        custom_headers.push(entry);
    }
    Ok(())
}

pub fn apply_custom_headers(headers: &mut HeaderMap, custom_headers: &[HeaderEntry]) {
    for header in custom_headers {
        let key = match HeaderName::from_bytes(header.key.as_bytes()) {
            Ok(key) => key,
            Err(e) => {
                warn!("Skipping invalid cached header name '{}': {}", header.key, e);
                continue;
            }
        };
        let value = match HeaderValue::from_str(&header.value) {
            Ok(value) => value,
            Err(e) => {
                warn!("Skipping invalid cached header value for '{}': {}", header.key, e);
                continue;
            }
        };
        headers.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_metadata_cache_round_trip() {
        let dir = TempDir::new().expect("tempdir");
        let rel = Path::new("sub/file.txt");
        let metadata = ObjectMetadataCache::from_current_state(
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(123),
            "\"abc\"".to_owned(),
            ChecksumSet {
                md5: "deadbeef".to_owned(),
                crc32: Some("c0ffee".to_owned()),
                crc64nvme: None,
            },
            vec![HeaderEntry {
                key: "x-amz-meta-owner".to_owned(),
                value: "alice".to_owned(),
            }],
        );

        save_metadata_cache(dir.path(), rel, &metadata).await;
        let loaded = load_metadata_cache(dir.path(), rel, 123).await.expect("load");
        assert_eq!(loaded, metadata);
    }

    #[test]
    fn test_upsert_custom_header_replaces_case_insensitively() {
        let mut headers = vec![HeaderEntry {
            key: "Content-Type".to_owned(),
            value: "text/plain".to_owned(),
        }];

        upsert_custom_header(&mut headers, "content-type", "application/json").expect("upsert");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].value, "application/json");
    }
}
