# AGENTS.md - fxv-storage-server

Reference document for AI agents working on this codebase.
Keep this file up to date when making significant changes.

---

## Project Overview

**fxv-storage-server** (FlexVault Storage Server) is a production-aimed async Rust HTTP server
implementing a small, deliberate subset of the Amazon S3 API. It serves and accepts uploads of
files from a local directory. It is intentionally minimal - no ListObjects, no ACLs, no versioning,
no bucket management.

- **License**: MIT
- **Toolchain**: Rust nightly (pinned via `rust-toolchain.toml`)
- **Crate root**: `fxv-storage-server/` within the workspace

---

## Implemented S3 Operations

| Operation | Method | Notes |
|---|---|---|
| GetObject | `GET /{*key}` | Full and range responses; conditional headers; streaming |
| PutObject | `PUT /{*key}` | Atomic via temp-file + rename; conditional headers |
| CreateMultipartUpload | `POST /{*key}?uploads` | Returns XML upload ID |
| UploadPart | `PUT /{*key}?partNumber=N&uploadId=X` | Parts stored in `.fxv-multipart-uploads/` |
| CompleteMultipartUpload | `POST /{*key}?uploadId=X` | Assembles parts atomically; XML body |
| DeleteObject | `DELETE /{*key}` | Idempotent 204 delete; clears store/cache entry |
| AbortMultipartUpload | `DELETE /{*key}?uploadId=X` | Cleans up temp part files |

**Not implemented** (and not planned): ListObjects, HeadObject, HeadBucket, CreateBucket,
GetBucketLocation, versioning, ACLs, presigned URLs, CORS, lifecycle policies.

---

## Architecture

### Module Map

```
src/
  main.rs              - CLI (clap) entrypoint wired through the shared server runner
  lib.rs               - AppState, build_app(), exports all modules
  config.rs            - Config struct (serve_dir, port)
  errors.rs            - error_chain! error types
  store.rs             - FileStore: HashMap<key, FileEntry>, SharedStore = Arc<RwLock<FileStore>>
  etag.rs              - MD5 and multipart ETag helpers
  metadata_cache.rs    - Structured JSON metadata cache for ETag, checksums, and custom headers
  server.rs            - Shared async bootstrap helpers: build app, bind listener, serve with shutdown
  test_server.rs       - Blocking RAII helper for spinning up the server in synchronous tests
  conditional.rs       - RFC 7232 If-Match / If-None-Match / If-Modified-Since / If-Unmodified-Since
  range.rs             - Range header parsing, ByteRange, content_range_header()
  s3_xml_compat.rs     - quick-xml+serde types for multipart XML; S3 error response helpers
  multipart_state.rs   - SharedUploadState = Arc<RwLock<HashMap<uploadId, UploadEntry>>> + multipart temp dir constant
  handlers/
    delete_object.rs   - DeleteObject + AbortMultipartUpload DELETE dispatch
    get_object.rs      - GetObject: 200, 206, 304, 412, 416
    put_object.rs      - PutObject + UploadPart dispatch; sanitize_key()
    multipart.rs       - CreateMultipartUpload, CompleteMultipartUpload, AbortMultipartUpload internals
tests/
  delete_object.rs     - 3 integration tests
  get_object.rs        - 8 integration tests
  put_object.rs        - 9 integration tests
  multipart.rs         - 5 integration tests
  abort_multipart.rs   - 4 integration tests
  s3_compat.rs         - 8 integration tests using the aws-sdk-s3 client
test_scripts/
  rclone_test.py       - Python integration test using rclone as an S3 client (see below)
```

### Routing

Single Axum wildcard route: `/{*key}`, dispatched by HTTP method.
Query parameters further dispatch within PUT (`partNumber`/`uploadId`) and POST (`uploads`/`uploadId`).
There is no bucket concept - the bucket name in the S3 URL is absorbed into the key.

### AppState

```rust
pub struct AppState {
    pub store: SharedStore,        // Arc<RwLock<FileStore>>
    pub uploads: SharedUploadState, // Arc<RwLock<HashMap<String, UploadEntry>>>
}
```

### ETag Strategy

