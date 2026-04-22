//! HTTP Range header parsing and byte-range response helpers.
//!
//! Only single-range requests are supported (S3 does not support multi-range GET).

use crate::errors::*;

/// A parsed, validated byte range within a file of known size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64, // inclusive
}

impl ByteRange {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    pub fn is_empty(&self) -> bool {
        false // ByteRange always has at least 1 byte (enforced by parse_range)
    }

    /// Format the `Content-Range` header value, e.g. `bytes 0-499/1234`.
    pub fn content_range_header(&self, total_size: u64) -> String {
        format!("bytes {}-{}/{}", self.start, self.end, total_size)
    }
}

/// Parse a `Range: bytes=<start>-<end>` header value for a resource of `file_size` bytes.
///
/// Returns:
/// - `Ok(Some(range))` for a valid, satisfiable range.
/// - `Ok(None)` if the header is absent.
/// - `Err(ErrorKind::InvalidRange)` if the header is present but malformed or unsatisfiable.
pub fn parse_range(header_value: Option<&str>, file_size: u64) -> Result<Option<ByteRange>> {
    let value = match header_value {
        None => return Ok(None),
        Some(v) => v.trim(),
    };

    let bytes_part = value
        .strip_prefix("bytes=")
        .ok_or_else(|| ErrorKind::InvalidRange(format!("must start with 'bytes=': {}", value)))?;

    // Only support a single range (S3 constraint)
    if bytes_part.contains(',') {
        return Err(ErrorKind::InvalidRange("multi-range requests are not supported".to_owned()).into());
    }

    let (start_str, end_str) = bytes_part
        .split_once('-')
        .ok_or_else(|| ErrorKind::InvalidRange(format!("missing '-': {}", bytes_part)))?;

    let (start, end) = match (start_str.trim(), end_str.trim()) {
        // bytes=500-999
        (s, e) if !s.is_empty() && !e.is_empty() => {
            let start: u64 = s
                .parse()
                .map_err(|_| ErrorKind::InvalidRange(format!("bad start: {}", s)))?;
            let end: u64 = e
                .parse()
                .map_err(|_| ErrorKind::InvalidRange(format!("bad end: {}", e)))?;
            if start > end {
                return Err(ErrorKind::InvalidRange(format!("start {} > end {}", start, end)).into());
            }
            (start, end.min(file_size.saturating_sub(1)))
        }
        // bytes=500-   (from byte 500 to end)
        (s, "") => {
            let start: u64 = s
                .parse()
                .map_err(|_| ErrorKind::InvalidRange(format!("bad start: {}", s)))?;
            (start, file_size.saturating_sub(1))
        }
        // bytes=-500   (last 500 bytes)
        ("", e) => {
            let suffix: u64 = e
                .parse()
                .map_err(|_| ErrorKind::InvalidRange(format!("bad suffix: {}", e)))?;
            let start = file_size.saturating_sub(suffix);
            (start, file_size.saturating_sub(1))
        }
        _ => {
            return Err(ErrorKind::InvalidRange(format!("empty range spec: {}", bytes_part)).into());
        }
    };

    if file_size == 0 || start >= file_size {
        return Err(
            ErrorKind::InvalidRange(format!("range not satisfiable: start={} size={}", start, file_size)).into(),
        );
    }

    Ok(Some(ByteRange { start, end }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_range_header() {
        assert_eq!(parse_range(None, 1000).unwrap(), None);
    }

    #[test]
    fn test_full_explicit_range() {
        let r = parse_range(Some("bytes=0-499"), 1000).unwrap().unwrap();
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 499);
        assert_eq!(r.len(), 500);
    }

    #[test]
    fn test_range_clamps_to_file_size() {
        let r = parse_range(Some("bytes=0-9999"), 100).unwrap().unwrap();
        assert_eq!(r.end, 99);
    }

    #[test]
    fn test_open_ended_range() {
        let r = parse_range(Some("bytes=500-"), 1000).unwrap().unwrap();
        assert_eq!(r.start, 500);
        assert_eq!(r.end, 999);
    }

    #[test]
    fn test_suffix_range() {
        let r = parse_range(Some("bytes=-200"), 1000).unwrap().unwrap();
        assert_eq!(r.start, 800);
        assert_eq!(r.end, 999);
    }

    #[test]
    fn test_suffix_range_larger_than_file() {
        let r = parse_range(Some("bytes=-9999"), 100).unwrap().unwrap();
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 99);
    }

    #[test]
    fn test_invalid_multi_range() {
        assert!(parse_range(Some("bytes=0-100,200-300"), 1000).is_err());
    }

    #[test]
    fn test_invalid_start_gt_end() {
        assert!(parse_range(Some("bytes=500-100"), 1000).is_err());
    }

    #[test]
    fn test_range_not_satisfiable_start_ge_size() {
        assert!(parse_range(Some("bytes=1000-1099"), 1000).is_err());
    }

    #[test]
    fn test_content_range_header() {
        let r = ByteRange { start: 0, end: 499 };
        assert_eq!(r.content_range_header(1000), "bytes 0-499/1000");
    }
}
