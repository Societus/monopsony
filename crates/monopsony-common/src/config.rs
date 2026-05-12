//! Configuration types for Monopsony

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: HttpServerConfig,
    pub grpc: GrpcServerConfig,
    pub database: DatabaseConfig,
    pub cache: CacheConfig,
    pub tls: TlsServerConfig,
    pub discovery: DiscoveryConfig,
    pub metrics: MetricsConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    /// Bind address for HTTP cache proxy
    pub bind: String,
    /// Optional hostname for certificate SAN
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcServerConfig {
    /// Bind address for gRPC control plane
    pub bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL
    pub url: String,
    /// Maximum connections in pool
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Path to cache directory
    pub path: PathBuf,
    /// Maximum cache size in GB
    pub max_size_gb: u64,
    /// Days before purging unreferenced packages
    #[serde(default = "default_purge_days")]
    pub purge_after_days: u32,
}

fn default_purge_days() -> u32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsServerConfig {
    /// Path to CA certificate
    pub ca_path: PathBuf,
    /// Path to CA private key
    pub ca_key_path: PathBuf,
    /// Path to server certificate
    pub server_cert_path: PathBuf,
    /// Path to server private key
    pub server_key_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    /// Enable mDNS discovery
    #[serde(default = "default_true")]
    pub mdns_enabled: bool,
    /// mDNS service name
    #[serde(default = "default_mdns_service_name")]
    pub mdns_service_name: String,
}

fn default_true() -> bool {
    true
}

fn default_mdns_service_name() -> String {
    "monopsony-server".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// Enable Prometheus metrics
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Bind address for metrics endpoint
    #[serde(default = "default_metrics_bind")]
    pub bind: String,
    /// Path for metrics endpoint
    #[serde(default = "default_metrics_path")]
    pub path: String,
}

