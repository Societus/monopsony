//! Monopsony Common Library
//!
//! Shared types, traits, and utilities for the Monopsony package caching system.

pub mod config;
pub mod error;
pub mod metrics;
pub mod models;
pub mod os_release;
pub mod package_manager;
pub mod pin_store;
pub mod proto;
pub mod shadow_diff;
pub mod shadow_template;
pub mod shadow_types;
pub mod version_ord;

pub use config::*;
pub use error::{Error, Result};
pub use models::*;
pub use package_manager::{
    PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern,
};
pub use shadow_diff::{shadow_diff, ShadowDiffClass, ShadowDiffWarning};
pub use shadow_template::{render_step, StepContext, TemplateError};
