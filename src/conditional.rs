//! Conditional request evaluation.
//!
//! - `evaluate`: RFC 7232 conditions for GET/HEAD, giving 304 Not Modified, 412 Precondition Failed, or proceed.
//! - `evaluate_delete`: S3 conditional deletes (`If-Match` on DeleteObject), giving 404, 412, or proceed.
//! - `evaluate_write`: S3 conditional writes (PutObject, CompleteMultipartUpload), giving 412 or proceed.
//! - `single_header`, `write_condition_headers`: reading the headers, refusing one that can't be read or is repeated.
//!
//! S3 evaluation order for GET/HEAD per RFC 7232 s.6:
//! 1. If-Match               -> 412 on mismatch
//! 2. If-Unmodified-Since    -> 412 on modified (only if If-Match absent)
//! 3. If-None-Match          -> 304 on match
//! 4. If-Modified-Since      -> 304 on unmodified (only if If-None-Match absent)

// == Std
use std::time::SystemTime;

// == External
use http::{HeaderMap, HeaderName, StatusCode, header};

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

/// Returns `true` if `etag` (the object's, double-quoted) satisfies an `If-Match` / `If-None-Match`
/// value: the wildcard `*`, or a single ETag compared strongly, quoted or not, as S3 accepts both.
/// A weak ETag (`W/"..."`) or the comma-separated list form (rare in practice, not needed for the
/// S3 subset) never matches. Every conditional check (GET, PUT, DELETE) compares through this.
pub(crate) fn etag_matches(etag: &str, header_value: &str) -> bool {
    let v = header_value.trim();
    v == "*" || etag.trim_matches('"') == v.trim_matches('"')
}

/// Why a request's conditional headers can't be evaluated. Each is refused rather than treated as no condition, which
/// would turn a conditional write or delete into an unconditional one.
#[derive(Debug, PartialEq, Eq)]
pub enum ConditionHeaderError {
    /// The header, by its display name, has a value that isn't visible ASCII.
    Unreadable(&'static str),
    /// The header, by its display name, is given more than once.
    Repeated(&'static str),
    /// A write's `If-None-Match` has a value other than `*`, which S3 doesn't implement.
    UnsupportedIfNoneMatch,
}

impl ConditionHeaderError {
    /// The message an S3 error response gives for it.
    pub fn message(&self) -> String {
        match self {
            Self::Unreadable(name) => format!("The {name} header is invalid."),
            Self::Repeated(name) => format!("Only one {name} header is supported."),
            Self::UnsupportedIfNoneMatch => {
                "A header you provided implies functionality that is not implemented.".into()
            }
        }
    }
}

/// The value of the request's `name` header, if it has one, or an error for a value that can't be read or a header
/// given more than once. `display_name` is how the error refers to the header.
pub fn single_header(
    headers: &HeaderMap,
    name: &HeaderName,
    display_name: &'static str,
) -> Result<Option<String>, ConditionHeaderError> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(value), None) => value
            .to_str()
            .map(|value| Some(value.to_owned()))
            .map_err(|_| ConditionHeaderError::Unreadable(display_name)),
        (Some(_), Some(_)) => Err(ConditionHeaderError::Repeated(display_name)),
    }
}

/// A write's `If-Match` and `If-None-Match` values, for [`evaluate_write`]. `If-None-Match` must be `*`, the only value
/// S3 accepts on a write: any other, a quoted `"*"` or a list included, is refused.
pub fn write_condition_headers(headers: &HeaderMap) -> Result<(Option<String>, Option<String>), ConditionHeaderError> {
    let if_match = single_header(headers, &header::IF_MATCH, "If-Match")?;
    let if_none_match = single_header(headers, &header::IF_NONE_MATCH, "If-None-Match")?;
    if if_none_match.as_deref().is_some_and(|value| value != "*") {
        return Err(ConditionHeaderError::UnsupportedIfNoneMatch);
    }
    Ok((if_match, if_none_match))
}

/// Result of evaluating a write's conditional headers: a PutObject, or a CompleteMultipartUpload.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteCondition {
    /// Write the object.
    Proceed,
    /// Return 412 Precondition Failed.
    PreconditionFailed,
}

