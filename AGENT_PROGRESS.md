# AGENT_PROGRESS.md

Tracks decisions made and changes applied so work can be resumed from any checkpoint.

---

## Stage 1 – API Research & Planning (Complete)

### Key Decisions
- **ETag format**: BLAKE3 (hex, double-quoted wire format). Intentional deviation from S3's MD5 — clients must not treat ETags as MD5.
- **Multipart ETag**: BLAKE3 of the fully assembled file (same as single-part PutObject). Not S3's `md5_of_parts-N` format.
- **Routing**: Axum wildcard `/{*key}` handler, single route for all methods. Query-param dispatch for PUT (PutObject vs UploadPart) and POST (CreateMultipartUpload vs CompleteMultipartUpload).
- **ServeDir**: Rejected — no ETag support, incompatible with our conditional header requirements.
- **Multipart state**: In-memory (`tokio::sync::RwLock<HashMap>`), not persisted. Lost on restart.
- **AbortMultipartUpload**: Planned as Stage 5.
- **ETag disk cache**: `.fxv-etag-cache/` subdirectory within the serve directory. Format: `<mtime_secs> <etag>` per file.
- **Content-Type**: Always `application/octet-stream` (no MIME detection).
- **If-Match on PutObject**: Supported (checks existing file ETag before overwrite).
- **XML module**: Named `s3_xml_compat.rs`.
- **Stages**: 2=GetObject, 3=PutObject, 4=Multipart, 5=Abort.

### Dependencies Approved
`tokio`, `axum`, `tower`, `tower-http`, `clap`, `blake3`, `error-chain`, `tracing` (0.1.41), `tracing-subscriber` (0.3.20), `tracing-appender` (0.2.4), `quick-xml`, `serde`, `uuid`, `bytes`, `httpdate`, `http`, `tokio-util`

Dev: `reqwest`, `tempfile`, `filetime`

---

## Stage 2 – GetObject (Complete)

### Files Created
- `fxv-storage-server/` — Rust crate root (nightly toolchain via `rust-toolchain.toml`)
- `src/lib.rs` — Library target, exports `build_app()` for integration tests
- `src/main.rs` — Binary target: CLI args (`--serve-dir`, `--port`), server startup
- `src/config.rs` — `Config` struct (serve_dir, port)
- `src/errors.rs` — `error_chain!` definitions: Io, StripPrefix, PathTraversal, InvalidRange, UploadNotFound, PartNotFound
- `src/etag.rs` — BLAKE3 ETag computation; `.fxv-etag-cache/` disk cache with mtime-based invalidation
- `src/store.rs` — `FileStore`: async directory walk, builds `HashMap<String, FileEntry>` at startup; `SharedStore = Arc<RwLock<FileStore>>`
- `src/conditional.rs` — RFC 7232 conditional evaluation (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since)
- `src/range.rs` — Range header parsing, `ByteRange`, `content_range_header()` formatting
- `src/handlers/get_object.rs` — Full GetObject handler: 200, 206, 304, 412, 416, 500
- `src/handlers/put_object.rs` — Stub (501 Not Implemented)
- `src/handlers/multipart.rs` — Stub (501 Not Implemented)
- `tests/get_object.rs` — 8 integration tests covering: 200 full, 404, ETag/Last-Modified headers, 206 range, 304 If-None-Match, 412 If-Match, nested paths, 416

### Test Results
- 30 unit tests: all pass
- 8 integration tests: all pass
- `cargo check`: clean
- `cargo clippy`: clean

### Git Commits
- `Initial project scaffold` — AGENT.md added
- `Stage 2: GetObject implementation` — full GetObject with tests (pending)

---

## Stage 3 – PutObject (Complete)

### Files Created/Modified
- `src/handlers/put_object.rs` — Full PutObject handler with:
  - Atomic write: body → temp file in serve_dir, BLAKE3 computed during streaming, then `rename()`
  - Conditional headers: `If-None-Match: *` (prevent overwrite), `If-Match: <etag>` (conditional replace)
  - Path traversal protection via `sanitize_key()`: strips leading `/`, rejects `..` and absolute components
  - Parent directory creation for nested keys
  - ETag disk cache update after successful write
  - In-memory `FileStore` index updated after successful write
- `src/store.rs` — Added `serve_dir: PathBuf` field to `FileStore`, `serve_dir()` accessor, `upsert()` now public
- `Cargo.toml` — Added `http-body-util = "0.1"` for `BodyExt::frame()` body streaming
- `tests/put_object.rs` — 9 integration tests

### Key Decisions
- **`If-Match: *`** on PutObject: treated as "object must exist, any ETag OK". If file doesn't exist → 412.
- **Nested key directories**: created with `create_dir_all()` before writing; failure is non-fatal (write will fail and return 500 anyway).
- **Leading `/` in key**: stripped by `sanitize_key()` before path resolution — matches S3 behavior where `/key` and `key` are equivalent.

### Test Results
- 33 unit tests: all pass
- 17 integration tests (8 GetObject + 9 PutObject): all pass
- `cargo check`: clean
- `cargo clippy`: clean

### Git Commits
- `Stage 3: PutObject implementation`

---

## Pending Stages

| Stage | Feature                     | Status  |
|-------|-----------------------------|---------|
| 4     | Multipart Upload            | pending |
| 5     | AbortMultipartUpload        | pending |
