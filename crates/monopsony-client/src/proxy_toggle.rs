//! Proxy toggle module for Monopsony client.
//!
//! Provides `proxy status`, `proxy on`, and `proxy off` functionality for
//! package managers, Docker, and shadow packages. Uses the
//! [`PackageManagerBackend`] trait's `proxy_config` / `remove_proxy_config`
//! actions to apply configuration changes.
//!
//! All functions return [`ProxyToggleResult`] vectors so the caller can
//! render a uniform summary table regardless of subsystem.

use monopsony_common::package_manager::{
    get_available_backends, get_backend, PackageManagerBackend, PackageManagerKind,
    ProxyConfigAction,
};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which subsystem(s) a toggle operation should target.
#[derive(Debug, Clone)]
pub enum ToggleTarget {
    /// All detected package managers plus Docker.
    All,
    /// A single package manager by name (pacman, apt, dnf, apk, winget, choco, scoop, …).
    Pm(String),
    /// Docker daemon registry-mirror configuration.
    Docker,
    /// Shadow package trust enforcement.
    Shadow,
}

/// Outcome of a single toggle action for a single subsystem.
#[derive(Debug, Clone)]
pub struct ProxyToggleResult {
    /// Human-readable subsystem name, e.g. `"pacman"`, `"docker"`, `"shadow"`.
    pub subsystem: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// Human-readable detail message.
    pub message: String,
}

// ---------------------------------------------------------------------------
// Privilege detection
// ---------------------------------------------------------------------------

/// Check whether the current process is running with root/elevated privileges.
///
/// On Unix this runs `id -u` and checks for UID 0. On non-Unix targets it
/// always returns `true` (Windows privilege model differs; UAC handled elsewhere).
#[cfg(unix)]
fn is_root() -> bool {
    match std::process::Command::new("id").arg("-u").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim() == "0",
        Err(_) => {
            // Fallback: check $USER
            std::env::var("USER")
                .map(|u| u == "root")
                .unwrap_or(false)
        }
    }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    // Windows: no direct root concept. Administrator status is checked via UAC
    // at the shell level. Return true so we don't block operations unnecessarily.
    true
}

/// Build a "not root" error result.
fn not_root_result(subsystem: &str) -> ProxyToggleResult {
    ProxyToggleResult {
        subsystem: subsystem.to_string(),
        success: false,
        message: "This action requires root. Run with: sudo monopsony-client proxy on --all"
            .to_string(),
    }
}

// ---------------------------------------------------------------------------
// Docker daemon.json helpers
// ---------------------------------------------------------------------------

/// Default Docker daemon config path on Unix. On Windows Docker Desktop manages
/// this differently, but the file is still commonly found at this path inside
/// the Docker VM / WSL2 backend.
fn docker_daemon_json_path() -> PathBuf {
    if cfg!(target_os = "windows") {
        PathBuf::from(r"C:\ProgramData\docker\config\daemon.json")
    } else {
        PathBuf::from("/etc/docker/daemon.json")
    }
}

/// Read the existing daemon.json, or return an empty JSON object if it doesn't
/// exist or can't be parsed.
fn read_daemon_json() -> serde_json::Value {
    let path = docker_daemon_json_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return serde_json::json!({});
            }
            serde_json::from_str(trimmed).unwrap_or_else(|e| {
                warn!(
                    "Failed to parse existing {}: {} — starting from empty config",
                    path.display(),
                    e
                );
                serde_json::json!({})
            })
        }
        Err(_) => serde_json::json!({}),
    }
}

/// Build the registry-mirrors URL from a server address (host or host:port).
/// If the address already contains a scheme, use it as-is. Otherwise prepend
/// `http://` and use port 3128 (the Monopsony proxy port).
fn mirror_url(server_addr: &str) -> String {
    if server_addr.starts_with("http://") || server_addr.starts_with("https://") {
        server_addr.trim_end_matches('/').to_string()
    } else {
        // Strip any existing port, then add :3128
        let host = server_addr.split(':').next().unwrap_or(server_addr);
        format!("http://{}:3128", host)
    }
}