/// Evaluate a write's `If-Match` and `If-None-Match` headers, as read by [`write_condition_headers`], as S3
/// conditional writes do.
///
/// - `etag`: the current ETag of the object (double-quoted), or `None` if it doesn't exist.
/// - `if_match`: value of the `If-Match` header, if present: the write proceeds only onto an object with that ETag.
/// - `if_none_match`: value of the `If-None-Match` header, if present, which is `*`: the write proceeds only when there
///   is no object.
///
/// `If-Match` on a missing object fails with 412 here. S3 answers such a write with 404.
pub fn evaluate_write(etag: Option<&str>, if_match: Option<&str>, if_none_match: Option<&str>) -> WriteCondition {
    match (etag, if_match, if_none_match) {
        (Some(_), _, Some("*")) => WriteCondition::PreconditionFailed,
        (Some(etag), Some(if_match), _) if !etag_matches(etag, if_match) => WriteCondition::PreconditionFailed,
        (None, Some(_), _) => WriteCondition::PreconditionFailed,
        _ => WriteCondition::Proceed,
    }
}

/// Result of evaluating a DeleteObject request's `If-Match` header.
#[derive(Debug, PartialEq, Eq)]
pub enum DeleteCondition {
    /// Delete the object, or return 204 for a missing one when no condition was given.
    Proceed,
    /// Return 404 NoSuchKey: a condition was given and there is no object to evaluate it against.
    NotFound,
    /// Return 412 Precondition Failed: the object's ETag does not match.
    PreconditionFailed,
}

