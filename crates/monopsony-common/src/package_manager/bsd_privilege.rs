//! Shared privilege escalation helper for BSD package manager backends.
//!
//! BSD package managers typically require root to read the local package database.
//! This module provides a common function that tries doas first (common on
//! MidnightBSD/FreeBSD/OpenBSD), falls back to sudo, then runs plain if already root.

use tokio::process::Command;

/// Build a tokio Command with privilege escalation if needed.
///
/// Resolution order:
/// 1. Already root → run directly
/// 2. `doas` available → prefix with `doas`
/// 3. `sudo` available → prefix with `sudo`
/// 4. Neither available → run directly (may fail at runtime)
pub fn escalate(program: &str, args: &[&str]) -> Command {
    let is_root = std::env::var("USER").map(|u| u == "root").unwrap_or(false);

    if is_root {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    } else if which::which("doas").is_ok() {
        let mut cmd = Command::new("doas");
        cmd.arg(program).args(args);
        cmd
    } else if which::which("sudo").is_ok() {
        let mut cmd = Command::new("sudo");
        cmd.arg(program).args(args);
        cmd
    } else {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    }
}

/// Set HTTP/HTTPS proxy environment variables (common for all BSD backends
/// that use env vars instead of config files).
pub fn proxy_env_actions(proxy_addr: &str) -> Vec<crate::ProxyConfigAction> {
    vec![
        crate::ProxyConfigAction::SetEnv {
            key: "HTTP_PROXY".to_string(),
            value: proxy_addr.to_string(),
        },
        crate::ProxyConfigAction::SetEnv {
            key: "HTTPS_PROXY".to_string(),
            value: proxy_addr.to_string(),
        },
        crate::ProxyConfigAction::SetEnv {
            key: "http_proxy".to_string(),
            value: proxy_addr.to_string(),
        },
        crate::ProxyConfigAction::SetEnv {
            key: "https_proxy".to_string(),
            value: proxy_addr.to_string(),
        },
    ]
}

/// Unset HTTP/HTTPS proxy environment variables.
pub fn remove_proxy_env_actions() -> Vec<crate::ProxyConfigAction> {
    vec![
        crate::ProxyConfigAction::UnsetEnv {
            key: "HTTP_PROXY".to_string(),
        },
        crate::ProxyConfigAction::UnsetEnv {
            key: "HTTPS_PROXY".to_string(),
        },
        crate::ProxyConfigAction::UnsetEnv {
            key: "http_proxy".to_string(),
        },
        crate::ProxyConfigAction::UnsetEnv {
            key: "https_proxy".to_string(),
        },
    ]
}
