// == Std
use std::{future::Future, net::SocketAddr, path::Path};

// == Internal
use crate::{build_app, config::Config, errors::Result, store};

// == External
use axum::{Router, serve::ListenerExt as _};
use tokio::net::TcpListener;
use tracing::{info, warn};

/// Build the shared file index for a serve directory.
pub async fn build_store_for_dir(serve_dir: &Path) -> Result<store::SharedStore> {
    info!("Building file index from {:?}", serve_dir);
    store::build_shared_store(serve_dir).await
}

/// Build the application router around an existing shared store.
pub fn build_app_from_store(shared_store: store::SharedStore) -> Router {
    build_app(shared_store)
}

/// Build the application router and backing file index for a serve directory.
pub async fn build_store_and_app(serve_dir: &Path) -> Result<(store::SharedStore, Router)> {
    let shared_store = build_store_for_dir(serve_dir).await?;
    let app = build_app_from_store(shared_store.clone());
    Ok((shared_store, app))
}

/// Build the application router and backing file index for a serve directory.
pub async fn build_app_from_dir(serve_dir: &Path) -> Result<Router> {
    let (_, app) = build_store_and_app(serve_dir).await?;
    Ok(app)
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
    let (_, app) = build_store_and_app(&config.serve_dir).await?;
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = bind_listener(addr).await?;
    serve_with_shutdown(listener, app, shutdown).await
}