/// Write a merged daemon.json with the Monopsony mirror added to registry-mirrors.
fn docker_proxy_on(server_addr: &str) -> ProxyToggleResult {
    let mirror = mirror_url(server_addr);
    let mut daemon = read_daemon_json();
    if !daemon.is_object() {
        daemon = serde_json::json!({});
    }
    let obj = daemon.as_object_mut().unwrap();

    // Build the registry-mirrors list, preserving existing entries.
    let mut mirrors: Vec<String> = obj
        .get("registry-mirrors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if !mirrors.contains(&mirror) {
        mirrors.push(mirror.clone());
    }
    obj.insert(
        "registry-mirrors".to_string(),
        serde_json::Value::Array(
            mirrors
                .iter()
                .map(|m| serde_json::Value::String(m.clone()))
                .collect(),
        ),
    );

    let pretty = serde_json::to_string_pretty(&daemon).unwrap_or_else(|e| {
        warn!("Failed to serialize daemon.json: {}", e);
        "{}".to_string()
    });

    let path = docker_daemon_json_path();
    match write_file_with_dirs(&path, &pretty) {
        Ok(_) => {
            info!("Docker: wrote registry-mirror {} to {}", mirror, path.display());
            ProxyToggleResult {
                subsystem: "docker".to_string(),
                success: true,
                message: format!(
                    "Added registry-mirror {} to {} (restart Docker to apply)",
                    mirror,
                    path.display()
                ),
            }
        }
        Err(e) => {
            warn!("Docker: failed to write {}: {}", path.display(), e);
            ProxyToggleResult {
                subsystem: "docker".to_string(),
                success: false,
                message: format!("Failed to write {}: {}", path.display(), e),
            }
        }
    }
}

/// Remove the Monopsony mirror from daemon.json registry-mirrors.
fn docker_proxy_off() -> ProxyToggleResult {
    let path = docker_daemon_json_path();
    if !path.exists() {
        return ProxyToggleResult {
            subsystem: "docker".to_string(),
            success: true,
            message: format!("{} does not exist — nothing to remove", path.display()),
        };
    }

    let mut daemon = read_daemon_json();
    if !daemon.is_object() {
        daemon = serde_json::json!({});
    }
    let obj = daemon.as_object_mut().unwrap();

    let changed = if let Some(arr) = obj.get_mut("registry-mirrors").and_then(|v| v.as_array_mut()) {
        let before = arr.len();
        arr.retain(|v| {
            match v.as_str() {
                Some(s) => !s.contains("monopsony") && !s.contains(":3128"),
                None => true,
            }
        });
        arr.len() != before
    } else {
        false
    };

    if changed {
        let pretty = serde_json::to_string_pretty(&daemon).unwrap_or_else(|e| {
            warn!("Failed to serialize daemon.json: {}", e);
            "{}".to_string()
        });
        match write_file_with_dirs(&path, &pretty) {
            Ok(_) => {
                info!("Docker: removed registry-mirror from {}", path.display());
                ProxyToggleResult {
                    subsystem: "docker".to_string(),
                    success: true,
                    message: format!(
                        "Removed registry-mirror from {} (restart Docker to apply)",
                        path.display()
                    ),
                }
            }
            Err(e) => {
                warn!("Docker: failed to write {}: {}", path.display(), e);
                ProxyToggleResult {
                    subsystem: "docker".to_string(),
                    success: false,
                    message: format!("Failed to write {}: {}", path.display(), e),
                }
            }
        }
    } else {
        ProxyToggleResult {
            subsystem: "docker".to_string(),
            success: true,
            message: "No Monopsony mirror found in Docker daemon.json".to_string(),
        }
    }
}

