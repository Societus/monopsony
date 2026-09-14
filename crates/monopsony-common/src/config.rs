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
    #[serde(default)]
    pub scan: ScanConfig,
    #[serde(default)]
    pub node: NodeConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub access_control: AccessControlConfig,
    /// HTTP CONNECT pass-through settings ([proxy] section). All fields
    /// default so existing TOMLs without the section still parse.
    #[serde(default)]
    pub proxy: ProxyServerConfig,
}

/// Server-side HTTP CONNECT pass-through configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServerConfig {
    /// Enable HTTP CONNECT tunneling through the server proxy. Default: false.
    #[serde(default)]
    pub connect_enabled: bool,
    /// Permit CONNECT to hosts resolving to private/loopback IPs (SSRF guard
    /// bypass, e.g. LAN-internal mirrors). Default: false.
    #[serde(default)]
    pub allow_private_upstreams: bool,
    /// Optional host reject-list for CONNECT destinations. Entries match
    /// case-insensitively after trimming a trailing dot, at a label boundary
    /// (`evil.com` denies `evil.com`, `evil.com.`, and `api.evil.com`).
    #[serde(default)]
    pub connect_deny: Option<Vec<String>>,
    /// Timeout in seconds for dialing the CONNECT upstream. Default: 10.
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Maximum total lifetime in seconds for a CONNECT tunnel before the
    /// relay is torn down. Default: 3600 (1 hour).
    #[serde(default = "default_tunnel_max_lifetime_secs")]
    pub tunnel_max_lifetime_secs: u64,
}

fn default_connect_timeout_secs() -> u64 {
    10
}

fn default_tunnel_max_lifetime_secs() -> u64 {
    3600
}

impl Default for ProxyServerConfig {
    fn default() -> Self {
        Self {
            connect_enabled: false,
            allow_private_upstreams: false,
            connect_deny: None,
            connect_timeout_secs: default_connect_timeout_secs(),
            tunnel_max_lifetime_secs: default_tunnel_max_lifetime_secs(),
        }
    }
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
    /// Optional WebSocket tunnel URL for clients behind Cloudflare Tunnels
    /// or reverse proxies. When set, the server includes this URL in
    /// PairResponse so paired clients know to connect via WSS instead of
    /// direct TCP. Example: "wss://monopsony.example.com/ws/tunnel"
    #[serde(default)]
    pub ws_url: Option<String>,
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
pub struct TelemetryConfig {
    /// Enable install success tracking via inventory reports.
    /// When false, server ignores installed_at fields.
    #[serde(default = "default_true")]
    pub install_tracking: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            install_tracking: true,
        }
    }
}

fn default_true() -> bool {
    true
}

// ============================================================================
// Access Control Configuration
// ============================================================================

fn default_allowed_cidrs() -> Vec<String> {
    vec![
        "10.0.0.0/8".to_string(),
        "172.16.0.0/12".to_string(),
        "192.168.0.0/16".to_string(),
        "100.64.0.0/10".to_string(),
    ]
}

/// IP-based access control for client connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessControlConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_allowed_cidrs")]
    pub allowed_cidrs: Vec<String>,
    #[serde(default = "default_true")]
    pub restrict_pairing: bool,
    #[serde(default = "default_true")]
    pub log_rejected: bool,
}

impl Default for AccessControlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_cidrs: default_allowed_cidrs(),
            restrict_pairing: true,
            log_rejected: true,
        }
    }
}

/// Check if an IP is allowed by the access control config.
pub fn is_ip_allowed(ip: std::net::IpAddr, config: &AccessControlConfig) -> bool {
    if !config.enabled {
        return true;
    }
    config.allowed_cidrs.iter().any(|cidr| ip_in_cidr(ip, cidr))
}

/// CIDR matching for IPv4 and IPv6.
pub fn ip_in_cidr(ip: std::net::IpAddr, cidr: &str) -> bool {
    let (network, prefix_len) = match cidr.split_once('/') {
        Some((net, prefix)) => (net, prefix.parse::<u32>().ok()),
        None => (cidr, None),
    };
    if prefix_len.is_none() {
        return network
            .parse::<std::net::IpAddr>()
            .map_or(false, |addr| addr == ip);
    }
    let prefix_len = prefix_len.unwrap();
    match ip {
        std::net::IpAddr::V4(addr) => {
            let net: std::net::Ipv4Addr = match network.parse() {
                Ok(n) => n,
                Err(_) => return false,
            };
            if prefix_len > 32 {
                return false;
            }
            let mask = if prefix_len == 0 {
                0u32
            } else {
                !0u32 << (32 - prefix_len)
            };
            (u32::from(addr) & mask) == (u32::from(net) & mask)
        }
        std::net::IpAddr::V6(addr) => {
            let net: std::net::Ipv6Addr = match network.parse() {
                Ok(n) => n,
                Err(_) => return false,
            };
            if prefix_len > 128 {
                return false;
            }
            let ab = addr.octets();
            let nb = net.octets();
            let full = (prefix_len / 8) as usize;
            let rem = prefix_len % 8;
            if ab[..full] != nb[..full] {
                return false;
            }
            if rem > 0 && full < 16 {
                let m = !0u8 << (8 - rem);
                if (ab[full] & m) != (nb[full] & m) {
                    return false;
                }
            }
            true
        }
    }
}

