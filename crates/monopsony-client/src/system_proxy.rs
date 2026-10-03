//! macOS system-proxy forwarding (Sprint 26 Phase 3 Tier 1).
//!
//! Routes system-wide HTTP(S) traffic through the LOCAL Monopsony client
//! proxy (`127.0.0.1:3129`) via `networksetup`. The client proxy relays
//! CONNECT tunnels to the server (Sprint 19 Phase 1) — **forwarding, not
//! caching**: App Store / GUI-app HTTPS is opaque by design. Docs and UI
//! must keep that label honest.
//!
//! Enable:  `networksetup -setwebproxy <svc> 127.0.0.1 3129` and
//!          `-setsecurewebproxy <svc> 127.0.0.1 3129` per network service.
//! Disable: `-setwebproxystate <svc> off` and `-setsecurewebproxystate <svc>
//!          off` (clean removal — the stored server fields stay but are
//!          inert, matching what uninstallers do).
//! Status:  `-getwebproxy` / `-getsecurewebproxy` → `Enabled: Yes|No` plus
//!          Server/Port; ON only when enabled AND pointed at us.

use monopsony_common::package_manager::ProxyConfigAction;

/// Local client-proxy address the system proxy points at.
pub const SYSTEM_PROXY_HOST: &str = "127.0.0.1";
pub const SYSTEM_PROXY_PORT: u16 = 3129;

/// The `host:port` string for the local client proxy (pre-connect check).
pub fn listen_addr() -> String {
    format!("{}:{}", SYSTEM_PROXY_HOST, SYSTEM_PROXY_PORT)
}

/// networksetup lives here on every macOS release.
const NETWORKSETUP: &str = "/usr/sbin/networksetup";

/// System-proxy forwarding is a macOS-only feature.
pub fn is_supported() -> bool {
    cfg!(target_os = "macos") && std::path::Path::new(NETWORKSETUP).exists()
}

/// Parse `networksetup -listallnetworkservices` output into service names.
///
/// Line 1 is a legend ("An asterisk (*) denotes …"); `*`-prefixed lines are
/// DISABLED services — skipped (configuring them is a no-op that confuses
/// status).
pub fn parse_network_services(output: &str) -> Vec<String> {
    output
        .lines()
        .skip(1) // legend line
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.starts_with('*'))
        .map(str::to_string)
        .collect()
}

/// Parse `networksetup -getwebproxy|-getsecurewebproxy <svc>` output into
/// (enabled, server, port). Missing/blank fields parse as None/0.
pub fn parse_proxy_state(output: &str) -> (bool, Option<String>, Option<u16>) {
    let mut enabled = false;
    let mut server = None;
    let mut port = None;
    for line in output.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        match key.trim() {
            "Enabled" => enabled = val.eq_ignore_ascii_case("yes"),
            "Server" if !val.is_empty() => server = Some(val.to_string()),
            "Port" => port = val.parse::<u16>().ok(),
            _ => {}
        }
    }
    (enabled, server, port)
}

/// Build the enable actions: web + secure web proxy per service, pointed at
/// the local client proxy. Executed through the shared `ProxyConfigAction`
/// executor (same one the PM toggles use).
pub fn enable_actions(services: &[String]) -> ProxyConfigAction {
    let mut cmds = Vec::new();
    for svc in services {
        for kind in ["-setwebproxy", "-setsecurewebproxy"] {
            cmds.push(ProxyConfigAction::RunCommand {
                program: NETWORKSETUP.to_string(),
                args: vec![
                    kind.to_string(),
                    svc.clone(),
                    SYSTEM_PROXY_HOST.to_string(),
                    SYSTEM_PROXY_PORT.to_string(),
                ],
            });
        }
    }
    if cmds.is_empty() {
        ProxyConfigAction::None
    } else {
        ProxyConfigAction::Multiple(cmds)
    }
}

/// Build the disable actions: state-off per service (clean removal).
pub fn disable_actions(services: &[String]) -> ProxyConfigAction {
    let mut cmds = Vec::new();
    for svc in services {
        for kind in ["-setwebproxystate", "-setsecurewebproxystate"] {
            cmds.push(ProxyConfigAction::RunCommand {
                program: NETWORKSETUP.to_string(),
                args: vec![kind.to_string(), svc.clone(), "off".to_string()],
            });
        }
    }
    if cmds.is_empty() {
        ProxyConfigAction::None
    } else {
        ProxyConfigAction::Multiple(cmds)
    }
}

