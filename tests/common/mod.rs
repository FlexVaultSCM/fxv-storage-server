// == Std
use std::path::Path;

// == Internal
use fxv_storage_server::test_server::TestServer;

/// Start an embedded test server rooted at `serve_dir`.
pub fn spawn_server(serve_dir: &Path) -> TestServer {
    TestServer::new_ephemeral(serve_dir).expect("start server")
}