- MD5 hash of file content for normal objects, hex-encoded, double-quoted wire format: `"<32 hex chars>"`
- Completed multipart uploads use the S3 multipart ETag form: `"<32 hex chars>-<part count>"`
- Computed during upload (streaming) - no post-write re-read needed
- Cached to disk at `<serve_dir>/.fxv-metadata-cache/<rel/path/to/file>.json` as structured JSON
- Cache is mtime-invalidated: if file mtime changes, metadata is recomputed or refreshed
- Multipart ETags are preserved across restart via the on-disk cache; if the cache is missing, startup
  can only recompute a whole-object MD5 from bytes on disk because multipart part boundaries are gone
- Custom headers from metadata are replayed on applicable GET responses (200, 206, and 304)

### Atomic Writes

PutObject and CompleteMultipartUpload both write to a UUID-named temp file in the same directory
as the target, then `rename()` into place. On POSIX this is atomic - readers never see a partial
file.

### Concurrency Model

- `FileStore` is behind a `tokio::sync::RwLock`. Multiple concurrent GETs hold the read lock
  simultaneously; writes are exclusive.
- **TOCTOU fix**: The conditional check (If-Match / If-None-Match), temp-file rename,
  metadata-cache write, and store upsert are all performed under a single write-lock acquisition
  in both `put_object` and `complete_multipart_upload`. An early read-lock check is kept as an
  optimization to reject obviously-failing requests before streaming the body, but the write-lock
  re-check is the definitive gate.
- Metadata-cache writes use `tokio::fs::write` which is not atomic in isolation. It is safe
  because **all runtime call-sites hold the FileStore write lock** for the duration. The
  metadata synthesis call-site used during `FileStore::build` at startup runs before
  any requests are served. File-level locking is not needed - this is a single-process design.
- Multipart upload state (`SharedUploadState`) has its own independent `RwLock`.

### TCP_NODELAY

`TCP_NODELAY` is set on every accepted connection via `axum::serve::ListenerExt::tap_io` in
the shared server runner, reducing latency for small request/response exchanges.

### Embedded test usage

- The crate is already consumable as a library via `src/lib.rs`; no special Cargo target split is
  required for test code to depend on it.
- `test_server::TestServer` is the blocking-friendly helper for synchronous tests. It owns a
  background thread, creates its own Tokio runtime, exposes `url()` / `port()`, and shuts down on
  `Drop`.
- The CLI and the test helper should stay wired through `server.rs` so listener binding, app
  construction, and shutdown behavior remain aligned.

---

## Key Design Decisions

| Decision | Choice | Reason |
|---|---|---|
| ETag hash | MD5 | Better alignment with S3 and client checksum expectations |
| Multipart ETag | S3 multipart formula (`md5(part-md5s)-N`) | Matches S3 semantics |
| `tower-http::ServeDir` | Rejected | No ETag support; incompatible with conditional header requirements |
| Multipart state persistence | None (in-memory only) | Lost on restart - acceptable for our use case |
| Part temp storage | `<serve_dir>/.fxv-multipart-uploads/part-<id>-<n>.fxv_tmp` | Keeps upload scratch data separate from cached object metadata |
| Content-Type | Default `application/octet-stream`, overrideable via metadata | Supports server-controlled and persisted object metadata |
| Bucket concept | None | Bucket name is absorbed into the key path |
| `If-Match: *` on PutObject | "object must exist, any ETag OK" -> 412 if missing | Matches S3 behaviour |
| Leading `/` in key | Stripped by `sanitize_key()` | S3 treats `/key` and `key` as equivalent |
| XML library | `quick-xml` + `serde` | Lightweight; only a few types needed |
| Error responses | S3-compatible XML `<Error><Code>...</Code><Message>...</Message></Error>` | Required for S3 client compat |

---

## rclone Integration Test (`test_scripts/rclone_test.py`)

A Python 3 script that builds the server binary, spins it up on a random port, and exercises it
via `rclone`. Run with:

```
python3 test_scripts/rclone_test.py [--release]
```

### Required rclone flags

Two flags are required for rclone to work with this server:

| Flag | Reason |
|---|---|
| `--s3-no-check-bucket` | Without it rclone calls `HeadBucket` (-> 404) then `CreateBucket` (-> 500 or loop), because the server has no bucket concept |
| `--no-traverse` | Without it rclone calls `ListObjectsV2` before read operations (e.g. `rclone cat`); server returns 404 which rclone treats as "directory not found" |

Flags confirmed **not** required (and therefore absent from the script):

