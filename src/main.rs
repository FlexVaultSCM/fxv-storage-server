use axum::serve::ListenerExt as _;
use clap::Parser;
use fxv_storage_server::{build_app, config::Config, store};
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "fxv-storage-server",
    about = "FlexVault Storage Server — S3-compatible file server"
)]
struct Cli {
    /// Directory to serve files from (and upload files to).
    #[arg(long, short = 'd')]
    serve_dir: PathBuf,

    /// TCP port to listen on.
    #[arg(long, short = 'p', default_value = "3000")]
    port: u16,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let serve_dir = cli.serve_dir.canonicalize().unwrap_or_else(|e| {
        eprintln!(
            "Cannot resolve serve-dir '{}': {}",
            cli.serve_dir.display(),
            e
        );
        std::process::exit(1);
    });

    let _config = Config {
        serve_dir: serve_dir.clone(),
        port: cli.port,
    };

    info!("Building file index from {:?}", serve_dir);
    let shared_store = store::build_shared_store(&serve_dir)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to build file index: {}", e);
            std::process::exit(1);
        });

    let app = build_app(shared_store);

    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    info!("Listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to bind {}: {}", addr, e);
            std::process::exit(1);
        });

    axum::serve(
        listener.tap_io(|stream| {
            if let Err(e) = stream.set_nodelay(true) {
                tracing::warn!("Failed to set TCP_NODELAY: {}", e);
            }
        }),
        app,
    )
    .await
    .expect("server error");
}
