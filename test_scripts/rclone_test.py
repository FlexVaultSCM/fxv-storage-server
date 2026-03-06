#!/usr/bin/env python3
"""
rclone_test.py - Integration tests for fxv-storage-server using rclone.

Spins up a local fxv-storage-server instance, generates test files, and
exercises the server through rclone's S3 backend, verifying PutObject,
GetObject, and multipart upload round-trips.

Requirements:
  - rclone installed and in PATH  (https://rclone.org/install/)
  - fxv-storage-server binary built:
      cargo build              # debug (default)
      cargo build --release    # release

Usage:
  python3 test_scripts/rclone_test.py [--profile debug|release] [--port PORT]

Run from anywhere; the script resolves the binary relative to its own location.

Notes on rclone S3 compatibility:
  - Uses the Minio provider (--s3-provider Minio), which automatically enables
    path-style addressing to match our server's /{*key} wildcard routing.
    Note: do NOT use the inline connection-string form with endpoint=http://...
    because rclone's parser treats the ":" in "http://" as a path separator.

  Two flags are required:
  - --s3-no-check-bucket: without it rclone calls HeadBucket (-> 404) and then
    CreateBucket (PUT /bucket-name -> 500), because the server has no bucket
    concept; rclone retries indefinitely and hangs.
  - --no-traverse: without it rclone calls ListObjectsV2 before read operations
    (e.g. `rclone cat`); our server returns 404 for listing which rclone treats
    as "directory not found". For upload-only operations (copyto, copy) a 404
    on listing is treated as an empty destination and uploads proceed regardless.

  Flags investigated and found NOT to be required:
  - --s3-disable-checksum: rclone sends Content-MD5 on PUT; our server ignores
    it, so operations succeed either way.
  - --ignore-checksum: rclone only compares ETags as MD5 when they are exactly
    32 hex chars. Our BLAKE3 ETags are 64 hex chars, so rclone skips checksum
    comparison entirely regardless of this flag.
  - --s3-no-head: rclone does HEAD before and after each upload; our server
    handles HEAD correctly (Axum auto-strips the body for HEAD on GET routes).

  Tests that require ListObjects (rclone ls, rclone sync) are intentionally
  omitted, as that API is out of scope for fxv-storage-server.
"""

import argparse
import hashlib
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import time

# ---------------------------------------------------------------------------
# Constants / defaults
# ---------------------------------------------------------------------------

BUCKET = "test-bucket"

# Applied to every rclone invocation.
# Both flags are required:
#   --s3-no-check-bucket: without it rclone calls HeadBucket (-> 404) then
#     CreateBucket (PUT /bucket-name -> 500), retrying indefinitely.
#   --no-traverse: without it rclone calls ListObjectsV2 before read
#     operations (e.g. `rclone cat`); our server returns 404 for listing
#     which rclone treats as "directory not found" -> error.
#     Note: for upload-only commands (copyto, copy) a 404 from ListObjectsV2
#     is treated as an empty destination and uploads proceed fine regardless.
_BASE_FLAGS = [
    "--s3-no-check-bucket",
    "--no-traverse",
]

PASS = "\033[32mPASS\033[0m"
FAIL = "\033[31mFAIL\033[0m"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_failures: list[str] = []