/// Node deployment tier — controls storage and scanning requirements.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum NodeTier {
    /// Homelab, air-gap, testing. No restrictions.
    #[default]
    Private,
    /// Internet-facing, serves other clients. Requires S3 + scan.
    Public,
    /// Public + bidirectional federation. Requires S3 + scan + peer verification.
    Federated,
}

impl std::fmt::Display for NodeTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeTier::Private => write!(f, "private"),
            NodeTier::Public => write!(f, "public"),
            NodeTier::Federated => write!(f, "federated"),
        }
    }
}

/// Node identity and tier configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Deployment tier: "private" (default), "public", or "federated"
    #[serde(default)]
    pub tier: NodeTier,
    /// Unique node name (for federation identification)
    #[serde(default)]
    pub name: Option<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            tier: NodeTier::Private,
            name: None,
        }
    }
}

/// Cache backend type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CacheBackend {
    #[default]
    Filesystem,
    S3,
}

impl std::fmt::Display for CacheBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheBackend::Filesystem => write!(f, "filesystem"),
            CacheBackend::S3 => write!(f, "s3"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Storage backend: "filesystem" (default) or "s3"
    #[serde(default)]
    pub backend: CacheBackend,
    /// Path to cache directory (filesystem backend)
    pub path: PathBuf,
    /// Maximum cache size in GB
    pub max_size_gb: u64,
    /// Days before purging unreferenced packages
    #[serde(default = "default_purge_days")]
    pub purge_after_days: u32,
    /// S3 backend configuration
    #[serde(default)]
    pub s3: Option<S3Config>,
    /// Streaming proxy limits (large package handling)
    #[serde(default)]
    pub proxy_limits: ProxyLimitsConfig,
}

/// Limits for the streaming HTTP cache proxy.
///
/// Sized for bare-metal package pulls (linux-firmware ≈ 1 GB single files).
/// Bodies are streamed to disk with O(1) memory; these limits guard against
/// malicious upstreams and orphaned partial uploads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyLimitsConfig {
    /// Maximum single response body in bytes, enforced via Content-Length
    /// when present and a streamed-bytes counter otherwise.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: u64,
    /// How long an in-flight upstream fetch may stall (no bytes received)
    /// before the server gives up and returns an error. Entirely separate
    /// from client-side timeouts.
    #[serde(default = "default_upstream_stall_secs")]
    pub upstream_stall_secs: u64,
    /// Cleanup interval for orphaned partial uploads left by crashes.
    #[serde(default = "default_stale_part_sweep_secs")]
    pub stale_part_sweep_secs: u64,
    /// Age threshold for the stale-part janitor: staged files older than
    /// this many seconds are considered orphaned and deleted.
    #[serde(default = "default_stale_part_ttl_secs")]
    pub stale_part_ttl_secs: u64,
}

impl Default for ProxyLimitsConfig {
    fn default() -> Self {
        Self {
            max_response_bytes: default_max_response_bytes(),
            upstream_stall_secs: default_upstream_stall_secs(),
            stale_part_sweep_secs: default_stale_part_sweep_secs(),
            stale_part_ttl_secs: default_stale_part_ttl_secs(),
        }
    }
}

fn default_max_response_bytes() -> u64 {
    4 * 1024 * 1024 * 1024 // 4 GiB — linux-firmware et al.
}

fn default_upstream_stall_secs() -> u64 {
    120
}

fn default_stale_part_sweep_secs() -> u64 {
    3600
}

fn default_stale_part_ttl_secs() -> u64 {
    7200
}

