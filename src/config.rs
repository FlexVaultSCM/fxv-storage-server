// == Std
use std::path::PathBuf;

/// Server configuration, populated from CLI arguments.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Config {
    /// Directory from which files are served and to which uploads are written.
    pub serve_dir: PathBuf,
    /// TCP port to listen on.
    pub port: u16,
}