def die(msg: str) -> None:
    print(f"\nERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def check_rclone() -> None:
    if shutil.which("rclone") is None:
        die(
            "rclone is not installed or not in PATH.\n"
            "  Install it from: https://rclone.org/install/"
        )
    result = subprocess.run(["rclone", "version"], capture_output=True, text=True)
    version_line = (result.stdout.splitlines() or ["(unknown)"])[0]
    print(f"rclone: {version_line}")


def find_binary(profile: str) -> pathlib.Path:
    workspace = pathlib.Path(__file__).resolve().parent.parent
    exe = "fxv-storage-server.exe" if sys.platform == "win32" else "fxv-storage-server"
    binary = workspace / "fxv-storage-server" / "target" / profile / exe
    if not binary.exists():
        die(
            f"Binary not found: {binary}\n"
            f"  Build it with:  cargo build{'  --release' if profile == 'release' else ''}\n"
            f"  (inside {workspace / 'fxv-storage-server'})"
        )
    return binary


def wait_for_server(port: int, timeout: float = 10.0) -> None:
    """Wait until the server accepts TCP connections on *port*."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=0.5)
            s.close()
            return
        except OSError:
            time.sleep(0.1)
    die(f"Server on port {port} did not become ready within {timeout:.0f}s")


def sha256(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


class RcloneRunner:
    """Thin wrapper that calls rclone with pre-configured S3 flags."""

    def __init__(self, port: int) -> None:
        # Use --s3-* flags rather than an inline connection string.
        # Inline connection strings break when the endpoint URL contains "://"
        # because rclone's parser treats the first ":" as a path separator.
        #
        # Minio provider automatically enables path-style addressing, which
        # matches our server's /{*key} wildcard routing.
        self._port = port
        self._s3_flags = [
            "--s3-provider", "Minio",
            "--s3-access-key-id", "fxvtest",
            "--s3-secret-access-key", "fxvtest",
            "--s3-endpoint", f"http://127.0.0.1:{port}",
        ]

    def rpath(self, key: str) -> str:
        """Full rclone remote path for a key inside BUCKET."""
        return f":s3:{BUCKET}/{key}"

    def remote_bucket(self) -> str:
        """Remote path pointing at the bucket root (for rclone copy)."""
        return f":s3:{BUCKET}"

    def __call__(self, *args) -> subprocess.CompletedProcess:
        """Run rclone, print the command, raise on failure."""
        cmd = ["rclone"] + list(args) + _BASE_FLAGS + self._s3_flags
        print("    $", " ".join(str(a) for a in cmd))
        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode != 0:
            if result.stdout.strip():
                print(f"    stdout: {result.stdout.strip()}")
            if result.stderr.strip():
                print(f"    stderr: {result.stderr.strip()}")
            raise RuntimeError(
                f"rclone exited {result.returncode}: {' '.join(str(a) for a in args[:3])}"
            )
        return result


# ---------------------------------------------------------------------------
# Test harness
# ---------------------------------------------------------------------------

def test(name: str, fn) -> None:
    print(f"\n[TEST] {name}")
    try:
        fn()
        print(f"  [{PASS}]")
    except Exception as exc:
        print(f"  [{FAIL}] {exc}")
        _failures.append(name)


# ---------------------------------------------------------------------------
# Test cases
# ---------------------------------------------------------------------------

def run_tests(rc: RcloneRunner, up: pathlib.Path, dl: pathlib.Path) -> int:
    """Run all tests; returns total count."""
    cases = [
        ("Small text file round-trip (PutObject + GetObject)",
         _t_small_roundtrip),
        ("Overwrite: upload v1 then v2, verify v2 returned",
         _t_overwrite),
        ("Binary 256 KiB round-trip (PutObject + GetObject)",
         _t_binary),
        ("Nested key path (a/b/c/file.txt)",
         _t_nested),
        ("rclone cat streams object body",
         _t_cat),
        ("Directory upload with rclone copy --no-traverse",
         _t_dir_upload),
        ("Multipart upload: 8 MiB with 5 MiB cutoff",
         _t_multipart),
    ]
    for name, fn in cases:
        test(name, lambda f=fn: f(rc, up, dl))
    return len(cases)


def _t_small_roundtrip(rc: RcloneRunner, up: pathlib.Path, dl: pathlib.Path) -> None:
    src = up / "small.txt"
    src.write_text("Hello from rclone!\n" * 20)
    rc("copyto", str(src), rc.rpath("small.txt"))
    dst = dl / "small.txt"
    rc("copyto", rc.rpath("small.txt"), str(dst))
    if src.read_bytes() != dst.read_bytes():
        raise RuntimeError("Content mismatch after round-trip")


def _t_overwrite(rc: RcloneRunner, up: pathlib.Path, dl: pathlib.Path) -> None:
    f = up / "overwrite.txt"
    f.write_text("version 1")
    rc("copyto", str(f), rc.rpath("overwrite.txt"))
    f.write_text("version 2")
    rc("copyto", str(f), rc.rpath("overwrite.txt"))
    dst = dl / "overwrite.txt"
    rc("copyto", rc.rpath("overwrite.txt"), str(dst))
    got = dst.read_text()
    if got != "version 2":
        raise RuntimeError(f"Expected 'version 2', got {got!r}")


def _t_binary(rc: RcloneRunner, up: pathlib.Path, dl: pathlib.Path) -> None:
    src = up / "binary.bin"
    src.write_bytes(os.urandom(256 * 1024))
    rc("copyto", str(src), rc.rpath("binary.bin"))
    dst = dl / "binary.bin"
    rc("copyto", rc.rpath("binary.bin"), str(dst))
    if sha256(src) != sha256(dst):
        raise RuntimeError("SHA-256 mismatch for binary round-trip")


def _t_nested(rc: RcloneRunner, up: pathlib.Path, dl: pathlib.Path) -> None:
    src = up / "nested.txt"
    src.write_text("deep nested content")
    rc("copyto", str(src), rc.rpath("a/b/c/nested.txt"))
    dst = dl / "nested.txt"
    rc("copyto", rc.rpath("a/b/c/nested.txt"), str(dst))
    if dst.read_text() != "deep nested content":
        raise RuntimeError("Nested key content mismatch")


def _t_cat(rc: RcloneRunner, up: pathlib.Path, dl: pathlib.Path) -> None:
    src = up / "cat.txt"
    src.write_text("meow")
    rc("copyto", str(src), rc.rpath("cat.txt"))
    result = rc("cat", rc.rpath("cat.txt"))
    if result.stdout.strip() != "meow":
        raise RuntimeError(f"rclone cat returned {result.stdout!r}")


def _t_dir_upload(rc: RcloneRunner, up: pathlib.Path, dl: pathlib.Path) -> None:
    """rclone copy uploads a directory tree; --no-traverse skips ListObjects."""
    tree = up / "tree"
    tree.mkdir(exist_ok=True)
    (tree / "alpha.txt").write_text("alpha")
    (tree / "beta.txt").write_text("beta")
    (tree / "sub").mkdir(exist_ok=True)
    (tree / "sub" / "gamma.txt").write_text("gamma")

    rc("copy", str(tree), f"{rc.remote_bucket()}/tree")

    for rel, expected in [
        ("alpha.txt", "alpha"),
        ("beta.txt", "beta"),
        ("sub/gamma.txt", "gamma"),
    ]:
        dst = dl / f"tree_{rel.replace('/', '_')}"
        rc("copyto", rc.rpath(f"tree/{rel}"), str(dst))
        got = dst.read_text()
        if got != expected:
            raise RuntimeError(f"tree/{rel}: expected {expected!r}, got {got!r}")


def _t_multipart(rc: RcloneRunner, up: pathlib.Path, dl: pathlib.Path) -> None:
    """
    Upload an 8 MiB file with a 5 MiB cutoff, forcing rclone to use
    CreateMultipartUpload -> UploadPart x 2 -> CompleteMultipartUpload.
    """
    src = up / "large.bin"
    src.write_bytes(os.urandom(8 * 1024 * 1024))
    rc(
        "copyto", str(src), rc.rpath("large.bin"),
        "--s3-upload-cutoff=5242880",  # 5 MiB -> triggers multipart for 8 MiB file
        "--s3-chunk-size=5242880",     # 5 MiB parts (rclone enforced minimum)
    )
    dst = dl / "large.bin"
    rc("copyto", rc.rpath("large.bin"), str(dst))
    if sha256(src) != sha256(dst):
        raise RuntimeError("SHA-256 mismatch for multipart round-trip")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(
        description="rclone integration tests for fxv-storage-server"
    )
    parser.add_argument(
        "--profile",
        choices=["debug", "release"],
        default="debug",
        help="Cargo build profile to use (default: debug)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=18080,
        help="Port to run the server on (default: 18080)",
    )
    args = parser.parse_args()

    check_rclone()

    binary = find_binary(args.profile)
    print(f"Binary:  {binary}")
    print(f"Port:    {args.port}")

    serve_dir = pathlib.Path(tempfile.mkdtemp(prefix="fxv-serve-"))
    upload_dir = pathlib.Path(tempfile.mkdtemp(prefix="fxv-up-"))
    download_dir = pathlib.Path(tempfile.mkdtemp(prefix="fxv-dl-"))

    print(f"Serve dir: {serve_dir}\n")

    server = subprocess.Popen(
        [str(binary), "--serve-dir", str(serve_dir), "--port", str(args.port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    print(f"Server PID {server.pid} - waiting for ready...")

    try:
        wait_for_server(args.port)
        print("Server ready.\n")

        rc = RcloneRunner(args.port)
        total = run_tests(rc, upload_dir, download_dir)

        print()
        print("=" * 60)
        if _failures:
            print(f"  FAILED - {len(_failures)}/{total} test(s) failed:")
            for name in _failures:
                print(f"    - {name}")
            print("=" * 60)
            sys.exit(1)
        else:
            print(f"  ALL {total} TESTS PASSED")
            print("=" * 60)
            sys.exit(0)

    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.send_signal(9)
        for d in (serve_dir, upload_dir, download_dir):
            shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    main()
