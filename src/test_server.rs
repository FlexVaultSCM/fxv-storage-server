// == Std
use std::{
    error, fmt,
    future::Future,
    io,
    net::SocketAddr,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

// == Internal
use crate::{errors, metadata_cache, server, store::SharedStore};

// == External
use tokio::{runtime::Runtime, sync::Notify};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_PORT_WINDOW: u16 = 100;

/// A blocking RAII handle for an embedded fxv-storage-server instance.
///
/// The server runs on a dedicated thread with its own Tokio runtime so callers
/// can use it from synchronous unit tests.
pub struct TestServer {
    local_addr: SocketAddr,
    serve_dir: PathBuf,
    store: SharedStore,
    shutdown: Arc<Notify>,
    join_handle: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
pub enum TestServerError {
    Startup(errors::Error),
    Operation(errors::Error),
    StartupTimedOut(Duration),
    StartupChannelClosed,
    ThreadPanicked,
}

impl fmt::Display for TestServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Startup(err) => write!(f, "server startup failed: {}", err),
            Self::Operation(err) => write!(f, "server operation failed: {}", err),
            Self::StartupTimedOut(timeout) => {
                write!(f, "timed out waiting {:?} for server startup", timeout)
            }
            Self::StartupChannelClosed => write!(f, "server startup channel closed unexpectedly"),
            Self::ThreadPanicked => write!(f, "server background thread panicked"),
        }
    }
}

impl error::Error for TestServerError {}

impl TestServer {
    /// Start a server within a default port window beginning at `start_port`.
    pub fn new(serve_dir: &Path, start_port: u16) -> Result<Self, TestServerError> {
        Self::new_with_port_range(serve_dir, start_port..start_port + DEFAULT_PORT_WINDOW)
    }

    /// Start a server on an ephemeral localhost port.
    pub fn new_ephemeral(serve_dir: &Path) -> Result<Self, TestServerError> {
        Self::start(serve_dir, BindStrategy::Ephemeral)
    }

    /// Start a server on the first available port in `port_range`.
    pub fn new_with_port_range(serve_dir: &Path, port_range: Range<u16>) -> Result<Self, TestServerError> {
        Self::start(serve_dir, BindStrategy::PortRange(port_range))
    }

    /// Return the bound local socket address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Return the bound local TCP port.
    pub fn port(&self) -> u16 {
        self.local_addr.port()
    }

    /// Return the base URL for this server.
    pub fn url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    /// Add or replace a server-controlled custom header for an indexed object.
    pub fn add_custom_header(&self, path: &str, key: &str, value: &str) -> Result<(), TestServerError> {
        let rel_path = crate::handlers::put_object::sanitize_key(path)
            .ok_or_else(|| invalid_input_error("The specified object key is invalid."))
            .map_err(TestServerError::Operation)?;
        let object_key = rel_path_to_key(&rel_path);
        let serve_dir = self.serve_dir.clone();
        let store = self.store.clone();
        let key = key.to_owned();
        let value = value.to_owned();

        block_on_result(async move {
            let mut store = store.write().await;
            let existing = store
                .get(&object_key)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("object '{}' not found", object_key)))
                .map_err(errors::Error::from)?;
            let mut custom_headers = existing.custom_headers.clone();
            metadata_cache::upsert_custom_header(&mut custom_headers, &key, &value)?;

            let metadata = metadata_cache::ObjectMetadataCache::from_current_state(
                existing.modified,
                existing.etag.clone(),
                existing.checksums.clone(),
                custom_headers.clone(),
            );
            metadata_cache::save_metadata_cache(&serve_dir, &rel_path, &metadata).await;

            store.upsert(
                object_key.clone(),
                crate::store::FileEntry {
                    custom_headers,
                    ..existing
                },
            );
            Ok(())
        })
        .map_err(TestServerError::Operation)
    }

    fn start(serve_dir: &Path, bind_strategy: BindStrategy) -> Result<Self, TestServerError> {
        let serve_dir = serve_dir.to_path_buf();
        let serve_dir_for_thread = serve_dir.clone();
        let shutdown = Arc::new(Notify::new());
        let shutdown_inner = shutdown.clone();
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);

        let join_handle = thread::spawn(move || {
            let runtime = match Runtime::new() {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = startup_tx.send(Err(e.into()));
                    return;
                }
            };

            runtime.block_on(async move {
                let result = async {
                    let listener = bind_strategy.bind_listener().await?;
                    let local_addr = listener.local_addr()?;
                    let (store, app) = server::build_store_and_app(&serve_dir_for_thread).await?;
                    let shutdown_for_server = shutdown_inner.clone();

                    let _ = startup_tx.send(Ok(StartupSuccess { local_addr, store }));
                    server::serve_with_shutdown(listener, app, async move {
                        shutdown_for_server.notified().await;
                    })
                    .await
                }
                .await;

                if let Err(err) = result {
                    let _ = startup_tx.send(Err(err));
                }
            });
        });

        let StartupSuccess { local_addr, store } = match startup_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(started)) => started,
            Ok(Err(err)) => {
                join_handle.join().map_err(|_| TestServerError::ThreadPanicked)?;
                return Err(TestServerError::Startup(err));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                shutdown.notify_one();
                let _ = join_handle.join();
                return Err(TestServerError::StartupTimedOut(STARTUP_TIMEOUT));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return match join_handle.join() {
                    Ok(()) => Err(TestServerError::StartupChannelClosed),
                    Err(_) => Err(TestServerError::ThreadPanicked),
                };
            }
        };

        Ok(Self {
            local_addr,
            serve_dir,
            store,
            shutdown,
            join_handle: Some(join_handle),
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(join_handle) = self.join_handle.take() {
            join_handle.join().expect("background test server thread panicked");
        }
    }
}