/// S3-compatible storage backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    /// S3 endpoint URL (e.g., "http://127.0.0.1:8333" for SeaweedFS)
    pub endpoint: String,
    /// Region name
    #[serde(default = "default_s3_region")]
    pub region: String,
    /// Bucket name
    pub bucket: String,
    /// Access key ID (or set MONOPSONY_S3_ACCESS_KEY env var)
    #[serde(default)]
    pub access_key: String,
    /// Secret access key (or set MONOPSONY_S3_SECRET_KEY env var)
    #[serde(default)]
    pub secret_key: String,
    /// Path to SSE-C encryption key file (256-bit / 32 bytes)
    #[serde(default)]
    pub encryption_key_path: Option<PathBuf>,
    /// Object Lock retention period in days (0 = no retention)
    #[serde(default)]
    pub retention_days: u32,
    /// Use path-style access (true for SeaweedFS/Garage, false for AWS)
    #[serde(default = "default_true")]
    pub force_path_style: bool,
}

fn default_s3_region() -> String {
    "us-east-1".to_string()
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
    /// Additional hostnames/IPs to include in server certificate SAN.
    /// Ensures TLS handshake succeeds regardless of which address
    /// the client connects from (public DNS, private IP, mDNS name, etc.).
    /// Example: san_names = ["monopsony.example.com", "10.10.20.58"]
    #[serde(default)]
    pub san_names: Vec<String>,
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
// Scan Configuration
// ============================================================================

fn default_clamd_socket() -> String {
    "tcp://127.0.0.1:3310".to_string()
}

fn default_entropy_threshold() -> f64 {
    7.5
}

/// Malware and vulnerability scanning configuration.
///
/// When enabled, cached packages are scanned asynchronously after write
/// using ClamAV (signature-based), YARA (supply-chain rules), and
/// Shannon entropy heuristics. Results are stored in `cache_scan_results`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfig {
    /// Enable malware scanning (requires clamd for ClamAV)
    #[serde(default)]
    pub enabled: bool,

    /// ClamAV daemon address (e.g., "tcp://127.0.0.1:3310")
    #[serde(default = "default_clamd_socket")]
    pub clamd_socket: String,

    /// Directory containing YARA rule files (*.yar)
    #[serde(default)]
    pub yara_rules_dir: Option<PathBuf>,

    /// Entropy threshold for flagging (0.0-8.0, default 7.5)
    #[serde(default = "default_entropy_threshold")]
    pub entropy_threshold: f64,

    /// Block updates with detected malware unless client forces
    #[serde(default = "default_true")]
    pub block_on_malware: bool,

    /// Warn (but allow) on CVEs found in requested version
    #[serde(default = "default_true")]
    pub warn_on_cve: bool,

    /// OSV API endpoint for CVE lookups (or local mirror).
    #[serde(default = "default_osv_endpoint")]
    pub osv_endpoint: String,
}

fn default_osv_endpoint() -> String {
    "https://api.osv.dev".to_string()
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            clamd_socket: default_clamd_socket(),
            yara_rules_dir: None,
            entropy_threshold: default_entropy_threshold(),
            block_on_malware: true,
            warn_on_cve: true,
            osv_endpoint: default_osv_endpoint(),
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
    /// When false, omit install timestamps from inventory reports.
    #[serde(default = "default_true")]
    pub report_install_telemetry: bool,
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
    /// Alternate addresses to try if primary fails.
    #[serde(default)]
    pub alt_addresses: Vec<String>,
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
    /// Total deadline for the retry loop when the server proxy returns a
    /// transient failure (502/503/504, connection error, or per-attempt
    /// timeout). The package manager sees a single request that blocks
    /// up to this long. Default 5 min — sized for large bare-metal
    /// packages fetched through a cold cache.
    #[serde(default = "default_retry_deadline")]
    pub retry_deadline_ms: u64,
    /// Initial backoff between retries (ms). Doubles each attempt, capped
    /// at 30 s, ±20% jitter.
    #[serde(default = "default_retry_backoff_initial_ms")]
    pub retry_backoff_initial_ms: u64,
}

fn default_proxy_bind() -> String {
    "127.0.0.1:3143".to_string()
}

fn default_fallback_timeout() -> u64 {
    2000
}

fn default_retry_deadline() -> u64 {
    300_000
}

