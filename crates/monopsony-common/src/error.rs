//! Error types for Monopsony

use std::path::PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("IO error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Database error: {0}")]
    Database(String),

    #[error("Package manager error for {manager}: {message}")]
    PackageManager { manager: String, message: String },

    #[error("Cache error: {0}")]
    Cache(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("Certificate error: {0}")]
    Certificate(String),

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Discovery error: {0}")]
    Discovery(String),

    #[error("gRPC error: {0}")]
    Grpc(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Package not found: {0}")]
    PackageNotFound(String),

    #[error("Client not found: {0}")]
    ClientNotFound(String),

    #[error("Upstream fetch failed for {url}: {status}")]
    UpstreamFetch { url: String, status: u16 },

    #[error("Hash mismatch for {path}: expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("Command failed: {command}")]
    CommandFailed {
        command: String,
        exit_code: Option<i32>,
    },

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("TOML parsing error: {0}")]
    Toml(String),
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io {
            path: PathBuf::new(),
            source: err,
        }
    }
}

impl From<toml::de::Error> for Error {
    fn from(err: toml::de::Error) -> Self {
        Error::Toml(err.to_string())
    }
}