/// Check if Docker daemon.json has a Monopsony or :3128 registry-mirror.
fn docker_status() -> ProxyToggleResult {
    let path = docker_daemon_json_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(daemon) => {
                    let mirrors: Vec<String> = daemon
                        .get("registry-mirrors")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();

                    let monopsony_mirrors: Vec<&String> = mirrors
                        .iter()
                        .filter(|m| m.contains("monopsony") || m.contains(":3128"))
                        .collect();

                    if monopsony_mirrors.is_empty() {
                        ProxyToggleResult {
                            subsystem: "docker".to_string(),
                            success: true,
                            message: "Docker daemon.json exists, no Monopsony mirror configured"
                                .to_string(),
                        }
                    } else {
                        ProxyToggleResult {
                            subsystem: "docker".to_string(),
                            success: true,
                            message: format!(
                                "Docker proxy active (mirrors: {})",
                                monopsony_mirrors
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        }
                    }
                }
                Err(e) => ProxyToggleResult {
                    subsystem: "docker".to_string(),
                    success: true,
                    message: format!(
                        "Docker daemon.json exists but unparseable: {}",
                        e
                    ),
                },
            }
        }
        Err(_) => ProxyToggleResult {
            subsystem: "docker".to_string(),
            success: true,
            message: format!(
                "Docker daemon.json not found at {} — mirror not configured",
                path.display()
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// File write helper
// ---------------------------------------------------------------------------

/// Write file contents, creating parent directories as needed.
fn write_file_with_dirs(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

// ---------------------------------------------------------------------------
// Package manager helpers
// ---------------------------------------------------------------------------

/// Collect all `WriteFile` paths from a `ProxyConfigAction` tree.
fn collect_write_paths(action: &ProxyConfigAction, paths: &mut Vec<PathBuf>) {
    match action {
        ProxyConfigAction::WriteFile { path, .. } => {
            paths.push(path.clone());
        }
        ProxyConfigAction::Multiple(actions) => {
            for a in actions {
                collect_write_paths(a, paths);
            }
        }
        _ => {}
    }
}

/// Collect all `RemoveFile` paths from a `ProxyConfigAction` tree.
#[allow(dead_code)]
fn collect_remove_paths(action: &ProxyConfigAction, paths: &mut Vec<PathBuf>) {
    match action {
        ProxyConfigAction::RemoveFile { path } => {
            paths.push(path.clone());
        }
        ProxyConfigAction::Multiple(actions) => {
            for a in actions {
                collect_remove_paths(a, paths);
            }
        }
        _ => {}
    }
}

/// Determine if a subsystem is "configured" based on whether proxy_config
/// WriteFile paths exist on disk.
fn pm_is_proxied(backend: &dyn PackageManagerBackend) -> bool {
    let action = backend.proxy_config("dummy");
    let mut paths = Vec::new();
    collect_write_paths(&action, &mut paths);
    if paths.is_empty() {
        // No file-based config — check env var actions.
        // For SetEnv-based backends, assume not configured (env vars are ephemeral).
        return false;
    }
    paths.iter().any(|p| p.exists())
}

/// Apply proxy on for a single package manager backend.
fn pm_proxy_on(backend: &dyn PackageManagerBackend, server_addr: &str) -> ProxyToggleResult {
    let kind = backend.kind();
    let name = kind.as_str();

    // Build the proxy address that the backend expects.
    // backends use an http:// URL form for proxy_config().
    let proxy_addr = if server_addr.starts_with("http://") || server_addr.starts_with("https://") {
        server_addr.to_string()
    } else {
        format!("http://{}", server_addr)
    };

    let action = backend.proxy_config(&proxy_addr);
    info!("PM [{}]: applying proxy on ({})", name, proxy_addr);

    let results = action.execute();
    let errors: Vec<String> = results
        .iter()
        .filter_map(|r| match r {
            Ok(desc) => {
                info!("PM [{}]: {}", name, desc);
                None
            }
            Err(e) => {
                warn!("PM [{}]: FAILED: {}", name, e);
                Some(e.to_string())
            }
        })
        .collect();

    if errors.is_empty() {
        let descs: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().ok().map(|s| s.to_string()))
            .collect();
        ProxyToggleResult {
            subsystem: name.to_string(),
            success: true,
            message: descs.join("; "),
        }
    } else {
        ProxyToggleResult {
            subsystem: name.to_string(),
            success: false,
            message: errors.join("; "),
        }
    }
}

/// Apply proxy off for a single package manager backend.
fn pm_proxy_off(backend: &dyn PackageManagerBackend) -> ProxyToggleResult {
    let kind = backend.kind();
    let name = kind.as_str();

    let action = backend.remove_proxy_config();
    info!("PM [{}]: applying proxy off", name);

    let results = action.execute();
    let errors: Vec<String> = results
        .iter()
        .filter_map(|r| match r {
            Ok(desc) => {
                info!("PM [{}]: {}", name, desc);
                None
            }
            Err(e) => {
                warn!("PM [{}]: FAILED: {}", name, e);
                Some(e.to_string())
            }
        })
        .collect();

    if errors.is_empty() {
        let descs: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().ok().map(|s| s.to_string()))
            .collect();
        ProxyToggleResult {
            subsystem: name.to_string(),
            success: true,
            message: descs.join("; "),
        }
    } else {
        ProxyToggleResult {
            subsystem: name.to_string(),
            success: false,
            message: errors.join("; "),
        }
    }
}

/// Status for a single package manager backend.
fn pm_status(backend: &dyn PackageManagerBackend) -> ProxyToggleResult {
    let kind = backend.kind();
    let name = kind.as_str();

    if pm_is_proxied(backend) {
        ProxyToggleResult {
            subsystem: name.to_string(),
            success: true,
            message: format!("{} proxy: configured", name),
        }
    } else {
        ProxyToggleResult {
            subsystem: name.to_string(),
            success: true,
            message: format!("{} proxy: not configured", name),
        }
    }
}

// ---------------------------------------------------------------------------
// Shadow trust helpers
// ---------------------------------------------------------------------------

/// Default client config path.
fn client_config_path() -> PathBuf {
    if cfg!(windows) {
        std::env::var("ProgramData")
            .map(|d| PathBuf::from(format!(r"{}\monopsony\client.toml", d)))
            .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData\monopsony\client.toml"))
    } else {
        PathBuf::from("/etc/monopsony/client.toml")
    }
}

/// Read the shadow_trust.enabled from client config.
/// Returns `false` if the config or section is missing.
fn shadow_trust_enabled() -> bool {
    let path = client_config_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let config: monopsony_common::config::ClientConfig = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to parse {}: {}", path.display(), e);
            return false;
        }
    };
    // Shadow trust is considered "enabled" if the policy is set to something
    // other than the default "disabled" sentinel, or if trusted keys exist.
    // The ShadowPackageTrustConfig always exists (serde default) so we check
    // whether it has meaningful content.
    !config.shadow_trust.policy.is_empty()
        && config.shadow_trust.policy != "disabled"
        && (!config.shadow_trust.trusted_author_keys.is_empty()
            || !config.shadow_trust.trusted_operator_keys.is_empty())
}

