//! RFC 7232 conditional request evaluation for GET/HEAD.
//!
//! Returns the appropriate HTTP status code if a conditional check fails
//! (304 Not Modified, 412 Precondition Failed), or `None` if the request
//! should proceed normally.
//!
//! S3 evaluation order per RFC 7232 s.6:
//! 1. If-Match               -> 412 on mismatch
//! 2. If-Unmodified-Since    -> 412 on modified (only if If-Match absent)
//! 3. If-None-Match          -> 304 on match
//! 4. If-Modified-Since      -> 304 on unmodified (only if If-None-Match absent)

// == Std
use std::time::SystemTime;

// == External
use http::StatusCode;

/// Result of evaluating conditional headers.
#[derive(Debug, PartialEq, Eq)]
pub enum ConditionalResult {
    /// Proceed with the normal response.
    Proceed,
    /// Return 304 Not Modified (no body).
    NotModified,
    /// Return 412 Precondition Failed.
    PreconditionFailed,
}

impl ConditionalResult {
    #[allow(dead_code)]
    /// Return the HTTP status associated with this conditional result, if any.
    pub fn status_code(&self) -> Option<StatusCode> {
        match self {
            ConditionalResult::Proceed => None,
            ConditionalResult::NotModified => Some(StatusCode::NOT_MODIFIED),
            ConditionalResult::PreconditionFailed => Some(StatusCode::PRECONDITION_FAILED),
        }
    }
}

/// Evaluate all four conditional headers for a GET/HEAD request.
///
/// - `etag`:     the current ETag of the resource (double-quoted, e.g. `"\"abc123\""`).
/// - `modified`: the last-modified time of the resource.
/// - `if_match`: value of the `If-Match` header, if present.
/// - `if_none_match`: value of the `If-None-Match` header, if present.
/// - `if_modified_since`: value of the `If-Modified-Since` header parsed as `SystemTime`, if present.
/// - `if_unmodified_since`: value of the `If-Unmodified-Since` header parsed as `SystemTime`, if present.
pub fn evaluate(
    etag: &str,
    modified: SystemTime,
    if_match: Option<&str>,
    if_none_match: Option<&str>,
    if_modified_since: Option<SystemTime>,
    if_unmodified_since: Option<SystemTime>,
) -> ConditionalResult {
    // Step 1: If-Match
    if let Some(im) = if_match {
        if !etag_matches(etag, im) {
            return ConditionalResult::PreconditionFailed;
        }
        // If-Match matched -> If-Unmodified-Since is ignored (S3 / RFC 7232 s.6.2)
        // proceed to step 3
    } else {
        // Step 2: If-Unmodified-Since (only when If-Match is absent)
        if let Some(ius) = if_unmodified_since {
            // Passes when the file has NOT been modified since `ius`
            // i.e. modified <= ius
            if modified > ius {
                return ConditionalResult::PreconditionFailed;
            }
        }
    }

    // Step 3: If-None-Match
    if let Some(inm) = if_none_match {
        if etag_matches(etag, inm) {
            return ConditionalResult::NotModified;
        }
        // If-None-Match did not match -> If-Modified-Since is ignored (RFC 7232 s.6.3)
    } else {
        // Step 4: If-Modified-Since (only when If-None-Match is absent)
        if let Some(ims) = if_modified_since {
            // Not modified when modified <= ims
            if !is_modified_since(modified, ims) {
                return ConditionalResult::NotModified;
            }
        }
    }

    ConditionalResult::Proceed
}

/// Returns `true` if `etag` satisfies the `If-Match` / `If-None-Match` value.
/// Supports the wildcard `*` and a single quoted ETag value.
/// Does NOT support the comma-separated list form (rare in practice, not needed for S3 subset).
fn etag_matches(etag: &str, header_value: &str) -> bool {
    let v = header_value.trim();
    if v == "*" {
        return true;
    }
    // Strong comparison: exact match of the double-quoted ETag string
    etag == v
}

