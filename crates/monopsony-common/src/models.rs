//! Data models for Monopsony

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a client
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientId(pub Uuid);

impl ClientId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(s: &str) -> crate::Result<Self> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|e| crate::Error::Parse(format!("Invalid client ID: {}", e)))
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a package
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackageId(pub Uuid);

impl PackageId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(s: &str) -> crate::Result<Self> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|e| crate::Error::Parse(format!("Invalid package ID: {}", e)))
    }
}

impl Default for PackageId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for an API key
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApiKeyId(pub Uuid);

impl Default for ApiKeyId {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiKeyId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for ApiKeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A registered client in the system
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    pub id: ClientId,
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub package_managers: Vec<String>,
    pub last_seen: DateTime<Utc>,
    pub registered_at: DateTime<Utc>,
    pub cert_serial: Option<String>,
}

/// A cached package
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub id: PackageId,
    pub manager: String,
    pub name: String,
    pub version: String,
    pub arch: Option<String>,
    pub file_path: String,
    pub hash: Option<String>,
    pub upstream_url: String,
    pub size_bytes: i64,
    pub cached_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
}

/// Association between a client and a package
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientPackage {
    pub client_id: ClientId,
    pub package_id: PackageId,
    pub installed_at: DateTime<Utc>,
}

/// An API key for authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: ApiKeyId,
    pub label: String,
    pub key_hash: String,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
}

/// A package scheduled for purge
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeEntry {
    pub package_id: PackageId,
    pub scheduled_at: DateTime<Utc>,
    pub purge_after: DateTime<Utc>,
}

/// Operating system information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsInfo {
    pub os_type: OsType,
    pub version: String,
    pub hostname: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsType {
    Linux,
    Windows,
    Macos,
}

impl std::fmt::Display for OsType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OsType::Linux => write!(f, "linux"),
            OsType::Windows => write!(f, "windows"),
            OsType::Macos => write!(f, "macos"),
        }
    }
}

/// Server status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub version: String,
    pub uptime_seconds: u64,
    pub clients_connected: u64,
    pub packages_cached: u64,
    pub cache_size_bytes: u64,
    pub cache_max_bytes: u64,
}

/// Discovery information broadcast via mDNS
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryInfo {
    pub address: String,
    pub http_port: u16,
    pub grpc_port: u16,
    pub version: String,
}

impl DiscoveryInfo {
    pub fn http_endpoint(&self) -> String {
        format!("{}:{}", self.address, self.http_port)
    }

    pub fn grpc_endpoint(&self) -> String {
        format!("{}:{}", self.address, self.grpc_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_id_display() {
        let id = ClientId::new();
        let display = format!("{}", id);
        assert!(Uuid::parse_str(&display).is_ok());
    }

    #[test]
    fn test_client_id_parse() {
        let id = ClientId::new();
        let display = format!("{}", id);
        let parsed = ClientId::parse(&display).unwrap();
        assert_eq!(id.0, parsed.0);
    }

    /// Regression test: ClientId::parse must reject strings with a "client-"
    /// prefix. This was the root cause of the pairing DB update failure —
    /// the server generated "client-{uuid}" but parse() only accepts bare UUIDs.
    #[test]
    fn test_client_id_parse_rejects_prefix() {
        let prefixed = format!("client-{}", uuid::Uuid::new_v4().simple());
        assert!(ClientId::parse(&prefixed).is_err());
    }
}
