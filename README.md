# fxv-storage-server

**FlexVault Storage Server** - a lightweight, async Rust HTTP server that implements a subset of the Amazon S3 API for serving and uploading files.

## Overview

`fxv-storage-server` is designed for use-cases where a full S3-compatible object store is overkill. It serves files from a local directory over HTTP, with S3-compatible request/response semantics.

**Supported operations:**
- `GET /{key}` - GetObject (with range requests and conditional headers)
- `PUT /{key}` - PutObject (atomic write, conditional headers) *(Stage 3)*
- `POST /{key}?uploads` - CreateMultipartUpload *(Stage 4)*
- `PUT /{key}?partNumber=N&uploadId=X` - UploadPart *(Stage 4)*
- `POST /{key}?uploadId=X` - CompleteMultipartUpload *(Stage 4)*
- `DELETE /{key}` - DeleteObject
- `DELETE /{key}?uploadId=X` - AbortMultipartUpload *(Stage 5)*

## Design Notes

- **ETags** use MD5. Single-part uploads and externally-present files use the content MD5; completed multipart uploads use the S3 multipart ETag formula.
- **Atomicity**: All writes use a temp-file-then-rename pattern, so partial files are never served.
- **Conditional headers**: Full RFC 7232 support (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since).
- **Range requests**: Single-range requests supported (S3 constraint: no multi-range).
- **Content-Type**: Always `application/octet-stream`.
- **ETag cache**: Precomputed ETags are cached in `.fxv-etag-cache/` inside the serve directory to avoid re-hashing on restart.
- **Multipart temp storage**: In-progress uploaded parts live under `.fxv-multipart-uploads/`, separate from the ETag cache.

## Usage

```
fxv-storage-server --serve-dir /path/to/files --port 3000
```

## Library usage in tests

This crate already exposes a library target via `src/lib.rs`, so test code can
depend on it directly and spin up a real server without already running inside
Tokio.

```rust
let server = fxv_storage_server::test_server::TestServer::new_ephemeral(serve_dir.path())?;
let base_url = server.url();
```

`TestServer` owns a background thread plus its own Tokio runtime and shuts the
server down automatically on drop.

## Building

Requires the Rust **nightly** toolchain (see `rust-toolchain.toml`).

```sh
cargo build --release
cargo test
```

## License

MIT

## Attributions

Design and API reference:
- [Amazon S3 API Reference](https://docs.aws.amazon.com/AmazonS3/latest/API/)
- [s3s-project/s3s](https://github.com/s3s-project/s3s) - reviewed for design inspiration on S3 API structuring in Rust
- [RFC 7232](https://tools.ietf.org/html/rfc7232) - HTTP conditional requests
