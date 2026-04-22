/// In-memory state for multipart uploads.
///
/// Lives only for the duration of the server process; lost on restart.
use crate::etag::Md5DigestBytes;
use crate::metadata_cache::HeaderEntry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Directory name (relative to serve_dir) used for in-progress multipart part files.
pub const MULTIPART_UPLOAD_DIR: &str = ".fxv-multipart-uploads";

/// Metadata for a single uploaded part.
#[derive(Debug, Clone)]
pub struct PartEntry {
    /// Path to the temp file holding this part's data.
    pub abs_path: PathBuf,
    /// Number of bytes in this part.
    pub size: u64,
    /// MD5 ETag of this part's data (double-quoted hex string).
    pub etag: String,
    /// Raw MD5 digest bytes used to build S3 multipart ETags.
    pub md5_bytes: Md5DigestBytes,
}

/// State for one in-progress multipart upload.
#[derive(Debug, Clone)]
pub struct UploadEntry {
    /// Object key this upload targets.
    pub key: String,
    /// Custom headers to apply to the completed object.
    pub custom_headers: Vec<HeaderEntry>,
    /// Parts uploaded so far, keyed by 1-based part number.
    pub parts: HashMap<u32, PartEntry>,
}

/// Shared, async-safe multipart upload state.
pub type SharedUploadState = Arc<RwLock<HashMap<String, UploadEntry>>>;

/// Construct an empty shared upload state.
pub fn new_shared_upload_state() -> SharedUploadState {
    Arc::new(RwLock::new(HashMap::new()))
}