| Flag | Why not needed |
|---|---|
| `--ignore-checksum` | MD5 ETags now align with rclone's normal checksum handling |
| `--s3-disable-checksum` | Server ignores `Content-MD5` header entirely |
| `--s3-no-head` | HEAD requests work correctly; Axum auto-strips body for HEAD on GET routes |

The `--s3-provider Minio` flag is used to enable path-style addressing, matching the server's
`/{*key}` wildcard routing.

**Inline connection string gotcha**: The form `:s3,endpoint=http://127.0.0.1:PORT:bucket/key`
is broken - rclone's parser treats `:` in `http://` as a path separator, routing to host `"http"`
(10-second DNS timeout). Always use `--s3-endpoint http://...` as a CLI flag with `:s3:bucket/key`
as the remote path.

---

## S3 Client Compatibility Tests (`tests/s3_compat.rs`)

Uses `aws-sdk-s3` directly. Tests: GetObject 404, GetObject round-trip, DeleteObject round-trip,
GetObject range, GetObject If-None-Match 304, PutObject If-None-Match prevents overwrite,
full multipart flow, AbortMultipartUpload.

---

## Development Rules

### Language
- Prefer American English spelling in code, comments and documentation, e.g. "initialize",
"optimized", "behavior", "canceled", etc.
- Avoid using non-ASCII characters in code and comments like em and en dashes, curly quotes,
ellipses, etc. Emojis are permitted in documentation and comments sparingly if they add clarity
or emphasis.

### Rust

- Use the **nightly** toolchain (pinned via `rust-toolchain.toml`).
- Keep code idiomatic Rust - prefer `?` for error propagation, avoid unnecessary clones.
- Use `.expect("short message")` for logic invariant violations (coder mistakes). Prefer over
  bare `.unwrap()`.
- For fallible operations that are part of normal control flow, use `Result`/`error_chain`.
- Use `tracing` for logging. Keep `tracing`, `tracing-subscriber`, and `tracing-appender`
  version-pinned together in `Cargo.toml` (they must stay in sync).
- **Before adding any new dependency to `Cargo.toml`, pause and ask for confirmation.**
- Factor logic so it can be unit-tested without spinning up a full stack (see `conditional.rs`,
  `range.rs`, `store.rs`, `etag.rs`, `handlers/put_object.rs::sanitize_key`).
- Include integration tests (in `tests/`) for each feature that do spin up the full stack.
- All unit test modules must be inside a `mod tests` block gated by `#[cfg(test)]`.

### Code Quality (enforce before every commit)

- **`cargo fmt`** - code must be formatted; run `cargo fmt` and commit any changes.
- **`cargo clippy -- -D warnings`** - must produce zero warnings or errors.
- **`cargo test`** - all tests must pass.

### Git

- Commit after each meaningful unit of work with a descriptive message.
- Do not commit secrets, credentials, or generated artefacts (`target/`, `__pycache__/`, `*.pyc`
  are in `.gitignore`).

---

## Approved Dependencies

### Runtime

| Crate | Version | Purpose |
|---|---|---|
| `tokio` | 1 (full) | Async runtime |
| `axum` | 0.8 | Web framework |
| `tower` | 0.5 | Middleware |
| `tower-http` | 0.6 (trace) | HTTP tracing layer |
| `clap` | 4 (derive) | CLI argument parsing |
| `md-5` | 0.10 | MD5 hashing for ETags |
| `error-chain` | 0.12.4 | Structured error types |
| `tracing` | 0.1.41 | Instrumentation |
| `tracing-subscriber` | 0.3.20 | Log subscriber |
| `tracing-appender` | 0.2.4 | Log appending |
| `quick-xml` | 0.37 (serialize) | S3 XML serialization |
| `serde` | 1 (derive) | Serialization derive macros |
| `uuid` | 1 (v4) | Upload IDs, temp file names |
| `bytes` | 1 | Byte buffer utilities |
| `http-body-util` | 0.1 | Body streaming (`BodyExt::frame()`) |
| `httpdate` | 1 | HTTP date parsing/formatting |
| `http` | 1 | HTTP primitives (kept in sync with axum) |
| `tokio-util` | 0.7 (io) | `ReaderStream` for streaming responses |

### Dev / Test

| Crate | Purpose |
|---|---|
| `reqwest` | HTTP client for integration tests |
| `tempfile` | Temporary directories in tests |
| `filetime` | Setting file mtimes in tests |
| `aws-sdk-s3` + `aws-config` | S3 client compatibility tests |
