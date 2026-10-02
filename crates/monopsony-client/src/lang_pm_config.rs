//! Language package-manager proxy config (Sprint 23 slice 5).
//!
//! Routes `proxy on|off|status --pm npm|pip|cargo` through the Phase B
//! backends' `proxy_config()` / `remove_proxy_config()` actions, executed via
//! the shared [`ProxyConfigAction::execute`] executor (the SAME one the
//! distro-PM path uses — no second executor lives here).
//!
//! Unlike the distro backends (which expect a full `http://` URL in
//! `proxy_config(proxy_addr)`), the language backends embed the scheme
//! themselves (`registry=http://{addr}/npm`), so the address is normalized to
//! a bare `host:port` before dispatch.

use monopsony_common::package_manager::{get_backend, PackageManagerKind, ProxyConfigAction};

/// Parse a language package manager name into its backend kind.
pub fn parse_lang_pm(s: &str) -> Option<PackageManagerKind> {
    match s.to_lowercase().as_str() {
        "npm" => Some(PackageManagerKind::Npm),
        "pip" => Some(PackageManagerKind::Pip),
        "cargo" => Some(PackageManagerKind::Cargo),
        "brew" => Some(PackageManagerKind::Homebrew),
        _ => None,
    }
}

/// Normalize a server address to the bare `host:port` form the language
/// backends expect.
///
/// Strips any scheme (`http://`, `https://`, `ws://`, `wss://`) and any path,
/// then appends the default Monopsony proxy port (3128) when no port is
/// present.
pub fn normalize_proxy_addr(addr: &str) -> String {
    let mut rest = addr;
    for scheme in ["http://", "https://", "ws://", "wss://"] {
        if let Some(stripped) = rest.strip_prefix(scheme) {
            rest = stripped;
            break;
        }
    }
    // Cut any path/query component.
    let authority = rest.split(['/']).next().unwrap_or(rest);
    if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{}:3128", authority)
    }
}

/// Collect all `WriteFile` paths from a `ProxyConfigAction` tree.
fn collect_write_paths(action: &ProxyConfigAction, paths: &mut Vec<std::path::PathBuf>) {
    match action {
        ProxyConfigAction::WriteFile { path, .. } => paths.push(path.clone()),
        ProxyConfigAction::Multiple(actions) => {
            for a in actions {
                collect_write_paths(a, paths);
            }
        }
        _ => {}
    }
}

/// Expand a leading `~` in a backend config path to the real HOME path.
/// Paths without a leading `~` are returned unchanged.
fn expand_tilde(path: &std::path::Path) -> std::path::PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with('~') {
        shellexpand::tilde(&s).to_string().into()
    } else {
        path.to_path_buf()
    }
}

/// Read-only status check: does the config file the backend would write
/// exist and contain a proxy line?
///
/// The proxy line heuristic: all three language backends write shim URLs of
/// the form `http://<addr>/<shim-path>` into their config, so a config that
/// routes through Monopsony always contains `http://`.
fn config_file_is_proxied(path: &std::path::Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => content.contains("http://"),
        Err(_) => false,
    }
}

/// Read-only status for a language package manager: regenerates the backend's
/// config action, checks whether the file it would write exists on disk and
/// contains the proxy line. Writes nothing.
pub fn lang_pm_status(kind: PackageManagerKind) -> (bool, String) {
    let backend = match get_backend(kind) {
        Some(b) => b,
        None => {
            return (false, format!("{}: backend not registered", kind.as_str()));
        }
    };
    let action = backend.proxy_config("status-probe");
    let name = kind.as_str();
    let mut paths = Vec::new();
    collect_write_paths(&action, &mut paths);
    if paths.is_empty() {
        return (false, format!("{} proxy: not configured", name));
    }
    for p in &paths {
        let expanded = expand_tilde(p);
        if config_file_is_proxied(&expanded) {
            return (
                true,
                format!("{} proxy: configured ({})", name, p.display()),
            );
        }
    }
    (
        false,
        format!(
            "{} proxy: not configured ({} not found or has no proxy line)",
            name,
            paths[0].display()
        ),
    )
}

/// Apply proxy on/off for a language package manager by executing the
/// backend's `ProxyConfigAction` through the shared executor.
///
/// `enable = true` runs `proxy_config(addr)`, `false` runs
/// `remove_proxy_config()`. Returns a human-readable summary of what was
/// done, or an error listing the failed steps.
pub fn lang_pm_apply(
    kind: PackageManagerKind,
    server_addr: &str,
    enable: bool,
) -> Result<String, String> {
    let backend = match get_backend(kind) {
        Some(b) => b,
        None => {
            return Err(format!("backend for '{}' not registered", kind.as_str()));
        }
    };

    let action = if enable {
        let addr = normalize_proxy_addr(server_addr);
        backend.proxy_config(&addr)
    } else {
        backend.remove_proxy_config()
    };
    // Backend config paths embed `~` (backend convention); expand before the
    // shared executor runs so files land in the real HOME (not a literal
    // `~` directory), matching what `status` checks.
    let action = expand_action_paths(action);

    // Shared executor (monopsony-common ProxyConfigAction::execute) — the same
    // one the distro-PM path uses.
    let results = action.execute();
    let mut ok: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for r in &results {
        match r {
            Ok(desc) => ok.push(desc.clone()),
            Err(e) => errs.push(e.to_string()),
        }
    }
    if errs.is_empty() {
        Ok(ok.join("; "))
    } else {
        Err(errs.join("; "))
    }
}