fn default_retry_backoff_initial_ms() -> u64 {
    2_000
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
        let content = std::fs::read_to_string(path).map_err(|e| crate::Error::Io {
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
            crate::Error::Config(format!("in {}: at {}: {}", path.display(), path_str, inner))
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
        } else if !self.database.url.starts_with("postgres://")
            && !self.database.url.starts_with("postgresql://")
        {
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
                "missing [cache].path — add something like path = \"/var/cache/monopsony\""
                    .to_string(),
            );
        }

        if self.cache.max_size_gb == 0 {
            warnings.push("[cache].max_size_gb is 0 — cache will have no size limit".to_string());
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
                "auth_mode = \"mtls\" is not yet implemented — use \"open\" or \"token\" for now"
                    .to_string(),
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

        // Node tier validation
        match self.node.tier {
            NodeTier::Public | NodeTier::Federated => {
                if self.cache.backend != CacheBackend::S3 {
                    errors.push(format!(
                        "[node] tier = \"{}\" requires S3 storage backend, but [cache] backend = \"{}\"",
                        self.node.tier, self.cache.backend
                    ));
                }
                if !self.scan.enabled {
                    errors.push(format!(
                        "[node] tier = \"{}\" requires scanning enabled — set [scan] enabled = true",
                        self.node.tier
                    ));
                }
                if let NodeTier::Federated = self.node.tier {
                    if self.node.name.is_none() {
                        warnings.push(
                            "[node] tier = \"federated\" but name is not set — set [node] name for federation identity".to_string()
                        );
                    }
                }
            }
            NodeTier::Private => {}
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
        let content = std::fs::read_to_string(path).map_err(|e| crate::Error::Io {
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
            crate::Error::Config(format!("in {}: at {}: {}", path.display(), path_str, inner))
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
                ws_url: None,
            },
            database: DatabaseConfig {
                url: "postgres://monopsony:pass@localhost/monopsony".to_string(),
                max_connections: 10,
            },
            cache: CacheConfig {
                backend: CacheBackend::Filesystem,
                path: PathBuf::from("/tmp/cache"),
                max_size_gb: 50,
                purge_after_days: 30,
                s3: None,
                proxy_limits: ProxyLimitsConfig::default(),
            },
            tls: TlsServerConfig {
                ca_path: PathBuf::from("/tmp/ca.pem"),
                ca_key_path: PathBuf::from("/tmp/ca.key"),
                server_cert_path: PathBuf::from("/tmp/server.pem"),
                server_key_path: PathBuf::from("/tmp/server.key"),
                san_names: Vec::new(),
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
            scan: ScanConfig::default(),
            node: NodeConfig::default(),
            telemetry: TelemetryConfig::default(),
            access_control: AccessControlConfig::default(),
            proxy: ProxyServerConfig::default(),
        }
    }

    #[test]
    fn test_validate_valid_config() {
        let config = valid_server_config();
        assert!(
            config.validate().is_ok(),
            "Valid config should pass validation"
        );
    }

    #[test]
    fn test_validate_empty_bind() {
        let mut config = valid_server_config();
        config.server.bind = String::new();
        let errors = config.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("[server]") && e.contains("bind")),
            "Should flag empty bind: got {:?}",
            errors
        );
    }

    #[test]
    fn test_validate_bad_bind_format() {
        let mut config = valid_server_config();
        config.server.bind = "no-port-here".to_string();
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("host:port")),
            "Should flag bad bind format: got {:?}",
            errors
        );
    }

    #[test]
    fn test_validate_bad_database_url() {
        let mut config = valid_server_config();
        config.database.url = "mysql://wrong".to_string();
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("postgres://")),
            "Should flag bad database URL: got {:?}",
            errors
        );
    }

    #[test]
    fn test_validate_mtls_not_implemented() {
        let mut config = valid_server_config();
        config.server.auth_mode = AuthMode::Mtls;
        let errors = config.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("mtls") && e.contains("not yet implemented")),
            "Should flag mtls as unimplemented: got {:?}",
            errors
        );
    }

    #[test]
    fn test_validate_zero_max_connections() {
        let mut config = valid_server_config();
        config.database.max_connections = 0;
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("max_connections")),
            "Should flag zero max_connections: got {:?}",
            errors
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

    // Node tier tests

    #[test]
    fn test_node_tier_default_is_private() {
        let config = NodeConfig::default();
        assert_eq!(config.tier, NodeTier::Private);
        assert!(config.name.is_none());
    }

    #[test]
    fn test_node_tier_display() {
        assert_eq!(format!("{}", NodeTier::Private), "private");
        assert_eq!(format!("{}", NodeTier::Public), "public");
        assert_eq!(format!("{}", NodeTier::Federated), "federated");
    }

    #[test]
    fn test_node_tier_deserialize() {
        let tier: NodeTier = toml::from_str("tier = \"public\"\n")
            .map(|v: NodeConfig| v.tier)
            .unwrap();
        assert_eq!(tier, NodeTier::Public);
    }

    #[test]
    fn test_node_tier_private_validates_clean() {
        let mut config = valid_server_config();
        config.node.tier = NodeTier::Private;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_node_tier_public_requires_s3() {
        let mut config = valid_server_config();
        config.node.tier = NodeTier::Public;
        // Default backend is filesystem
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("S3 storage backend")),
            "Should flag missing S3: got {:?}",
            errors
        );
    }

    #[test]
    fn test_node_tier_public_requires_scan_enabled() {
        let mut config = valid_server_config();
        config.node.tier = NodeTier::Public;
        config.cache.backend = CacheBackend::S3;
        config.cache.s3 = Some(S3Config {
            endpoint: "http://localhost:8333".into(),
            region: "us-east-1".into(),
            bucket: "test".into(),
            access_key: "key".into(),
            secret_key: "secret".into(),
            encryption_key_path: None,
            retention_days: 7,
            force_path_style: true,
        });
        config.scan.enabled = false;
        let errors = config.validate().unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("scanning enabled")),
            "Should flag scan disabled: got {:?}",
            errors
        );
    }

    #[test]
    fn test_node_tier_public_with_s3_and_scan_passes() {
        let mut config = valid_server_config();
        config.node.tier = NodeTier::Public;
        config.cache.backend = CacheBackend::S3;
        config.cache.s3 = Some(S3Config {
            endpoint: "http://localhost:8333".into(),
            region: "us-east-1".into(),
            bucket: "test".into(),
            access_key: "key".into(),
            secret_key: "secret".into(),
            encryption_key_path: None,
            retention_days: 7,
            force_path_style: true,
        });
        config.scan.enabled = true;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_node_tier_federated_warns_without_name() {
        let mut config = valid_server_config();
        config.node.tier = NodeTier::Federated;
        config.cache.backend = CacheBackend::S3;
        config.cache.s3 = Some(S3Config {
            endpoint: "http://localhost:8333".into(),
            region: "us-east-1".into(),
            bucket: "test".into(),
            access_key: "key".into(),
            secret_key: "secret".into(),
            encryption_key_path: None,
            retention_days: 7,
            force_path_style: true,
        });
        config.scan.enabled = true;
        // Should pass validation (warnings only, not errors)
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_node_config_default_values() {
        let cfg = NodeConfig::default();
        assert_eq!(cfg.tier, NodeTier::Private);
        assert!(cfg.name.is_none());
    }

    #[test]
    fn test_node_tier_serde_roundtrip() {
        let tiers = vec![NodeTier::Private, NodeTier::Public, NodeTier::Federated];
        for tier in tiers {
            let json = serde_json::to_string(&tier).unwrap();
            let parsed: NodeTier = serde_json::from_str(&json).unwrap();
            assert_eq!(tier, parsed);
        }
    }

    #[test]
    fn test_node_tier_json_roundtrip() {
        let tiers = vec![NodeTier::Private, NodeTier::Public, NodeTier::Federated];
        for tier in tiers {
            let json = serde_json::to_string(&tier).unwrap();
            let parsed: NodeTier = serde_json::from_str(&json).unwrap();
            assert_eq!(tier, parsed);
        }
        // Verify lowercase serialization
        assert_eq!(
            serde_json::to_string(&NodeTier::Public).unwrap(),
            "\"public\""
        );
        assert_eq!(
            serde_json::to_string(&NodeTier::Federated).unwrap(),
            "\"federated\""
        );
        // Unknown tier should fail
        assert!(serde_json::from_str::<NodeTier>("\"unknown\"").is_err());
    }

    #[test]
    fn test_node_config_toml_roundtrip() {
        let cfg = NodeConfig {
            tier: NodeTier::Public,
            name: Some("node-1".to_string()),
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        let parsed: NodeConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(cfg.tier, parsed.tier);
        assert_eq!(cfg.name, parsed.name);
    }

    #[test]
    fn test_sha256_hash_consistency() {
        use sha2::{Digest, Sha256};
        let data = b"hello monopsony";
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash1 = format!("{:x}", hasher.finalize());

        let mut hasher2 = Sha256::new();
        hasher2.update(data);
        let hash2 = format!("{:x}", hasher2.finalize());
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64); // SHA256 hex is 64 chars
    }

    #[test]
    fn test_sha256_tamper_detection() {
        use sha2::{Digest, Sha256};
        let data1 = b"original content";
        let data2 = b"tampered content";
        let mut h1 = Sha256::new();
        h1.update(data1);
        let hash1 = format!("{:x}", h1.finalize());
        let mut h2 = Sha256::new();
        h2.update(data2);
        let hash2 = format!("{:x}", h2.finalize());
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_sha256_empty_data() {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"");
        let hash = format!("{:x}", hasher.finalize());
        // Known SHA256 of empty string
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