/// Returns `true` if `modified` is strictly after `since` (i.e. the resource was modified).
/// Uses second-level granularity (HTTP date headers have 1-second resolution).
fn is_modified_since(modified: SystemTime, since: SystemTime) -> bool {
    // Truncate to whole seconds for comparison
    let modified_secs = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let since_secs = since
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    modified_secs > since_secs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ts(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    const ETAG: &str = "\"abc123\"";
    const OTHER_ETAG: &str = "\"xyz789\"";

    #[test]
    fn test_no_conditionals_proceeds() {
        // Verify an unconditional request proceeds normally.
        assert_eq!(
            evaluate(ETAG, ts(1000), None, None, None, None),
            ConditionalResult::Proceed
        );
    }

    #[test]
    fn test_if_match_match_proceeds() {
        // Verify a matching If-Match header allows the request.
        assert_eq!(
            evaluate(ETAG, ts(1000), Some(ETAG), None, None, None),
            ConditionalResult::Proceed
        );
    }

    #[test]
    fn test_if_match_mismatch_precondition_failed() {
        // Verify a mismatched If-Match header rejects the request.
        assert_eq!(
            evaluate(ETAG, ts(1000), Some(OTHER_ETAG), None, None, None),
            ConditionalResult::PreconditionFailed
        );
    }

    #[test]
    fn test_if_match_wildcard_proceeds() {
        // Verify the wildcard If-Match form accepts an existing object.
        assert_eq!(
            evaluate(ETAG, ts(1000), Some("*"), None, None, None),
            ConditionalResult::Proceed
        );
    }

    #[test]
    fn test_if_none_match_match_not_modified() {
        // Verify a matching If-None-Match header produces 304 semantics.
        assert_eq!(
            evaluate(ETAG, ts(1000), None, Some(ETAG), None, None),
            ConditionalResult::NotModified
        );
    }

    #[test]
    fn test_if_none_match_mismatch_proceeds() {
        // Verify a non-matching If-None-Match header still allows the request.
        assert_eq!(
            evaluate(ETAG, ts(1000), None, Some(OTHER_ETAG), None, None),
            ConditionalResult::Proceed
        );
    }

    #[test]
    fn test_if_none_match_wildcard_not_modified() {
        // Verify wildcard If-None-Match blocks access to an existing object.
        assert_eq!(
            evaluate(ETAG, ts(1000), None, Some("*"), None, None),
            ConditionalResult::NotModified
        );
    }

    #[test]
    fn test_if_modified_since_not_modified() {
        // Verify an unchanged timestamp yields NotModified.
        assert_eq!(
            evaluate(ETAG, ts(1000), None, None, Some(ts(1000)), None),
            ConditionalResult::NotModified
        );
    }

    #[test]
    fn test_if_modified_since_modified() {
        // Verify a newer object still proceeds past If-Modified-Since.
        assert_eq!(
            evaluate(ETAG, ts(2000), None, None, Some(ts(1000)), None),
            ConditionalResult::Proceed
        );
    }

    #[test]
    fn test_if_unmodified_since_passes() {
        // Verify If-Unmodified-Since passes when the object is old enough.
        assert_eq!(
            evaluate(ETAG, ts(1000), None, None, None, Some(ts(2000))),
            ConditionalResult::Proceed
        );
    }

    #[test]
    fn test_if_unmodified_since_fails() {
        // Verify If-Unmodified-Since fails when the object changed too recently.
        assert_eq!(
            evaluate(ETAG, ts(2000), None, None, None, Some(ts(1000))),
            ConditionalResult::PreconditionFailed
        );
    }

    #[test]
    fn test_if_match_overrides_if_unmodified_since() {
        // Verify If-Match wins over a failing If-Unmodified-Since header.
        assert_eq!(
            evaluate(ETAG, ts(2000), Some(ETAG), None, None, Some(ts(1000))),
            ConditionalResult::Proceed
        );
    }

    #[test]
    fn test_if_none_match_overrides_if_modified_since() {
        // Verify If-None-Match wins over a conflicting If-Modified-Since header.
        assert_eq!(
            evaluate(ETAG, ts(2000), None, Some(ETAG), Some(ts(1000)), None),
            ConditionalResult::NotModified
        );
    }
}