enum BindStrategy {
    Ephemeral,
    PortRange(Range<u16>),
}

impl BindStrategy {
    async fn bind_listener(&self) -> errors::Result<tokio::net::TcpListener> {
        match self {
            Self::Ephemeral => server::bind_listener(SocketAddr::from(([127, 0, 0, 1], 0))).await,
            Self::PortRange(port_range) => bind_first_available_port(port_range.clone()).await,
        }
    }
}

async fn bind_first_available_port(port_range: Range<u16>) -> errors::Result<tokio::net::TcpListener> {
    for port in port_range.clone() {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {}
            Err(err) => return Err(err.into()),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        format!(
            "failed to bind fxv-storage-server on any port in range [{}..{})",
            port_range.start, port_range.end
        ),
    )
    .into())
}

struct StartupSuccess {
    local_addr: SocketAddr,
    store: SharedStore,
}

fn rel_path_to_key(rel_path: &Path) -> String {
    rel_path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn invalid_input_error(message: &str) -> errors::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}

fn block_on_result<F, T>(future: F) -> errors::Result<T>
where
    F: Future<Output = errors::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        thread::spawn(move || Runtime::new()?.block_on(future))
            .join()
            .map_err(|_| errors::Error::from(io::Error::other("metadata helper thread panicked")))?
    } else {
        Runtime::new()?.block_on(future)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, net::TcpStream, thread, time::Instant};

    #[test]
    fn test_blocking_server_serves_file() {
        // Create a serve directory containing one readable object.
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("hello.txt"), b"hello world").expect("write");

        // Start the blocking test server and request the object through HTTP.
        let server = TestServer::new_ephemeral(dir.path()).expect("start server");
        let response = reqwest::blocking::get(format!("{}/hello.txt", server.url())).expect("request");

        // Verify the response status and body match the on-disk file.
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().expect("body"), "hello world");
    }

    #[test]
    fn test_port_range_constructor_uses_requested_range() {
        // Create a serve directory containing one readable object.
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("hello.txt"), b"hello world").expect("write");

        // Start the server inside a constrained port range.
        let range = 44100..44110;
        let server = TestServer::new_with_port_range(dir.path(), range.clone()).expect("start");

        // Verify the chosen port and request handling both stay within that range.
        assert!(range.contains(&server.port()));
        let response = reqwest::blocking::get(format!("{}/hello.txt", server.url())).expect("request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    #[test]
    fn test_drop_shuts_server_down() {
        // Create a serve directory and start an embedded server.
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("hello.txt"), b"hello world").expect("write");

        // Drop the server handle after capturing its bound address.
        let server = TestServer::new_ephemeral(dir.path()).expect("start");
        let addr = server.local_addr();
        drop(server);

        // Poll until the socket stops accepting new connections.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if TcpStream::connect(addr).is_err() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "server still accepting connections after Drop"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn test_startup_error_propagates_to_sync_caller() {
        // Point the server at a directory that does not exist.
        let missing_dir = tempfile::tempdir().expect("tempdir").path().join("missing");

        // Verify startup surfaces the background failure synchronously.
        let result = TestServer::new_ephemeral(&missing_dir);
        assert!(matches!(result, Err(TestServerError::Startup(_))));
    }
}
