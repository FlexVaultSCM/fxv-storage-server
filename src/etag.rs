use crate::errors::*;
use md5::{Digest, Md5};
use std::path::Path;
use tokio::io::AsyncReadExt;

pub type Md5DigestBytes = [u8; 16];

/// Compute an MD5 ETag for the given file path by streaming its contents.
/// Returns a hex-encoded string wrapped in double quotes (the ETag wire format).
pub async fn compute_file_etag(path: &Path) -> Result<String> {
    Ok(etag_from_md5_hex(&compute_file_md5_hex(path).await?))
}

/// Compute an MD5 checksum for the given file path by streaming its contents.
/// Returns the hex string without ETag quoting.
pub async fn compute_file_md5_hex(path: &Path) -> Result<String> {
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
    Ok(md5_hex_from_digest_bytes(&digest))
}

/// Render an MD5 digest as a quoted ETag string.
pub fn etag_from_digest_bytes(digest: &Md5DigestBytes) -> String {
    etag_from_md5_hex(&md5_hex_from_digest_bytes(digest))
}

/// Render a raw MD5 hex string as a quoted ETag string.
pub fn etag_from_md5_hex(md5_hex: &str) -> String {
    format!("\"{}\"", md5_hex)
}

/// Render MD5 digest bytes as lowercase hex without ETag quoting.
pub fn md5_hex_from_digest_bytes(digest: &Md5DigestBytes) -> String {
    hex_encode(digest)
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

    #[test]
    fn test_multipart_etag_from_part_digests_matches_s3_shape() {
        let part1 = [0u8; 16];
        let part2 = [1u8; 16];
        let etag = multipart_etag_from_part_digests(&[part1, part2]);
        assert!(etag.starts_with('"'));
        assert!(etag.ends_with('"'));
        assert!(etag.contains("-2"));
    }

    #[tokio::test]
    async fn test_compute_file_md5_hex() {
        let dir = TempDir::new().expect("tempdir");
        let file = dir.path().join("test.bin");
        std::fs::write(&file, b"hello world").expect("write");

        let md5_hex = compute_file_md5_hex(&file).await.expect("md5");
        assert_eq!(md5_hex, "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }
}