/// Evaluate a DeleteObject request's `If-Match` header, as S3 conditional deletes do.
///
/// - `etag`: the current ETag of the object (double-quoted), or `None` if it doesn't exist.
/// - `if_match`: value of the `If-Match` header, if present: `*` to require that the object exists, or an ETag to
///   require that it is unchanged. The ETag may be quoted or not, as S3 accepts both.
///
/// Without `If-Match` a delete always proceeds, and a missing object still gets 204. The comparison is strong: a
/// weak ETag (`W/"..."`) or a comma-separated list never matches, so the delete is refused rather than guessed at.
/// `If-None-Match` is not evaluated, as S3 general purpose buckets support only `If-Match` on DeleteObject.
pub fn evaluate_delete(etag: Option<&str>, if_match: Option<&str>) -> DeleteCondition {
    let Some(if_match) = if_match else {
        return DeleteCondition::Proceed;
    };
    let Some(etag) = etag else {
        return DeleteCondition::NotFound;
    };
    if etag_matches(etag, if_match) {
        DeleteCondition::Proceed
    } else {
        DeleteCondition::PreconditionFailed
    }
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

    #[test]
    fn test_write_if_none_match_wildcard_requires_no_object() {
        // Verify `If-None-Match: *` proceeds onto a missing object and fails onto an existing one.
        assert_eq!(evaluate_write(None, None, Some("*")), WriteCondition::Proceed);
        assert_eq!(
            evaluate_write(Some(ETAG), None, Some("*")),
            WriteCondition::PreconditionFailed
        );
    }

    #[test]
    fn test_write_if_match_requires_the_etag() {
        // Verify `If-Match` proceeds onto a matching ETag, and fails onto another ETag or a missing object.
        assert_eq!(evaluate_write(Some(ETAG), Some(ETAG), None), WriteCondition::Proceed);
        assert_eq!(
            evaluate_write(Some(ETAG), Some(OTHER_ETAG), None),
            WriteCondition::PreconditionFailed
        );
        assert_eq!(
            evaluate_write(None, Some(ETAG), None),
            WriteCondition::PreconditionFailed
        );
    }

    #[test]
    fn test_write_without_conditions_proceeds() {
        // Verify an unconditional write proceeds whether or not the object exists.
        assert_eq!(evaluate_write(None, None, None), WriteCondition::Proceed);
        assert_eq!(evaluate_write(Some(ETAG), None, None), WriteCondition::Proceed);
    }

    fn headers(pairs: &[(&HeaderName, &[u8])]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append((*name).clone(), http::HeaderValue::from_bytes(value).unwrap());
        }
        headers
    }

    #[test]
    fn test_write_condition_headers_read_one_of_each() {
        // Verify an absent condition is None, and a single readable one is its value.
        assert_eq!(write_condition_headers(&HeaderMap::new()), Ok((None, None)));
        assert_eq!(
            write_condition_headers(&headers(&[
                (&header::IF_MATCH, ETAG.as_bytes()),
                (&header::IF_NONE_MATCH, b"*")
            ])),
            Ok((Some(ETAG.to_string()), Some("*".to_string())))
        );
    }

    #[test]
    fn test_write_condition_headers_refuse_what_they_cannot_evaluate() {
        // Verify a repeated or unreadable condition, and an If-None-Match other than a bare `*`, is an error rather
        // than no condition.
        assert_eq!(
            write_condition_headers(&headers(&[
                (&header::IF_NONE_MATCH, b"\"abc\""),
                (&header::IF_NONE_MATCH, b"*")
            ])),
            Err(ConditionHeaderError::Repeated("If-None-Match"))
        );
        assert_eq!(
            write_condition_headers(&headers(&[(&header::IF_MATCH, b"\xff")])),
            Err(ConditionHeaderError::Unreadable("If-Match"))
        );
        for value in [&b"\"*\""[..], b"*, \"abc\"", b"\"abc\""] {
            assert_eq!(
                write_condition_headers(&headers(&[(&header::IF_NONE_MATCH, value)])),
                Err(ConditionHeaderError::UnsupportedIfNoneMatch),
                "{value:?}"
            );
        }
    }

    #[test]
    fn test_delete_without_if_match_proceeds_whether_or_not_the_object_exists() {
        // Verify an unconditional delete proceeds, including for a missing object.
        assert_eq!(evaluate_delete(Some(ETAG), None), DeleteCondition::Proceed);
        assert_eq!(evaluate_delete(None, None), DeleteCondition::Proceed);
    }

    #[test]
    fn test_delete_if_match_etag() {
        // Verify a matching ETag proceeds, quoted or not, and a different one fails.
        assert_eq!(evaluate_delete(Some(ETAG), Some(ETAG)), DeleteCondition::Proceed);
        assert_eq!(evaluate_delete(Some(ETAG), Some("abc123")), DeleteCondition::Proceed);
        assert_eq!(
            evaluate_delete(Some(ETAG), Some(OTHER_ETAG)),
            DeleteCondition::PreconditionFailed
        );
    }

    #[test]
    fn test_delete_if_match_wildcard_requires_the_object() {
        // Verify If-Match: * proceeds for an existing object.
        assert_eq!(evaluate_delete(Some(ETAG), Some("*")), DeleteCondition::Proceed);
        assert_eq!(evaluate_delete(None, Some("*")), DeleteCondition::NotFound);
    }

    #[test]
    fn test_delete_if_match_on_a_missing_object_is_not_found() {
        // Verify a conditional delete of a missing object is 404, as S3 returns.
        assert_eq!(evaluate_delete(None, Some(ETAG)), DeleteCondition::NotFound);
    }

    #[test]
    fn test_delete_weak_etag_or_list_never_matches() {
        // Verify forms a strong comparison can't satisfy refuse the delete.
        assert_eq!(
            evaluate_delete(Some(ETAG), Some("W/\"abc123\"")),
            DeleteCondition::PreconditionFailed
        );
        assert_eq!(
            evaluate_delete(Some(ETAG), Some("\"xyz789\", \"abc123\"")),
            DeleteCondition::PreconditionFailed
        );
    }

    #[test]
    fn test_etag_matches() {
        // Verify the one comparison every conditional check uses.
        assert!(etag_matches(ETAG, ETAG));
        assert!(etag_matches(ETAG, "abc123"));
        assert!(etag_matches(ETAG, " \"abc123\" "));
        assert!(etag_matches(ETAG, "*"));
        assert!(!etag_matches(ETAG, OTHER_ETAG));
        assert!(!etag_matches(ETAG, "W/\"abc123\""));
        assert!(!etag_matches(ETAG, "\"xyz789\", \"abc123\""));
    }
}
