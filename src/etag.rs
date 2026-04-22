use crate::errors::*;
use md5::{Digest, Md5};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tracing::{debug, warn};

/// Directory name (relative to serve_dir) used to persist ETag cache files.
pub const ETAG_CACHE_DIR: &str = ".fxv-etag-cache";

pub type Md5DigestBytes = [u8; 16];

/// Compute an MD5 ETag for the given file path by streaming its contents.
/// Returns a hex-encoded string wrapped in double quotes (the ETag wire format).
pub async fn compute_file_etag(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest: Md5DigestBytes = hasher.finalize().into();
    Ok(etag_from_digest_bytes(&digest))
}

/// Render an MD5 digest as a quoted ETag string.
pub fn etag_from_digest_bytes(digest: &Md5DigestBytes) -> String {
    format!("\"{}\"", hex_encode(digest))
}

/// Compute a multipart ETag using the S3 algorithm:
/// `md5(concat(binary_part_md5s)) + "-" + part_count`.
pub fn multipart_etag_from_part_digests(part_digests: &[Md5DigestBytes]) -> String {
    let mut hasher = Md5::new();
    for digest in part_digests {
        hasher.update(digest);
    }
    let digest: Md5DigestBytes = hasher.finalize().into();
    format!("\"{}-{}\"", hex_encode(&digest), part_digests.len())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Returns the cache file path for a given serve-dir-relative key path.
/// Cache files live in `<serve_dir>/.fxv-etag-cache/<key_path>`.
fn cache_path(serve_dir: &Path, rel_path: &Path) -> PathBuf {
    serve_dir.join(ETAG_CACHE_DIR).join(rel_path)
}

/// Remove the cached ETag file for a given serve-dir-relative path.
pub async fn remove_cached_etag(serve_dir: &Path, rel_path: &Path) -> std::io::Result<()> {
    tokio::fs::remove_file(cache_path(serve_dir, rel_path)).await
}

/// Attempt to load a cached ETag for a file.
/// The cache entry is valid only if the cached mtime matches the file's current mtime.
/// Returns `Ok(Some(etag))` on a valid cache hit, `Ok(None)` on miss/stale.
pub async fn load_cached_etag(
    serve_dir: &Path,
    rel_path: &Path,
    file_mtime_secs: i64,
) -> Option<String> {
    let cache_file = cache_path(serve_dir, rel_path);
    let contents = tokio::fs::read_to_string(&cache_file).await.ok()?;
    // Format: "<mtime_secs> <etag>"
    let (mtime_str, etag) = contents.split_once(' ')?;
    let cached_mtime: i64 = mtime_str.parse().ok()?;
    if cached_mtime == file_mtime_secs {
        Some(etag.trim().to_owned())
    } else {
        None
    }
}

/// Persist an ETag to the cache for later reuse.
///
/// # Concurrency invariant
///
/// This function uses `tokio::fs::write` which is not atomic by itself.  It is
/// safe only when the caller holds the `FileStore` write lock for the entire
/// call, which serialises all runtime writers.  Both `put_object` and
/// `complete_multipart_upload` uphold this invariant.  The only other call-site
/// is `get_or_compute_etag`, which is used exclusively during startup
/// (`FileStore::build`) before any request is served.
pub async fn save_cached_etag(serve_dir: &Path, rel_path: &Path, file_mtime_secs: i64, etag: &str) {
    let cache_file = cache_path(serve_dir, rel_path);
    if let Some(parent) = cache_file.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        warn!("Failed to create ETag cache dir {:?}: {}", parent, e);
        return;
    }
    let contents = format!("{} {}", file_mtime_secs, etag);
    if let Err(e) = tokio::fs::write(&cache_file, &contents).await {
        warn!("Failed to write ETag cache {:?}: {}", cache_file, e);
    }
}

/// Compute (or load from cache) the MD5 ETag for a file.
/// `rel_path` is the path relative to `serve_dir`.
pub async fn get_or_compute_etag(serve_dir: &Path, rel_path: &Path) -> Result<String> {
    let abs_path = serve_dir.join(rel_path);
    let meta = tokio::fs::metadata(&abs_path).await?;
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if let Some(cached) = load_cached_etag(serve_dir, rel_path, mtime_secs).await {
        debug!("ETag cache hit for {:?}", rel_path);
        return Ok(cached);
    }

    debug!("Computing ETag for {:?}", rel_path);
    let etag = compute_file_etag(&abs_path).await?;
    save_cached_etag(serve_dir, rel_path, mtime_secs, &etag).await;
    Ok(etag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_compute_etag_deterministic() {
        let dir = TempDir::new().expect("tempdir");
        let file = dir.path().join("test.bin");
        std::fs::write(&file, b"hello world").expect("write");

        let etag1 = compute_file_etag(&file).await.expect("etag1");
        let etag2 = compute_file_etag(&file).await.expect("etag2");
        assert_eq!(etag1, etag2);
        assert_eq!(etag1, "\"5eb63bbbe01eeed093cb22bb8f5acdc3\"");
    }

    #[tokio::test]
    async fn test_compute_etag_differs_for_different_content() {
        let dir = TempDir::new().expect("tempdir");
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        std::fs::write(&a, b"content A").expect("write a");
        std::fs::write(&b, b"content B").expect("write b");
        let etag_a = compute_file_etag(&a).await.expect("etag_a");
        let etag_b = compute_file_etag(&b).await.expect("etag_b");
        assert_ne!(etag_a, etag_b);
    }

    #[tokio::test]
    async fn test_etag_cache_round_trip() {
        let dir = TempDir::new().expect("tempdir");
        let rel = std::path::Path::new("sub/file.txt");
        let abs = dir.path().join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
        std::fs::write(&abs, b"cached content").expect("write");

        let etag = get_or_compute_etag(dir.path(), rel).await.expect("first");
        // Second call should hit cache
        let etag2 = get_or_compute_etag(dir.path(), rel).await.expect("second");
        assert_eq!(etag, etag2);
    }

    #[tokio::test]
    async fn test_etag_cache_invalidates_on_mtime_change() {
        let dir = TempDir::new().expect("tempdir");
        let rel = std::path::Path::new("file.txt");
        let abs = dir.path().join(rel);
        std::fs::write(&abs, b"version 1").expect("write");

        let etag1 = get_or_compute_etag(dir.path(), rel).await.expect("v1");

        // Overwrite with different content and update mtime manually
        std::fs::write(&abs, b"version 2").expect("overwrite");
        // Force a different mtime by bumping it
        let new_mtime = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let ft = filetime::FileTime::from_system_time(new_mtime);
        filetime::set_file_mtime(&abs, ft).expect("set mtime");

        let etag2 = get_or_compute_etag(dir.path(), rel).await.expect("v2");
        assert_ne!(etag1, etag2);
    }

    #[test]
    fn test_multipart_etag_from_part_digests_matches_s3_shape() {
        let part1 = [0u8; 16];
        let part2 = [1u8; 16];
        let etag = multipart_etag_from_part_digests(&[part1, part2]);
        assert!(etag.starts_with('"'));
        assert!(etag.ends_with('"'));
        assert!(etag.contains("-2"));
    }
}
