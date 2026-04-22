use crate::{
    errors::*,
    metadata_cache::{self, ChecksumSet, HeaderEntry},
    multipart_state,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Metadata about a file tracked by the store.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Absolute path on disk.
    pub abs_path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// Last-modified time.
    pub modified: SystemTime,
    /// Pre-computed ETag (double-quoted string).
    pub etag: String,
    /// Stored checksums for the object.
    pub checksums: ChecksumSet,
    /// Stored custom headers for the object.
    pub custom_headers: Vec<HeaderEntry>,
}

/// In-memory index of files in the serve directory.
/// Keys are slash-separated paths relative to the serve root (e.g. `"a/b/c.txt"`).
#[derive(Debug)]
pub struct FileStore {
    /// Canonical path of the serve directory.
    serve_dir: PathBuf,
    entries: HashMap<String, FileEntry>,
}

impl FileStore {
    /// Build a `FileStore` by walking `serve_dir` and computing ETags for all files.
    pub async fn build(serve_dir: &Path) -> Result<Self> {
        let mut entries = HashMap::new();
        walk_dir(serve_dir, serve_dir, &mut entries).await?;
        info!("FileStore built with {} entries", entries.len());
        Ok(FileStore {
            serve_dir: serve_dir.to_owned(),
            entries,
        })
    }

    /// The serve directory this store is rooted at.
    pub fn serve_dir(&self) -> &Path {
        &self.serve_dir
    }

    /// Look up an entry by its slash-separated key (e.g. `"a/b/c.txt"`).
    pub fn get(&self, key: &str) -> Option<&FileEntry> {
        self.entries.get(key)
    }

    /// Insert or replace an entry (used after PutObject / CompleteMultipartUpload).
    pub fn upsert(&mut self, key: String, entry: FileEntry) {
        self.entries.insert(key, entry);
    }

    /// Remove an entry.
    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
}

/// Recursively walk `dir`, compute ETags, and collect entries into `map`.
/// `rel_base` is the serve root; used to produce relative keys.
async fn walk_dir(rel_base: &Path, dir: &Path, map: &mut HashMap<String, FileEntry>) -> Result<()> {
    let mut read_dir = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;

        if file_type.is_dir() {
            // Skip metadata and multipart internal directories
            if path
                .file_name()
                .is_some_and(|n| n == metadata_cache::METADATA_CACHE_DIR || n == multipart_state::MULTIPART_UPLOAD_DIR)
            {
                continue;
            }
            // Recurse - Box the future to avoid infinite-size type
            walk_dir_boxed(rel_base, &path, map).await?;
        } else if file_type.is_file() {
            let rel_path = path.strip_prefix(rel_base)?;
            let rel_key = rel_path
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");

            match compute_entry(rel_base, rel_path, &path).await {
                Ok(entry) => {
                    map.insert(rel_key, entry);
                }
                Err(e) => {
                    warn!("Skipping {:?}: {}", path, e);
                }
            }
        }
    }
    Ok(())
}

fn walk_dir_boxed<'a>(
    rel_base: &'a Path,
    dir: &'a Path,
    map: &'a mut HashMap<String, FileEntry>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(walk_dir(rel_base, dir, map))
}

async fn compute_entry(serve_dir: &Path, rel_path: &Path, abs_path: &Path) -> Result<FileEntry> {
    let meta = tokio::fs::metadata(abs_path).await?;
    let metadata = metadata_cache::get_or_compute_metadata(serve_dir, rel_path).await?;
    Ok(FileEntry {
        abs_path: abs_path.to_owned(),
        size: meta.len(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        etag: metadata.etag,
        checksums: metadata.checksums,
        custom_headers: metadata.custom_headers,
    })
}

/// Thread-safe shared handle to the `FileStore`.
pub type SharedStore = Arc<RwLock<FileStore>>;

/// Construct a `SharedStore` from the given serve directory.
pub async fn build_shared_store(serve_dir: &Path) -> Result<SharedStore> {
    let store = FileStore::build(serve_dir).await?;
    Ok(Arc::new(RwLock::new(store)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_store_builds_and_indexes_files() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("root.txt"), b"root file").expect("write root");
        std::fs::write(dir.path().join("sub/nested.txt"), b"nested file").expect("write nested");

        let store = FileStore::build(dir.path()).await.expect("build");
        assert!(store.get("root.txt").is_some());
        assert!(store.get("sub/nested.txt").is_some());
        assert!(store.get("nonexistent.txt").is_none());
    }

    #[tokio::test]
    async fn test_store_skips_metadata_cache_dir() {
        let dir = TempDir::new().expect("tempdir");
        let cache_dir = dir.path().join(metadata_cache::METADATA_CACHE_DIR);
        std::fs::create_dir_all(&cache_dir).expect("mkdir cache");
        std::fs::write(cache_dir.join("some_cache_file"), b"internal").expect("write cache");
        std::fs::write(dir.path().join("real.txt"), b"real content").expect("write real");

        let store = FileStore::build(dir.path()).await.expect("build");
        assert!(store.get("real.txt").is_some());
        // Cache dir contents must not appear
        assert!(
            store
                .get(&format!("{}/some_cache_file", metadata_cache::METADATA_CACHE_DIR))
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_store_skips_multipart_upload_dir() {
        let dir = TempDir::new().expect("tempdir");
        let multipart_dir = dir.path().join(multipart_state::MULTIPART_UPLOAD_DIR);
        std::fs::create_dir_all(&multipart_dir).expect("mkdir multipart");
        std::fs::write(multipart_dir.join("part-abc-1.fxv_tmp"), b"internal").expect("write part");
        std::fs::write(dir.path().join("real.txt"), b"real content").expect("write real");

        let store = FileStore::build(dir.path()).await.expect("build");
        assert!(store.get("real.txt").is_some());
        assert!(
            store
                .get(&format!("{}/part-abc-1.fxv_tmp", multipart_state::MULTIPART_UPLOAD_DIR))
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_store_upsert_and_remove() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), b"a").expect("write");
        let mut store = FileStore::build(dir.path()).await.expect("build");

        assert!(store.get("a.txt").is_some());
        store.remove("a.txt");
        assert!(store.get("a.txt").is_none());
    }
}
