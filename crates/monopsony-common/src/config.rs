//! Configuration types for Monopsony

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// HTTP proxy authentication mode
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// No authentication (LAN trust model, default)
    #[default]
    Open,
    /// API key via X-Monopsony-Key header
    Token,
    /// Mutual TLS client certificate auth (future)
    Mtls,
}

impl std::fmt::Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthMode::Open => write!(f, "open"),
            AuthMode::Token => write!(f, "token"),
            AuthMode::Mtls => write!(f, "mtls"),
        }
    }
}

impl std::str::FromStr for AuthMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "token" => Ok(Self::Token),
            "mtls" => Ok(Self::Mtls),
            other => Err(format!(
                "invalid auth_mode '{}'. Expected: open, token, mtls",
                other
            )),
        }
    }
}

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
    #[serde(default)]
    pub shadow_trust: ShadowPackageTrustConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    /// Bind address for HTTP cache proxy
    pub bind: String,
    /// Optional hostname for certificate SAN
    pub hostname: Option<String>,
    /// Authentication mode: "open" (default), "token", "mtls"
    #[serde(default)]
    pub auth_mode: AuthMode,
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
// Shadow Package Trust Configuration
// ============================================================================

fn default_trust_policy() -> String {
    "trusted_server_only".to_string()
}

/// Trust policy for verifying federated shadow package recipes.
///
/// - `trusted_server_only` (default): only accept recipes countersigned by this operator
/// - `trusted_authors`: accept recipes signed by trusted author keys
/// - `paranoid`: require both author and operator signatures
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowPackageTrustConfig {
    /// Trust policy: "trusted_server_only", "trusted_authors", or "paranoid"
    #[serde(default = "default_trust_policy")]
    pub policy: String,

    /// Author key IDs to trust (for trusted_authors policy)
    #[serde(default)]
    pub trusted_author_keys: Vec<String>,

    /// Operator key IDs to trust (for verifying countersignatures)
    #[serde(default)]
    pub trusted_operator_keys: Vec<String>,
}

impl Default for ShadowPackageTrustConfig {
    fn default() -> Self {
        Self {
            policy: default_trust_policy(),
            trusted_author_keys: Vec::new(),
            trusted_operator_keys: Vec::new(),
        }
    }
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
    #[serde(default)]
    pub shadow_trust: ShadowPackageTrustConfig,
    #[serde(default)]
    pub auto_update: AutoUpdateConfig,
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

fn default_auto_update_schedule() -> String {
    "0 0 3 * * * *".to_string() // sec min hour dom month dow year — daily at 3 AM
}

fn default_timezone() -> String {
    "UTC".to_string()
}

fn default_auto_update_severity() -> String {
    "all".to_string()
}

/// Auto-update configuration for shadow packages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoUpdateConfig {
    /// Enable automatic updates
    #[serde(default)]
    pub enabled: bool,

    /// Cron expression for update window (e.g., "0 3 * * *" = daily at 3 AM)
    #[serde(default = "default_auto_update_schedule")]
    pub schedule: String,

    /// Timezone for schedule (e.g., "America/New_York")
    #[serde(default = "default_timezone")]
    pub timezone: String,

    /// Only auto-update packages with this severity or lower.
    /// "all" (default) | "warning" | "breaking" (never auto-update breaking)
    #[serde(default = "default_auto_update_severity")]
    pub max_severity: String,

    /// Send notification before auto-update (seconds before)
    #[serde(default)]
    pub notify_before_secs: Option<u64>,
}

