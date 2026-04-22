// == Std
use std::{future, path::PathBuf, process};

// == Internal
use fxv_storage_server::{config::Config, server};

// == External
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "fxv-storage-server",
    about = "FlexVault Storage Server - S3-compatible file server"
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
        eprintln!("Cannot resolve serve-dir '{}': {}", cli.serve_dir.display(), e);
        process::exit(1);
    });

    let config = Config {
        serve_dir: serve_dir.clone(),
        port: cli.port,
    };

    server::run_with_shutdown(&config, future::pending())
        .await
        .unwrap_or_else(|e| {
            eprintln!("Server failed: {}", e);
            process::exit(1);
        });
}