fn default_metrics_bind() -> String {
    "0.0.0.0:9090".to_string()
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: trace, debug, info, warn, error
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Log format: json or pretty
    #[serde(default = "default_log_format")]
    pub format: String,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_format() -> String {
    "json".to_string()
}

// ============================================================================
// Client Configuration
// ============================================================================

/// Client configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub client: ClientIdentityConfig,
    pub server: ServerConnectionConfig,
    pub tls: TlsClientConfig,
    pub proxy: ProxyConfig,
    pub metrics: ClientMetricsConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientIdentityConfig {
    /// Client ID (set on first registration)
    #[serde(default)]
    pub id: String,
    /// Client hostname (auto-detected if not set)
    #[serde(default)]
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConnectionConfig {
    /// Server address (optional, uses mDNS if empty)
    #[serde(default)]
    pub address: Option<String>,
    /// gRPC port
    #[serde(default = "default_grpc_port")]
    pub grpc_port: u16,
    /// HTTP cache port
    #[serde(default = "default_http_port")]
    pub http_port: u16,
}

fn default_grpc_port() -> u16 {
    50051
}

fn default_http_port() -> u16 {
    3142
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsClientConfig {
    /// Path to CA certificate
    pub ca_path: PathBuf,
    /// Path to client certificate
    pub client_cert_path: PathBuf,
    /// Path to client private key
    pub client_key_path: PathBuf,
    /// Trust on first use mode
    #[serde(default)]
    pub trust_on_first_use: bool,
    /// Expected CA fingerprint for TOFU
    #[serde(default)]
    pub expected_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Bind address for local HTTP proxy
    #[serde(default = "default_proxy_bind")]
    pub bind: String,
    /// Timeout before falling back to upstream (ms)
    #[serde(default = "default_fallback_timeout")]
    pub fallback_timeout_ms: u64,
}

fn default_proxy_bind() -> String {
    "127.0.0.1:3143".to_string()
}

fn default_fallback_timeout() -> u64 {
    2000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMetricsConfig {
    /// Enable Prometheus metrics
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Bind address for metrics endpoint
    #[serde(default = "default_client_metrics_bind")]
    pub bind: String,
}

fn default_client_metrics_bind() -> String {
    "127.0.0.1:9091".to_string()
}

// ============================================================================
// Configuration Loading
// ============================================================================

impl ServerConfig {
    pub fn from_file(path: &std::path::Path) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
        Self::from_str_with_path(&content, path)
    }

    /// Parse config from a TOML string, tracking the path to any deserialization error.
    fn from_str_with_path(content: &str, path: &std::path::Path) -> crate::Result<Self> {
        let de = toml::de::Deserializer::new(content);
        serde_path_to_error::deserialize(de).map_err(|e| {
            let path_str = e.path().to_string();
            let inner = e.into_inner();
            crate::Error::Config(format!(
                "in {}: at {}: {}",
                path.display(),
                path_str,
                inner
            ))
        })
    }

    pub fn to_toml(&self) -> crate::Result<String> {
        toml::to_string_pretty(self).map_err(|e| crate::Error::Parse(e.to_string()))
    }
}

impl ClientConfig {
    pub fn from_file(path: &std::path::Path) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
        Self::from_str_with_path(&content, path)
    }

    /// Parse config from a TOML string, tracking the path to any deserialization error.
    fn from_str_with_path(content: &str, path: &std::path::Path) -> crate::Result<Self> {
        let de = toml::de::Deserializer::new(content);
        serde_path_to_error::deserialize(de).map_err(|e| {
            let path_str = e.path().to_string();
            let inner = e.into_inner();
            crate::Error::Config(format!(
                "in {}: at {}: {}",
                path.display(),
                path_str,
                inner
            ))
        })
    }

    pub fn to_toml(&self) -> crate::Result<String> {
        toml::to_string_pretty(self).map_err(|e| crate::Error::Parse(e.to_string()))
    }

    pub fn save(&self, path: &std::path::Path) -> crate::Result<()> {
        let content = self.to_toml()?;
        std::fs::write(path, content).map_err(|e| crate::Error::Io {
            path: path.to_path_buf(),
            source: e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_config_error_includes_path_to_missing_field() {
        let dir = std::env::temp_dir().join("monopsony-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("bad_server.toml");

        let mut f = std::fs::File::create(&file_path).unwrap();
        // Only provide [server] section, missing grpc, database, cache, tls, etc.
        writeln!(f, "[server]").unwrap();
        writeln!(f, "bind = \"0.0.0.0:3128\"").unwrap();

        let result = ServerConfig::from_file(&file_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();

        assert!(
            err_msg.contains("bad_server.toml"),
            "Error should mention file path: got {:?}",
            err_msg
        );
        assert!(
            err_msg.contains("grpc"),
            "Error should mention the missing section: got {:?}",
            err_msg
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_config_error_includes_path_to_wrong_type() {
        let dir = std::env::temp_dir().join("monopsony-config-test-type");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("wrong_type.toml");

        let mut f = std::fs::File::create(&file_path).unwrap();
        // database.max_connections should be u32, not a string
        writeln!(f, "[server]").unwrap();
        writeln!(f, "bind = \"0.0.0.0:3128\"").unwrap();
        writeln!(f, "hostname = \"test\"").unwrap();
        writeln!(f, "[grpc]").unwrap();
        writeln!(f, "bind = \"0.0.0.0:50051\"").unwrap();
        writeln!(f, "[database]").unwrap();
        writeln!(f, "url = \"postgres://test\"").unwrap();
        writeln!(f, "max_connections = \"not_a_number\"").unwrap();
        writeln!(f, "[cache]").unwrap();
        writeln!(f, "path = \"/tmp/cache\"").unwrap();
        writeln!(f, "max_size_gb = 10").unwrap();
        writeln!(f, "[tls]").unwrap();
        writeln!(f, "ca_path = \"/tmp/ca.pem\"").unwrap();
        writeln!(f, "ca_key_path = \"/tmp/ca.key\"").unwrap();
        writeln!(f, "server_cert_path = \"/tmp/server.pem\"").unwrap();
        writeln!(f, "server_key_path = \"/tmp/server.key\"").unwrap();

        let result = ServerConfig::from_file(&file_path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();

        assert!(
            err_msg.contains("wrong_type.toml"),
            "Error should mention file path: got {:?}",
            err_msg
        );
        assert!(
            err_msg.contains("database.max_connections"),
            "Error should include field path: got {:?}",
            err_msg
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}