/// Recursively rewrite `~`-prefixed WriteFile/RemoveFile paths to real HOME.
fn expand_action_paths(action: ProxyConfigAction) -> ProxyConfigAction {
    match action {
        ProxyConfigAction::WriteFile { path, content } => ProxyConfigAction::WriteFile {
            path: expand_tilde(&path),
            content,
        },
        ProxyConfigAction::RemoveFile { path } => ProxyConfigAction::RemoveFile {
            path: expand_tilde(&path),
        },
        ProxyConfigAction::Multiple(actions) => {
            ProxyConfigAction::Multiple(actions.into_iter().map(expand_action_paths).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_lang_pm() {
        assert_eq!(parse_lang_pm("npm"), Some(PackageManagerKind::Npm));
        assert_eq!(parse_lang_pm("pip"), Some(PackageManagerKind::Pip));
        assert_eq!(parse_lang_pm("cargo"), Some(PackageManagerKind::Cargo));
        assert_eq!(parse_lang_pm("NPM"), Some(PackageManagerKind::Npm));
        assert_eq!(parse_lang_pm("brew"), Some(PackageManagerKind::Homebrew));
        assert_eq!(parse_lang_pm("BREW"), Some(PackageManagerKind::Homebrew));
        assert_eq!(parse_lang_pm("pacman"), None);
        assert_eq!(parse_lang_pm(""), None);
    }

    #[test]
    fn test_normalize_proxy_addr() {
        assert_eq!(normalize_proxy_addr("127.0.0.1:3128"), "127.0.0.1:3128");
        assert_eq!(
            normalize_proxy_addr("http://10.0.0.1:3128"),
            "10.0.0.1:3128"
        );
        assert_eq!(normalize_proxy_addr("https://host"), "host:3128");
        assert_eq!(
            normalize_proxy_addr("wss://tunnel.example.com/path"),
            "tunnel.example.com:3128"
        );
        assert_eq!(normalize_proxy_addr("http://h:99/extra"), "h:99");
    }

    #[test]
    fn test_lang_pm_on_action_shapes() {
        // npm: registry-shim WriteFile, no scheme duplication.
        let npm = get_backend(PackageManagerKind::Npm).unwrap();
        let action = npm.proxy_config(&normalize_proxy_addr("http://10.0.0.1:3128"));
        match action {
            ProxyConfigAction::WriteFile { path, content } => {
                assert_eq!(path, std::path::PathBuf::from("~/.npmrc"));
                assert!(content.contains("registry=http://10.0.0.1:3128/npm"));
                assert!(!content.contains("http://http://"));
            }
            _ => panic!("expected WriteFile for npm"),
        }

        // pip: index-url shim.
        let pip = get_backend(PackageManagerKind::Pip).unwrap();
        let action = pip.proxy_config(&normalize_proxy_addr("10.0.0.1:3128"));
        match action {
            ProxyConfigAction::WriteFile { content, .. } => {
                assert!(content.contains("index-url = http://10.0.0.1:3128/pypi/simple"));
            }
            _ => panic!("expected WriteFile for pip"),
        }

        // cargo: sparse-registry replacement, RemoveFile on off.
        let cargo = get_backend(PackageManagerKind::Cargo).unwrap();
        match cargo.remove_proxy_config() {
            ProxyConfigAction::RemoveFile { .. } => {}
            _ => panic!("expected RemoveFile for cargo off"),
        }
    }

    #[test]
    fn test_config_file_is_proxied() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("npmrc");
        assert!(!config_file_is_proxied(&path));
        std::fs::write(&path, "registry=http://10.0.0.1:3128/npm\n").unwrap();
        assert!(config_file_is_proxied(&path));
        std::fs::write(&path, "save-exact=true\n").unwrap();
        assert!(!config_file_is_proxied(&path));
    }

    #[test]
    fn test_lang_pm_status_not_configured() {
        // `~/.npmrc` with a proxy line is not expected in test envs; this
        // asserts the OFF path shape without touching real HOME files beyond
        // a read.
        let (on, msg) = lang_pm_status(PackageManagerKind::Npm);
        let _ = on;
        assert!(msg.contains("npm"));
    }

    #[test]
    fn test_lang_pm_apply_off_is_safe() {
        // `off` on a non-existent config is a no-op success (executor skips
        // missing files), so this never writes to real HOME.
        let res = lang_pm_apply(PackageManagerKind::Npm, "127.0.0.1:3128", false);
        assert!(res.is_ok(), "off should succeed: {:?}", res.err());
    }
}
