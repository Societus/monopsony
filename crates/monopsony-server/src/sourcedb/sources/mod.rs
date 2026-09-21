//! Upstream resolution source adapters (Sprint 21 slice 21-3).
//! Shared response types for the sourcedb resolution pipeline.
//! Leaves own their individual source module files; this mod.rs is
//! parent-created contract glue — do not edit from leaf briefs.

/// A resolved upstream mapping for one package_key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedMapping {
    /// Observation source id, e.g. "aur", "arch:extra", "github:owner/repo".
    pub source: String,
    /// Upstream project identifier when known (AUR pkgbase, GitHub "owner/repo").
    pub origin: Option<String>,
    /// Project homepage URL when known.
    pub homepage: Option<String>,
}

pub mod arch;
pub mod aur;
pub mod github;