/// Toggle shadow trust on in the client config file.
fn shadow_proxy_on() -> ProxyToggleResult {
    let path = client_config_path();

    // Read existing config or create minimal TOML.
    let (toml_str, existed) = match std::fs::read_to_string(&path) {
        Ok(c) => (c, true),
        Err(_) => (String::new(), false),
    };

    // Parse into a dynamic toml::Value so we can modify without needing
    // the full ClientConfig struct round-trip (which requires many fields).
    let mut doc: toml::Value = if toml_str.trim().is_empty() {
        toml::Value::Table(toml::value::Table::new())
    } else {
        match toml::from_str(&toml_str) {
            Ok(v) => v,
            Err(e) => {
                return ProxyToggleResult {
                    subsystem: "shadow".to_string(),
                    success: false,
                    message: format!("Failed to parse {}: {}", path.display(), e),
                };
            }
        }
    };

    // Ensure top-level is a table.
    if !doc.is_table() {
        doc = toml::Value::Table(toml::value::Table::new());
    }
    let table = doc.as_table_mut().unwrap();

    let shadow_entry = table
        .entry("shadow_trust")
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));

    if !shadow_entry.is_table() {
        *shadow_entry = toml::Value::Table(toml::value::Table::new());
    }
    let shadow_table = shadow_entry.as_table_mut().unwrap();

    shadow_table.insert(
        "enabled".to_string(),
        toml::Value::Boolean(true),
    );

    let new_toml = toml::to_string_pretty(&doc).unwrap_or_else(|e| {
        warn!("Failed to serialize TOML: {}", e);
        String::new()
    });

    match write_file_with_dirs(&path, &new_toml) {
        Ok(_) => {
            info!(
                "Shadow: {} trust in {} (file {})",
                if existed { "updated" } else { "created" },
                path.display(),
                if existed { "existed" } else { "new" }
            );
            ProxyToggleResult {
                subsystem: "shadow".to_string(),
                success: true,
                message: format!(
                    "Shadow trust enabled in {} (was {})",
                    path.display(),
                    if existed { "updated" } else { "created" }
                ),
            }
        }
        Err(e) => {
            warn!("Shadow: failed to write {}: {}", path.display(), e);
            ProxyToggleResult {
                subsystem: "shadow".to_string(),
                success: false,
                message: format!("Failed to write {}: {}", path.display(), e),
            }
        }
    }
}

