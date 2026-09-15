//! Monopsony Common Library
//!
//! Shared types, traits, and utilities for the Monopsony package caching system.

pub mod config;
pub mod error;
pub mod metrics;
pub mod models;
pub mod package_manager;
pub mod pin_store;
pub mod proto;
pub mod shadow_types;

pub use config::*;
pub use error::{Error, Result};
pub use models::*;
pub use package_manager::{
    PackageManagerBackend, PackageManagerKind, PackageRef, ProxyConfigAction, UrlPattern,
};