fn run(program: &str, args: &[String]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {}", program, e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "{} {} failed: {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Enumerate configured (non-disabled) network services.
pub fn network_services() -> Result<Vec<String>, String> {
    let out = run(NETWORKSETUP, &["-listallnetworkservices".to_string()])?;
    Ok(parse_network_services(&out))
}

/// Per-service status: (service, on, detail). ON means enabled AND pointed
/// at the local client proxy.
pub fn status() -> Result<Vec<(String, bool, String)>, String> {
    let services = network_services()?;
    let mut rows = Vec::new();
    for svc in services {
        let mut on_any = false;
        let mut details = Vec::new();
        for kind in ["-getwebproxy", "-getsecurewebproxy"] {
            let out = run(NETWORKSETUP, &[kind.to_string(), svc.clone()])?;
            let (enabled, server, port) = parse_proxy_state(&out);
            let ours = enabled
                && server.as_deref() == Some(SYSTEM_PROXY_HOST)
                && port == Some(SYSTEM_PROXY_PORT);
            if ours {
                on_any = true;
            }
            details.push(format!(
                "{} {}:{:?}",
                kind.trim_start_matches("-get"),
                server.unwrap_or_default(),
                port
            ));
        }
        rows.push((
            svc,
            on_any,
            if on_any {
                "forwarded via monopsony client proxy".to_string()
            } else {
                details.join("; ")
            },
        ));
    }
    Ok(rows)
}

/// Apply enable/disable across all configured services. Returns per-service
/// (service, ok, message) rows for the toggle table.
pub fn apply(enable: bool) -> Vec<(String, bool, String)> {
    let services = match network_services() {
        Ok(s) => s,
        Err(e) => {
            return vec![("system-proxy".to_string(), false, e)];
        }
    };
    if services.is_empty() {
        return vec![(
            "system-proxy".to_string(),
            false,
            "no configurable network services found".to_string(),
        )];
    }
    let action = if enable {
        enable_actions(&services)
    } else {
        disable_actions(&services)
    };
    let results = action.execute();
    let all_ok = results.iter().all(|r| r.is_ok());
    let summary = results
        .iter()
        .map(|r| match r {
            Ok(desc) => desc.clone(),
            Err(e) => format!("ERR: {}", e),
        })
        .collect::<Vec<_>>()
        .join("; ");
    services
        .into_iter()
        .map(|svc| (svc, all_ok, summary.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICES_SAMPLE: &str = "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\nEthernet\n*Bluetooth PAN\nThunderbolt Bridge\n";

    #[test]
    fn test_parse_network_services() {
        let svc = parse_network_services(SERVICES_SAMPLE);
        assert_eq!(svc, vec!["Wi-Fi", "Ethernet", "Thunderbolt Bridge"]);
    }

    #[test]
    fn test_parse_network_services_empty() {
        let svc =
            parse_network_services("An asterisk (*) denotes that a network service is disabled.\n");
        assert!(svc.is_empty());
    }

    #[test]
    fn test_parse_proxy_state_on() {
        let out = "Enabled: Yes\nServer: 127.0.0.1\nPort: 3129\nAuthenticated Proxy Enabled: 0\n";
        let (enabled, server, port) = parse_proxy_state(out);
        assert!(enabled);
        assert_eq!(server.as_deref(), Some("127.0.0.1"));
        assert_eq!(port, Some(3129));
    }

    #[test]
    fn test_parse_proxy_state_off() {
        // Real off-state output: "Enabled: No\nServer: \nPort: 0\n…". Port 0
        // parses as Some(0) — status() treats any non-3129 port as not ours.
        let out = "Enabled: No\nServer: \nPort: 0\nAuthenticated Proxy Enabled: 0\n";
        let (enabled, server, port) = parse_proxy_state(out);
        assert!(!enabled);
        assert_eq!(server, None);
        assert_eq!(port, Some(0));
    }

    #[test]
    fn test_enable_action_shapes() {
        let svcs = vec!["Wi-Fi".to_string()];
        match enable_actions(&svcs) {
            ProxyConfigAction::Multiple(cmds) => {
                assert_eq!(cmds.len(), 2); // web + secure web
                match &cmds[0] {
                    ProxyConfigAction::RunCommand { program, args } => {
                        assert_eq!(program, "/usr/sbin/networksetup");
                        assert_eq!(
                            args,
                            &vec![
                                "-setwebproxy".to_string(),
                                "Wi-Fi".to_string(),
                                "127.0.0.1".to_string(),
                                "3129".to_string()
                            ]
                        );
                    }
                    other => panic!("expected RunCommand, got {:?}", other),
                }
            }
            other => panic!("expected Multiple, got {:?}", other),
        }
    }

    #[test]
    fn test_disable_action_shapes() {
        let svcs = vec!["Wi-Fi".to_string()];
        match disable_actions(&svcs) {
            ProxyConfigAction::Multiple(cmds) => {
                assert_eq!(cmds.len(), 2);
                match &cmds[0] {
                    ProxyConfigAction::RunCommand { program, args } => {
                        assert_eq!(program, "/usr/sbin/networksetup");
                        assert_eq!(
                            args,
                            &vec![
                                "-setwebproxystate".to_string(),
                                "Wi-Fi".to_string(),
                                "off".to_string()
                            ]
                        );
                    }
                    other => panic!("expected RunCommand, got {:?}", other),
                }
            }
            other => panic!("expected Multiple, got {:?}", other),
        }
    }

    #[test]
    fn test_actions_empty_services_none() {
        assert!(matches!(enable_actions(&[]), ProxyConfigAction::None));
        assert!(matches!(disable_actions(&[]), ProxyConfigAction::None));
    }

    #[test]
    fn test_is_supported_off_macos() {
        // Pure signal check: on the Linux dev box this is false; on a macOS
        // build host with networksetup present it flips true. Either way it
        // must not panic.
        let _ = is_supported();
    }
}
