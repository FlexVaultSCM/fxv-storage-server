use crate::errors;
use crate::server;
use std::{
    fmt,
    net::SocketAddr,
    ops::Range,
    path,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};
use tokio::{runtime::Runtime, sync::Notify};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_PORT_WINDOW: u16 = 100;

/// A blocking RAII handle for an embedded fxv-storage-server instance.
///
/// The server runs on a dedicated thread with its own Tokio runtime so callers
/// can use it from synchronous unit tests.
pub struct TestServer {
    local_addr: SocketAddr,
    shutdown: Arc<Notify>,
    join_handle: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
pub enum TestServerError {
    Startup(errors::Error),
    StartupTimedOut(Duration),
    StartupChannelClosed,
    ThreadPanicked,
}

impl fmt::Display for TestServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Startup(err) => write!(f, "server startup failed: {}", err),
            Self::StartupTimedOut(timeout) => {
                write!(f, "timed out waiting {:?} for server startup", timeout)
            }
            Self::StartupChannelClosed => write!(f, "server startup channel closed unexpectedly"),
            Self::ThreadPanicked => write!(f, "server background thread panicked"),
        }
    }
}

impl std::error::Error for TestServerError {}

impl TestServer {
    pub fn new(serve_dir: &path::Path, start_port: u16) -> Result<Self, TestServerError> {
        Self::new_with_port_range(serve_dir, start_port..start_port + DEFAULT_PORT_WINDOW)
    }

    pub fn new_ephemeral(serve_dir: &path::Path) -> Result<Self, TestServerError> {
        Self::start(serve_dir, BindStrategy::Ephemeral)
    }

    pub fn new_with_port_range(
        serve_dir: &path::Path,
        port_range: Range<u16>,
    ) -> Result<Self, TestServerError> {
        Self::start(serve_dir, BindStrategy::PortRange(port_range))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn port(&self) -> u16 {
        self.local_addr.port()
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.local_addr)
    }

    fn start(serve_dir: &path::Path, bind_strategy: BindStrategy) -> Result<Self, TestServerError> {
        let serve_dir = serve_dir.to_path_buf();
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
                    let app = server::build_app_from_dir(&serve_dir).await?;
                    let shutdown_for_server = shutdown_inner.clone();

                    let _ = startup_tx.send(Ok(local_addr));
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

        let local_addr = match startup_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(local_addr)) => local_addr,
            Ok(Err(err)) => {
                join_handle
                    .join()
                    .map_err(|_| TestServerError::ThreadPanicked)?;
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
            shutdown,
            join_handle: Some(join_handle),
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(join_handle) = self.join_handle.take() {
            join_handle
                .join()
                .expect("background test server thread panicked");
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

async fn bind_first_available_port(
    port_range: Range<u16>,
) -> errors::Result<tokio::net::TcpListener> {
    for port in port_range.clone() {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(err) => return Err(err.into()),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AddrNotAvailable,
        format!(
            "failed to bind fxv-storage-server on any port in range [{}..{})",
            port_range.start, port_range.end
        ),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, net::TcpStream, time::Instant};

    #[test]
    fn test_blocking_server_serves_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("hello.txt"), b"hello world").expect("write");

        let server = TestServer::new_ephemeral(dir.path()).expect("start server");
        let response =
            reqwest::blocking::get(format!("{}/hello.txt", server.url())).expect("request");

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().expect("body"), "hello world");
    }

    #[test]
    fn test_port_range_constructor_uses_requested_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("hello.txt"), b"hello world").expect("write");

        let range = 44100..44110;
        let server = TestServer::new_with_port_range(dir.path(), range.clone()).expect("start");

        assert!(range.contains(&server.port()));
        let response =
            reqwest::blocking::get(format!("{}/hello.txt", server.url())).expect("request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    #[test]
    fn test_drop_shuts_server_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("hello.txt"), b"hello world").expect("write");

        let server = TestServer::new_ephemeral(dir.path()).expect("start");
        let addr = server.local_addr();
        drop(server);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if TcpStream::connect(addr).is_err() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "server still accepting connections after Drop"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn test_startup_error_propagates_to_sync_caller() {
        let missing_dir = tempfile::tempdir().expect("tempdir").path().join("missing");

        let result = TestServer::new_ephemeral(&missing_dir);
        assert!(matches!(result, Err(TestServerError::Startup(_))));
    }
}