/// Toggle shadow trust off in the client config file.
fn shadow_proxy_off() -> ProxyToggleResult {
    let path = client_config_path();
    let toml_str = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return ProxyToggleResult {
                subsystem: "shadow".to_string(),
                success: true,
                message: format!("Config {} not found — shadow trust already off", path.display()),
            };
        }
    };

    let mut doc: toml::Value = match toml::from_str(&toml_str) {
        Ok(v) => v,
        Err(e) => {
            return ProxyToggleResult {
                subsystem: "shadow".to_string(),
                success: false,
                message: format!("Failed to parse {}: {}", path.display(), e),
            };
        }
    };

    let changed = if let Some(table) = doc.as_table_mut() {
        if let Some(shadow_entry) = table.get_mut("shadow_trust") {
            if let Some(shadow_table) = shadow_entry.as_table_mut() {
                let had = shadow_table
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                shadow_table.insert("enabled".to_string(), toml::Value::Boolean(false));
                had
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    if changed {
        let new_toml = toml::to_string_pretty(&doc).unwrap_or_else(|e| {
            warn!("Failed to serialize TOML: {}", e);
            String::new()
        });
        match write_file_with_dirs(&path, &new_toml) {
            Ok(_) => {
                info!("Shadow: disabled trust in {}", path.display());
                ProxyToggleResult {
                    subsystem: "shadow".to_string(),
                    success: true,
                    message: format!("Shadow trust disabled in {}", path.display()),
                }
            }
            Err(e) => {
                warn!("Shadow: failed to write {}: {}", path.display(), e);
                ProxyToggleResult {
                    subsystem: "shadow".to_string(),
                    success: false,
                    message: format!("Failed to write {}: {}", path.display(), e),
                }
            }
        }
    } else {
        ProxyToggleResult {
            subsystem: "shadow".to_string(),
            success: true,
            message: "Shadow trust was already disabled or not configured".to_string(),
        }
    }
}

fn shadow_status() -> ProxyToggleResult {
    if shadow_trust_enabled() {
        ProxyToggleResult {
            subsystem: "shadow".to_string(),
            success: true,
            message: "Shadow trust: enabled".to_string(),
        }
    } else {
        ProxyToggleResult {
            subsystem: "shadow".to_string(),
            success: true,
            message: "Shadow trust: disabled".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check the proxy status of each subsystem.
///
/// For each detected package manager: checks if its proxy config files exist
/// by running `proxy_config("dummy")` and seeing if WriteFile paths are present.
/// Docker: checks daemon.json for registry-mirrors with "monopsony" or ":3128".
/// Shadow: checks `[shadow_trust]` in client config.
pub async fn proxy_status(_cert_dir: &Path) -> Vec<ProxyToggleResult> {
    let mut results = Vec::new();

    // Package managers
    let backends = get_available_backends();
    for backend in &backends {
        results.push(pm_status(backend.as_ref()));
    }

    // Docker
    results.push(docker_status());

    // Shadow
    results.push(shadow_status());

    results
}

/// Enable proxying for the specified target.
pub async fn proxy_on(target: ToggleTarget, server_addr: &str) -> Vec<ProxyToggleResult> {
    let mut results = Vec::new();
    let rooted = is_root();

    match target {
        ToggleTarget::All => {
            // Package managers
            let backends = get_available_backends();
            if backends.is_empty() {
                results.push(ProxyToggleResult {
                    subsystem: "package-managers".to_string(),
                    success: true,
                    message: "No supported package managers detected".to_string(),
                });
            }
            for backend in &backends {
                let name = backend.kind().as_str();
                if !rooted {
                    results.push(not_root_result(name));
                } else {
                    results.push(pm_proxy_on(backend.as_ref(), server_addr));
                }
            }

            // Docker
            if !rooted {
                results.push(not_root_result("docker"));
            } else {
                results.push(docker_proxy_on(server_addr));
            }
        }

        ToggleTarget::Pm(pm_name) => {
            let kind = match PackageManagerKind::parse_kind(&pm_name.to_lowercase()) {
                Some(k) => k,
                None => {
                    results.push(ProxyToggleResult {
                        subsystem: pm_name.clone(),
                        success: false,
                        message: format!(
                            "Unknown package manager '{}'. Valid: pacman, apt, dnf, apk, winget, choco, scoop, pkg, pkg_add, pkgin, mport",
                            pm_name
                        ),
                    });
                    return results;
                }
            };

            // Get the specific backend.
            let backend = match get_backend(kind) {
                Some(b) => b,
                None => {
                    results.push(ProxyToggleResult {
                        subsystem: pm_name.clone(),
                        success: false,
                        message: format!("Backend for '{}' not available", pm_name),
                    });
                    return results;
                }
            };

            if !backend.is_available() {
                results.push(ProxyToggleResult {
                    subsystem: pm_name.clone(),
                    success: false,
                    message: format!("Package manager '{}' is not installed on this system", pm_name),
                });
                return results;
            }

            if !rooted {
                results.push(not_root_result(&pm_name));
            } else {
                results.push(pm_proxy_on(backend.as_ref(), server_addr));
            }
        }

        ToggleTarget::Docker => {
            if !rooted {
                results.push(not_root_result("docker"));
            } else {
                results.push(docker_proxy_on(server_addr));
            }
        }

        ToggleTarget::Shadow => {
            // Shadow toggle modifies client config, which may or may not need root
            // depending on the config path. Try it and report errors.
            results.push(shadow_proxy_on());
        }
    }

    results
}

/// Disable proxying for the specified target.
pub async fn proxy_off(target: ToggleTarget) -> Vec<ProxyToggleResult> {
    let mut results = Vec::new();
    let rooted = is_root();

    match target {
        ToggleTarget::All => {
            let backends = get_available_backends();
            if backends.is_empty() {
                results.push(ProxyToggleResult {
                    subsystem: "package-managers".to_string(),
                    success: true,
                    message: "No supported package managers detected".to_string(),
                });
            }
            for backend in &backends {
                let name = backend.kind().as_str();
                if !rooted {
                    results.push(not_root_result(name));
                } else {
                    results.push(pm_proxy_off(backend.as_ref()));
                }
            }

            // Docker
            if !rooted {
                results.push(not_root_result("docker"));
            } else {
                results.push(docker_proxy_off());
            }
        }

        ToggleTarget::Pm(pm_name) => {
            let kind = match PackageManagerKind::parse_kind(&pm_name.to_lowercase()) {
                Some(k) => k,
                None => {
                    results.push(ProxyToggleResult {
                        subsystem: pm_name.clone(),
                        success: false,
                        message: format!(
                            "Unknown package manager '{}'. Valid: pacman, apt, dnf, apk, winget, choco, scoop, pkg, pkg_add, pkgin, mport",
                            pm_name
                        ),
                    });
                    return results;
                }
            };

            let backend = match get_backend(kind) {
                Some(b) => b,
                None => {
                    results.push(ProxyToggleResult {
                        subsystem: pm_name.clone(),
                        success: false,
                        message: format!("Backend for '{}' not available", pm_name),
                    });
                    return results;
                }
            };

            if !backend.is_available() {
                results.push(ProxyToggleResult {
                    subsystem: pm_name.clone(),
                    success: false,
                    message: format!("Package manager '{}' is not installed on this system", pm_name),
                });
                return results;
            }

            if !rooted {
                results.push(not_root_result(&pm_name));
            } else {
                results.push(pm_proxy_off(backend.as_ref()));
            }
        }

        ToggleTarget::Docker => {
            if !rooted {
                results.push(not_root_result("docker"));
            } else {
                results.push(docker_proxy_off());
            }
        }

        ToggleTarget::Shadow => {
            results.push(shadow_proxy_off());
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mirror_url_formats() {
        assert_eq!(mirror_url("192.168.1.1:50051"), "http://192.168.1.1:3128");
        assert_eq!(mirror_url("192.168.1.1"), "http://192.168.1.1:3128");
        assert_eq!(
            mirror_url("http://server:3128"),
            "http://server:3128"
        );
        assert_eq!(
            mirror_url("https://secure:443"),
            "https://secure:443"
        );
    }

    #[test]
    fn test_docker_daemon_json_path() {
        let path = docker_daemon_json_path();
        assert!(path.to_string_lossy().contains("daemon.json"));
    }

    #[test]
    fn test_read_daemon_json_missing() {
        // Reading a non-existent path returns empty object.
        // This test is safe because read_daemon_json uses a fixed path
        // that might not exist on CI. Just verify it doesn't panic.
        let val = read_daemon_json();
        // Should be some JSON value (object or empty).
        assert!(val.is_object() || val.is_null());
    }

    #[test]
    fn test_proxy_toggle_result_construction() {
        let r = ProxyToggleResult {
            subsystem: "test".to_string(),
            success: true,
            message: "ok".to_string(),
        };
        assert!(r.success);
        assert_eq!(r.subsystem, "test");
    }

    #[test]
    fn test_toggle_target_variants() {
        let _a = ToggleTarget::All;
        let _p = ToggleTarget::Pm("pacman".to_string());
        let _d = ToggleTarget::Docker;
        let _s = ToggleTarget::Shadow;
    }

    #[test]
    fn test_collect_write_paths() {
        let action = ProxyConfigAction::Multiple(vec![
            ProxyConfigAction::WriteFile {
                path: PathBuf::from("/tmp/a"),
                content: "a".to_string(),
            },
            ProxyConfigAction::WriteFile {
                path: PathBuf::from("/tmp/b"),
                content: "b".to_string(),
            },
            ProxyConfigAction::None,
        ]);
        let mut paths = Vec::new();
        collect_write_paths(&action, &mut paths);
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn test_collect_remove_paths() {
        let action = ProxyConfigAction::Multiple(vec![
            ProxyConfigAction::RemoveFile {
                path: PathBuf::from("/tmp/a"),
            },
            ProxyConfigAction::None,
        ]);
        let mut paths = Vec::new();
        collect_remove_paths(&action, &mut paths);
        assert_eq!(paths.len(), 1);
    }
}
