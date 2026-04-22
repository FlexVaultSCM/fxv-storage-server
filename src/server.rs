use crate::{build_app, config::Config, errors::Result, store};
use axum::{Router, serve::ListenerExt as _};
use std::{future::Future, net::SocketAddr, path::Path};
use tokio::net::TcpListener;
use tracing::{info, warn};

/// Build the application router and backing file index for a serve directory.
pub async fn build_app_from_dir(serve_dir: &Path) -> Result<Router> {
    info!("Building file index from {:?}", serve_dir);
    let shared_store = store::build_shared_store(serve_dir).await?;
    Ok(build_app(shared_store))
}

/// Bind a TCP listener for the server.
pub async fn bind_listener(addr: SocketAddr) -> Result<TcpListener> {
    Ok(TcpListener::bind(addr).await?)
}

/// Serve the application until either the server exits or the shutdown future resolves.
pub async fn serve_with_shutdown<S>(listener: TcpListener, app: Router, shutdown: S) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let local_addr = listener.local_addr()?;
    info!("Listening on {}", local_addr);

    axum::serve(
        listener.tap_io(|stream| {
            if let Err(e) = stream.set_nodelay(true) {
                warn!("Failed to set TCP_NODELAY: {}", e);
            }
        }),
        app,
    )
    .with_graceful_shutdown(shutdown)
    .await?;

    Ok(())
}

/// Run the server from a full configuration object until the shutdown future resolves.
pub async fn run_with_shutdown<S>(config: &Config, shutdown: S) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let app = build_app_from_dir(&config.serve_dir).await?;
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = bind_listener(addr).await?;
    serve_with_shutdown(listener, app, shutdown).await
}