impl Default for AutoUpdateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: default_auto_update_schedule(),
            timezone: default_timezone(),
            max_severity: default_auto_update_severity(),
            notify_before_secs: None,
        }
    }
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
        let config: Self = Self::from_str_with_path(&content, path)?;
        config.validate().map_err(|errors| {
            crate::Error::Config(format!(
                "Configuration validation errors in {}:\n  - {}",
                path.display(),
                errors.join("\n  - ")
            ))
        })?;
        Ok(config)
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

    /// Post-deserialization validation with human-friendly messages.
    /// Returns Ok(()) if valid, Err with list of issues otherwise.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // [server] section
        if self.server.bind.is_empty() {
            errors.push(
                "missing required field 'bind' in [server] — add something like bind = \"0.0.0.0:3128\"".to_string()
            );
        } else if !self.server.bind.contains(':') {
            errors.push(format!(
                "[server].bind \"{}\" doesn't look like host:port — expected something like \"0.0.0.0:3128\"",
                self.server.bind
            ));
        }

        // [grpc] section
        if self.grpc.bind.is_empty() {
            errors.push(
                "missing required field 'bind' in [grpc] — add something like bind = \"0.0.0.0:50051\"".to_string()
            );
        }

        // [database] section
        if self.database.url.is_empty() {
            errors.push(
                "missing [database].url — add something like url = \"postgres://monopsony:password@localhost/monopsony\"".to_string()
            );
        } else if !self.database.url.starts_with("postgres://") && !self.database.url.starts_with("postgresql://") {
            errors.push(format!(
                "[database].url should start with postgres:// — got \"{}\"",
                &self.database.url[..self.database.url.len().min(40)]
            ));
        }

        if self.database.max_connections == 0 {
            errors.push("[database].max_connections must be at least 1".to_string());
        }

        // [cache] section
        if self.cache.path.as_os_str().is_empty() {
            errors.push(
                "missing [cache].path — add something like path = \"/var/cache/monopsony\"".to_string()
            );
        }

        if self.cache.max_size_gb == 0 {
            warnings.push(
                "[cache].max_size_gb is 0 — cache will have no size limit".to_string()
            );
        }

        // [tls] section — warn if paths don't exist (installer creates them)
        for (name, p) in [
            ("ca_path", &self.tls.ca_path),
            ("ca_key_path", &self.tls.ca_key_path),
            ("server_cert_path", &self.tls.server_cert_path),
            ("server_key_path", &self.tls.server_key_path),
        ] {
            if !p.exists() {
                warnings.push(format!(
                    "[tls].{} \"{}\" does not exist — run the installer to generate certificates",
                    name,
                    p.display()
                ));
            }
        }

        // Auth mode validation
        if self.server.auth_mode == AuthMode::Mtls {
            errors.push(
                "auth_mode = \"mtls\" is not yet implemented — use \"open\" or \"token\" for now".to_string()
            );
        }

        if self.server.auth_mode == AuthMode::Token {
            warnings.push(
                "auth_mode = \"token\" requires API keys — run 'monopsony-server api-key create' to generate one".to_string()
            );
        }

        // Cross-field validation
        if self.discovery.mdns_enabled && self.server.hostname.is_none() {
            warnings.push(
                "mDNS discovery is enabled but [server].hostname is not set — auto-detected hostname will be used".to_string()
            );
        }

        // Log warnings
        for w in &warnings {
            tracing::warn!("Config warning: {}", w);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
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

    /// Helper to build a valid ServerConfig for validation tests
    fn valid_server_config() -> ServerConfig {
        ServerConfig {
            server: HttpServerConfig {
                bind: "0.0.0.0:3128".to_string(),
                hostname: Some("test-host".to_string()),
                auth_mode: AuthMode::Open,
            },
            grpc: GrpcServerConfig {
                bind: "0.0.0.0:50051".to_string(),
            },
            database: DatabaseConfig {
                url: "postgres://monopsony:pass@localhost/monopsony".to_string(),
                max_connections: 10,
            },
            cache: CacheConfig {
                path: PathBuf::from("/tmp/cache"),
                max_size_gb: 50,
                purge_after_days: 30,
            },
            tls: TlsServerConfig {
                ca_path: PathBuf::from("/tmp/ca.pem"),
                ca_key_path: PathBuf::from("/tmp/ca.key"),
                server_cert_path: PathBuf::from("/tmp/server.pem"),
                server_key_path: PathBuf::from("/tmp/server.key"),
            },
            discovery: DiscoveryConfig {
                mdns_enabled: true,
                mdns_service_name: "monopsony-server".to_string(),
            },
            metrics: MetricsConfig {
                enabled: true,
                bind: "0.0.0.0:9090".to_string(),
                path: "/metrics".to_string(),
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                format: "json".to_string(),
            },
            shadow_trust: ShadowPackageTrustConfig::default(),
        }
    }

    #[test]
    fn test_validate_valid_config() {
        let config = valid_server_config();
        assert!(config.validate().is_ok(), "Valid config should pass validation");
    }

    #[test]
    fn test_validate_empty_bind() {
        let mut config = valid_server_config();
        config.server.bind = String::new();
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("[server]") && e.contains("bind")),
            "Should flag empty bind: got {:?}", errors
        );
    }

    #[test]
    fn test_validate_bad_bind_format() {
        let mut config = valid_server_config();
        config.server.bind = "no-port-here".to_string();
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("host:port")),
            "Should flag bad bind format: got {:?}", errors
        );
    }

    #[test]
    fn test_validate_bad_database_url() {
        let mut config = valid_server_config();
        config.database.url = "mysql://wrong".to_string();
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("postgres://")),
            "Should flag bad database URL: got {:?}", errors
        );
    }

    #[test]
    fn test_validate_mtls_not_implemented() {
        let mut config = valid_server_config();
        config.server.auth_mode = AuthMode::Mtls;
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("mtls") && e.contains("not yet implemented")),
            "Should flag mtls as unimplemented: got {:?}", errors
        );
    }

    #[test]
    fn test_validate_zero_max_connections() {
        let mut config = valid_server_config();
        config.database.max_connections = 0;
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("max_connections")),
            "Should flag zero max_connections: got {:?}", errors
        );
    }

    #[test]
    fn test_shadow_trust_config_defaults() {
        let trust = ShadowPackageTrustConfig::default();
        assert_eq!(trust.policy, "trusted_server_only");
        assert!(trust.trusted_author_keys.is_empty());
        assert!(trust.trusted_operator_keys.is_empty());
    }

    #[test]
    fn test_shadow_trust_config_in_server_config() {
        let config = valid_server_config();
        assert_eq!(config.shadow_trust.policy, "trusted_server_only");
    }

    #[test]
    fn test_shadow_trust_config_deserialize() {
        let toml = r#"
policy = "paranoid"
trusted_author_keys = ["key1", "key2"]
trusted_operator_keys = ["op1"]
"#;
        let trust: ShadowPackageTrustConfig = toml::from_str(toml).unwrap();
        assert_eq!(trust.policy, "paranoid");
        assert_eq!(trust.trusted_author_keys, vec!["key1", "key2"]);
        assert_eq!(trust.trusted_operator_keys, vec!["op1"]);
    }
}
