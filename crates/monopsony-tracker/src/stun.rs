//! STUN-based NAT detection integration.
//!
//! Mirrors self-report their NAT type via the announce endpoint.
//! The tracker stores the NAT type and factors it into discovery scoring.
//!
//! NAT types (per RFC 3489):
//! - Public: directly reachable, no NAT
//! - FullCone: NAT with open mapping, reachable from any source
//! - RestrictedCone: NAT reachable only from IPs the mirror has sent to
//! - PortRestrictedCone: like RestrictedCone but port-specific
//! - Symmetric: different mapping per destination, hardest to traverse

use rusqlite::params;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NatType {
    Public,
    FullCone,
    RestrictedCone,
    PortRestrictedCone,
    Symmetric,
    Unknown,
}

impl std::fmt::Display for NatType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Public => write!(f, "public"),
            Self::FullCone => write!(f, "full_cone"),
            Self::RestrictedCone => write!(f, "restricted_cone"),
            Self::PortRestrictedCone => write!(f, "port_restricted_cone"),
            Self::Symmetric => write!(f, "symmetric"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

impl NatType {
    /// Parse from string representation.
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "public" => Self::Public,
            "full_cone" => Self::FullCone,
            "restricted_cone" => Self::RestrictedCone,
            "port_restricted_cone" => Self::PortRestrictedCone,
            "symmetric" => Self::Symmetric,
            _ => Self::Unknown,
        }
    }

    /// Connectivity penalty for discovery scoring.
    /// Public/FullCone = no penalty, Symmetric = worst.
    pub fn score_penalty(&self) -> f64 {
        match self {
            Self::Public => 0.0,
            Self::FullCone => 0.0,
            Self::RestrictedCone => 5.0,
            Self::PortRestrictedCone => 10.0,
            Self::Symmetric => 20.0,
            Self::Unknown => 8.0,
        }
    }

    /// Whether this NAT type allows direct peer connectivity.
    pub fn is_directly_reachable(&self) -> bool {
        matches!(self, Self::Public | Self::FullCone)
    }
}

/// STUN binding response (simplified for self-report).
#[derive(Debug, Serialize, Deserialize)]
pub struct StunReport {
    pub mirror_id: String,
    pub nat_type: NatType,
    pub external_ip: Option<String>,
    pub external_port: Option<u16>,
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Add NAT columns to mirrors table (called during migration).
pub fn migrate_nat_columns(conn: &rusqlite::Connection) {
    let _ = conn.execute_batch(
        "ALTER TABLE mirrors ADD COLUMN nat_type TEXT DEFAULT 'unknown';
         ALTER TABLE mirrors ADD COLUMN external_ip TEXT;
         ALTER TABLE mirrors ADD COLUMN external_port INTEGER;",
    );
}

// ---------------------------------------------------------------------------
// Integration with scoring
// ---------------------------------------------------------------------------

/// Apply NAT penalty to a mirror's base score.
pub fn apply_nat_penalty(base_score: f64, nat_type_str: Option<&str>) -> f64 {
    let nat_type = nat_type_str
        .map(NatType::from_str_lossy)
        .unwrap_or(NatType::Unknown);

    (base_score - nat_type.score_penalty()).max(0.0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nat_type_display_roundtrip() {
        let types = [
            NatType::Public,
            NatType::FullCone,
            NatType::RestrictedCone,
            NatType::PortRestrictedCone,
            NatType::Symmetric,
            NatType::Unknown,
        ];
        for t in &types {
            assert_eq!(NatType::from_str_lossy(&t.to_string()), *t);
        }
    }

    #[test]
    fn test_nat_penalty_ordering() {
        assert_eq!(NatType::Public.score_penalty(), 0.0);
        assert_eq!(NatType::FullCone.score_penalty(), 0.0);
        assert!(NatType::RestrictedCone.score_penalty() < NatType::PortRestrictedCone.score_penalty());
        assert!(NatType::PortRestrictedCone.score_penalty() < NatType::Symmetric.score_penalty());
        assert!(NatType::Unknown.score_penalty() > 0.0);
    }

    #[test]
    fn test_apply_nat_penalty() {
        assert_eq!(apply_nat_penalty(50.0, Some("public")), 50.0);
        assert_eq!(apply_nat_penalty(50.0, Some("symmetric")), 30.0);
        assert_eq!(apply_nat_penalty(5.0, Some("symmetric")), 0.0); // clamped
        assert!(apply_nat_penalty(50.0, None) < 50.0); // Unknown has penalty
    }

    #[test]
    fn test_directly_reachable() {
        assert!(NatType::Public.is_directly_reachable());
        assert!(NatType::FullCone.is_directly_reachable());
        assert!(!NatType::Symmetric.is_directly_reachable());
        assert!(!NatType::Unknown.is_directly_reachable());
    }

    #[test]
    fn test_nat_from_str_lossy() {
        assert_eq!(NatType::from_str_lossy("garbage"), NatType::Unknown);
        assert_eq!(NatType::from_str_lossy(""), NatType::Unknown);
    }
}
