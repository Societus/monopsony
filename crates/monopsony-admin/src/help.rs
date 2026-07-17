//! Help system for the dashboard — static lookup of field and element help text.
//!
//! Returns JSON so the frontend controls rendering. No raw HTML injection.
//! Two namespaces: `field:` for recipe TOML fields, `gui:` for clickable elements.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

/// Response body for GET /api/v1/help/:key
#[derive(Debug, Serialize)]
pub struct HelpResponse {
    pub key: String,
    pub title: String,
    pub body: String,
}

/// One entry in the static help table.
struct HelpEntry {
    key: &'static str,
    title: &'static str,
    body: &'static str,
}

/// All help entries.  Keys use `field:` prefix for recipe TOML fields,
/// `gui:` prefix for dashboard clickable elements, and `section:` for
/// section-level intros shown when a TOML section header is typed but empty.
const HELP_DATA: &[HelpEntry] = &[
    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — Recipe TOML Section Intros
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "section:package",
        title: "[package] Section",
        body: "Identifies what software this recipe manages. Set `name` to the binary or app name, `source_type` to where updates come from, and `source_url` to the upstream location.",
    },
    HelpEntry {
        key: "section:install",
        title: "[install] Section",
        body: "Defines how the downloaded package is put on the target system. `method` controls the strategy: `extract` (unpack archive), `build` (compile from source), or `script` (run a custom shell script).",
    },
    HelpEntry {
        key: "section:install.health_check",
        title: "[install.health_check] Section",
        body: "Without a health_check, the update pipeline swaps the binary and assumes success. Adding one lets the system detect a broken update and automatically roll back to the previous version.",
    },
    HelpEntry {
        key: "section:asset_selector",
        title: "[asset_selector] Section",
        body: "GitHub releases often contain multiple files for different OS/arch combinations. This section tells the system which file to download for each client platform using regex patterns.",
    },
    HelpEntry {
        key: "section:version",
        title: "[version] Section",
        body: "Tells the system how to check what version is currently installed. The `detect_command` runs on the client, and `detect_pattern` extracts the version number from its output using a regex capture group.",
    },
    HelpEntry {
        key: "section:direct_url",
        title: "[direct_url] Section",
        body: "For packages not hosted on GitHub. Defines how to discover versions and construct download URLs using scrape or URL template patterns.",
    },
    HelpEntry {
        key: "section:build",
        title: "[build] Section",
        body: "When method=build, the system clones the source repo and runs these build steps to produce the binary.",
    },
    HelpEntry {
        key: "section:script",
        title: "[script] Section",
        body: "When method=script, the system runs a single shell script that handles the entire update process — download, install, restart.",
    },
    HelpEntry {
        key: "section:notifications",
        title: "[notifications] Section",
        body: "Optional metadata shown in the dashboard after updates. Not required for functionality.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [package] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:package.name",
        title: "package.name",
        body: "The package name. Must match the binary name in the archive (e.g., 'sonarr', 'monopsony-server').",
    },
    HelpEntry {
        key: "field:package.description",
        title: "package.description",
        body: "Human-readable description shown in the dashboard.",
    },
    HelpEntry {
        key: "field:package.homepage",
        title: "package.homepage",
        body: "URL to the project's website. Displayed for reference.",
    },
    HelpEntry {
        key: "field:package.source_type",
        title: "package.source_type",
        body: "Where releases come from: `github_release` (GitHub Releases API), `git_build` (clone + compile), or `direct_url` (scrape a URL).",
    },
    HelpEntry {
        key: "field:package.source_url",
        title: "package.source_url",
        body: "The upstream URL. For github_release, use `https://github.com/OWNER/REPO`. For direct_url, the base URL to scrape.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [install] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:install.method",
        title: "install.method",
        body: "`extract` = unpack tar.gz/zip to target_dir. `build` = run build steps then copy result. `script` = run a custom update script.",
    },
    HelpEntry {
        key: "field:install.target_dir",
        title: "install.target_dir",
        body: "Absolute path where the binary is installed (e.g., `/usr/local/bin`). Must start with /.",
    },
    HelpEntry {
        key: "field:install.chmod",
        title: "install.chmod",
        body: "Binary names to mark executable after extraction. Usually the same as the package name.",
    },
    HelpEntry {
        key: "field:install.service",
        title: "install.service",
        body: "systemd service name to stop before update and start after (e.g., `sonarr.service`).",
    },
    HelpEntry {
        key: "field:install.config_paths",
        title: "install.config_paths",
        body: "Config files to back up before update and restore if the update fails (e.g., `[\"/etc/sonarr/config.xml\"]`).",
    },
    HelpEntry {
        key: "field:install.backup_old",
        title: "install.backup_old",
        body: "If true, the old binary is saved to a versioned backup directory before overwriting. Default: false.",
    },
    HelpEntry {
        key: "field:install.pre_update",
        title: "install.pre_update",
        body: "Shell command to run before the binary is swapped. Typically stops the service: `systemctl stop sonarr`.",
    },
    HelpEntry {
        key: "field:install.post_update",
        title: "install.post_update",
        body: "Shell command to run after the new binary is in place. Typically starts the service: `systemctl start sonarr`.",
    },
    HelpEntry {
        key: "field:install.on_failure",
        title: "install.on_failure",
        body: "Shell command to run if the health check fails and rollback is triggered. Ensures the old service is running again.",
    },
    HelpEntry {
        key: "field:install.max_backups",
        title: "install.max_backups",
        body: "Maximum number of old version backup directories to keep. Older ones are automatically pruned. Default: 3.",
    },
    HelpEntry {
        key: "field:install.binaries",
        title: "install.binaries",
        body: "For multi-binary archives. Each entry has `source` (path in archive), `target` (install path), and optional `mode` (chmod like '0755').",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [install.health_check] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:install.health_check.type",
        title: "install.health_check.type",
        body: "`http` = GET a URL and check for 200. `port` = TCP connect to a port. `process` = check a process name is running via pgrep.",
    },
    HelpEntry {
        key: "field:install.health_check.url",
        title: "install.health_check.url",
        body: "Full URL to check (only for type=http). E.g., `http://localhost:8989/health`.",
    },
    HelpEntry {
        key: "field:install.health_check.port",
        title: "install.health_check.port",
        body: "Port number to test TCP connectivity (only for type=port). E.g., `50051`.",
    },
    HelpEntry {
        key: "field:install.health_check.process",
        title: "install.health_check.process",
        body: "Process name to check with pgrep -x (only for type=process). E.g., `monopsony-server`.",
    },
    HelpEntry {
        key: "field:install.health_check.timeout_secs",
        title: "install.health_check.timeout_secs",
        body: "How long to wait (polling every 2 seconds) before declaring the health check failed. Default: 30.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [asset_selector] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:asset_selector.tag_pattern",
        title: "asset_selector.tag_pattern",
        body: "Regex to match valid release tags. E.g., `v(\\d+\\.\\d+\\.\\d+)$` matches v1.2.3 but not v1.2.3-beta.",
    },
    HelpEntry {
        key: "field:asset_selector.rules",
        title: "[[asset_selector.rules]]",
        body: "One rule per OS/arch combination. The system tries each rule in order. First match wins.",
    },
    HelpEntry {
        key: "field:asset_selector.rules.os",
        title: "rules[].os",
        body: "Operating system: `linux`, `windows`, `freebsd`, `darwin`.",
    },
    HelpEntry {
        key: "field:asset_selector.rules.arch",
        title: "rules[].arch",
        body: "CPU architecture: `x86_64`, `aarch64`, `armv7`.",
    },
    HelpEntry {
        key: "field:asset_selector.rules.libc",
        title: "rules[].libc",
        body: "Optional. `glibc` or `musl` for Linux. Omit for Windows/macOS.",
    },
    HelpEntry {
        key: "field:asset_selector.rules.pattern",
        title: "rules[].pattern",
        body: "Regex matched against release asset filenames. First match wins. E.g., `linux-amd64\\.tar\\.gz$`.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [version] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:version.detect_command",
        title: "version.detect_command",
        body: "Shell command that outputs the version. E.g., `/usr/local/bin/sonarr --version` or `cat /etc/app/version`.",
    },
    HelpEntry {
        key: "field:version.detect_pattern",
        title: "version.detect_pattern",
        body: "Regex with one capture group for the version. E.g., `(\\d+\\.\\d+\\.\\d+)` extracts '4.0.7' from 'Sonarr 4.0.7.1863'.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [direct_url] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:direct_url.version_discovery",
        title: "direct_url.version_discovery",
        body: "`scrape` = download a page and regex-extract version numbers from it. `url_pattern` = construct the version URL from a template.",
    },
    HelpEntry {
        key: "field:direct_url.scrape_url",
        title: "direct_url.scrape_url",
        body: "URL to scrape for version numbers (only if discovery=scrape).",
    },
    HelpEntry {
        key: "field:direct_url.scrape_pattern",
        title: "direct_url.scrape_pattern",
        body: "Regex with one capture group to extract version from scraped HTML.",
    },
    HelpEntry {
        key: "field:direct_url.asset_url_template",
        title: "direct_url.asset_url_template",
        body: "Download URL with `{version}`, `{arch}`, `{os}` placeholders. E.g., `https://example.com/app-{version}-linux-{arch}.tar.gz`.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [build] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:build.steps",
        title: "build.steps",
        body: "Shell commands to run in order. E.g., `[\"cargo build --release\", \"cp target/release/myapp /usr/local/bin/\"]`.",
    },
    HelpEntry {
        key: "field:build.requires",
        title: "build.requires",
        body: "Build dependencies to install first. E.g., `[\"gcc\", \"make\", \"pkg-config\"]`.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [script] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:script.update",
        title: "script.update",
        body: "Full shell script. Has access to `$VERSION`, `$ASSET_URL`, `$TARGET_DIR` environment variables.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 2 — [notifications] Fields
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "field:notifications.changelog_url",
        title: "notifications.changelog_url",
        body: "URL to the release notes or changelog. Shown in the dashboard after an update.",
    },
    HelpEntry {
        key: "field:notifications.notes",
        title: "notifications.notes",
        body: "Free-text notes about this update. Displayed to the operator.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Navigation Sidebar
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:nav.overview",
        title: "Overview",
        body: "System health at a glance — service status, cache info, and client/shadow package counts.",
    },
    HelpEntry {
        key: "gui:nav.cache",
        title: "Proxy Cache",
        body: "Browse and manage cached package files. Shows hit counts, sizes, and lets you purge entries.",
    },
    HelpEntry {
        key: "gui:nav.shadow",
        title: "Shadow Packages",
        body: "Software managed outside the system package manager. Create recipes to auto-update third-party apps.",
    },
    HelpEntry {
        key: "gui:nav.settings",
        title: "Settings",
        body: "Edit the server's TOML config file directly. Changes require a server restart to take effect.",
    },
    HelpEntry {
        key: "gui:nav.mirrors",
        title: "Mirrors",
        body: "Distribution mirror management. Sync repos locally (Arch, Debian, Alpine) so clients download from your server instead of the internet.",
    },
    HelpEntry {
        key: "gui:nav.tracker",
        title: "Tracker",
        body: "Peer discovery tracker. Helps clients find mirror servers on your network or across the internet.",
    },
    HelpEntry {
        key: "gui:nav.monetization",
        title: "Monetization",
        body: "Optional tipping and bounty settings. Bitcoin/Lightning addresses, bug bounty payouts, value-for-value boosts.",
    },
    HelpEntry {
        key: "gui:nav.security",
        title: "Security",
        body: "TLS certificates, vulnerability scans, and quarantined packages. (Coming soon.)",
    },
    HelpEntry {
        key: "gui:nav.clients",
        title: "Clients",
        body: "Registered client machines. Shows hostname, OS, last-seen time, and pairing status.",
    },
    HelpEntry {
        key: "gui:nav.logout",
        title: "Logout",
        body: "End your dashboard session. You'll need to log in again.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Overview Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:overview.clients",
        title: "Clients",
        body: "Total machines that have paired with this server via `monopsony-client pair`.",
    },
    HelpEntry {
        key: "gui:overview.packages_tracked",
        title: "Packages Tracked",
        body: "Unique package URLs the server has seen across all clients' inventories.",
    },
    HelpEntry {
        key: "gui:overview.shadow_packages",
        title: "Shadow Packages",
        body: "Third-party applications managed through shadow package recipes.",
    },
    HelpEntry {
        key: "gui:overview.cache_dir",
        title: "Cache Dir",
        body: "Whether the proxy cache directory exists and is writable. 'Missing' means caching is broken.",
    },
    HelpEntry {
        key: "gui:overview.service_health",
        title: "Service Health",
        body: "Green dot = service is responding. Red dot = service is down or unreachable. Shows database, shadow, mirror, and TLS subsystems.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Proxy Cache Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:cache.refresh",
        title: "Refresh",
        body: "Reload the cache entry list and stats from the server.",
    },
    HelpEntry {
        key: "gui:cache.total_entries",
        title: "Total Entries",
        body: "Number of unique URLs currently cached on disk.",
    },
    HelpEntry {
        key: "gui:cache.total_size",
        title: "Total Size",
        body: "Combined disk usage of all cached files.",
    },
    HelpEntry {
        key: "gui:cache.meta_files",
        title: "Meta Files",
        body: "JSON sidecar files storing hit counts, timestamps, and content hashes for each cache entry.",
    },
    HelpEntry {
        key: "gui:cache.filter",
        title: "Filter by URL",
        body: "Type to narrow the table to matching URLs. Case-insensitive.",
    },
    HelpEntry {
        key: "gui:cache.purge",
        title: "Purge",
        body: "Delete this cache entry. The next request for this URL will re-download from upstream.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Shadow Packages Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:shadow.refresh",
        title: "Refresh",
        body: "Reload the list of registered shadow packages.",
    },
    HelpEntry {
        key: "gui:shadow.new_recipe",
        title: "New Recipe",
        body: "Open the TOML recipe editor to define a new shadow package.",
    },
    HelpEntry {
        key: "gui:shadow.unregister",
        title: "Unregister",
        body: "Remove this shadow package from the server. Does not uninstall the binary.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Recipe Editor Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:recipe.back",
        title: "Back",
        body: "Return to the Shadow Packages list.",
    },
    HelpEntry {
        key: "gui:recipe.estimate",
        title: "Estimate from Binary",
        body: "Upload a binary file and the system will try to auto-generate a recipe by inspecting it (ELF headers, embedded version strings, common patterns).",
    },
    HelpEntry {
        key: "gui:recipe.save",
        title: "Save & Register",
        body: "Validate the recipe and register it with the server. The package will appear in the Shadow Packages list.",
    },
    HelpEntry {
        key: "gui:recipe.editor",
        title: "TOML Editor",
        body: "Write the shadow package recipe here. Validation runs automatically as you type.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Settings Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:settings.reset",
        title: "Reset",
        body: "Discard your edits and reload the current config from the server.",
    },
    HelpEntry {
        key: "gui:settings.save",
        title: "Save",
        body: "Write the edited config to disk. The server must be restarted for changes to take effect.",
    },
    HelpEntry {
        key: "gui:settings.config_path",
        title: "Config file",
        body: "The on-disk location of server.toml that this editor modifies.",
    },
    HelpEntry {
        key: "gui:settings.editor",
        title: "TOML Editor",
        body: "Direct edit of server.toml. Be careful — invalid TOML can prevent the server from starting.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Mirrors Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:mirrors.refresh",
        title: "Refresh",
        body: "Reload mirror configurations and sync status.",
    },
    HelpEntry {
        key: "gui:mirrors.add_distro",
        title: "Add Mirror — Distro",
        body: "Short name for the distribution (e.g., 'archlinux', 'ubuntu', 'alpine').",
    },
    HelpEntry {
        key: "gui:mirrors.add_upstream",
        title: "Add Mirror — Upstream URL",
        body: "Base URL of the upstream repo to sync from (e.g., https://archive.archlinux.org/repos).",
    },
    HelpEntry {
        key: "gui:mirrors.add_interval",
        title: "Add Mirror — Sync Interval",
        body: "Seconds between automatic sync runs. Default: 3600 (1 hour).",
    },
    HelpEntry {
        key: "gui:mirrors.add_public",
        title: "Add Mirror — Public",
        body: "If checked, this mirror is accessible without authentication. Use for LAN-only or public mirrors.",
    },
    HelpEntry {
        key: "gui:mirrors.sync_now",
        title: "Sync Now",
        body: "Trigger an immediate sync of this mirror from upstream.",
    },
    HelpEntry {
        key: "gui:mirrors.delete",
        title: "Delete",
        body: "Remove this mirror configuration. Does not delete already-downloaded files.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Clients Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:clients.refresh",
        title: "Refresh",
        body: "Reload the client list from the database.",
    },
    HelpEntry {
        key: "gui:clients.hostname",
        title: "Hostname",
        body: "The machine name the client reported during pairing.",
    },
    HelpEntry {
        key: "gui:clients.os",
        title: "OS",
        body: "Operating system the client is running (linux, windows, freebsd).",
    },
    HelpEntry {
        key: "gui:clients.registered",
        title: "Registered",
        body: "When this client first paired with the server.",
    },
    HelpEntry {
        key: "gui:clients.last_seen",
        title: "Last Seen",
        body: "Most recent heartbeat timestamp. If old, the client may be offline.",
    },
    HelpEntry {
        key: "gui:clients.status",
        title: "Status",
        body: "Green = active (heartbeat within timeout). If missing, the client daemon may have stopped.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Monetization Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:monetization.public_metrics",
        title: "Public Metrics",
        body: "Show aggregate download stats publicly. No identifying information is exposed.",
    },
    HelpEntry {
        key: "gui:monetization.lightning",
        title: "Lightning Address",
        body: "A Lightning Network address (user@domain) for receiving tips and boost payments.",
    },
    HelpEntry {
        key: "gui:monetization.bitcoin",
        title: "Bitcoin Address",
        body: "An on-chain BTC address for donations and bounty payouts.",
    },
    HelpEntry {
        key: "gui:monetization.bounty_address",
        title: "Bounty Address",
        body: "Address specifically for bug bounty payouts. Can be same as Bitcoin address.",
    },
    HelpEntry {
        key: "gui:monetization.v4v_enabled",
        title: "V4V Enabled",
        body: "Value-for-value: allow clients to send micro-payments (boosts) when downloading packages.",
    },
    HelpEntry {
        key: "gui:monetization.v4v_default_boost",
        title: "V4V Default Boost",
        body: "Default boost amount in satoshis applied to each download if the client doesn't specify one.",
    },

    // ═══════════════════════════════════════════════════════════════════
    // Part 1 — Tracker Page
    // ═══════════════════════════════════════════════════════════════════

    HelpEntry {
        key: "gui:tracker.refresh",
        title: "Refresh",
        body: "Reload tracker status and peer list.",
    },
    HelpEntry {
        key: "gui:tracker.peers_known",
        title: "Peers Known",
        body: "Number of mirror servers the tracker is aware of.",
    },
    HelpEntry {
        key: "gui:tracker.mirrors_announced",
        title: "Mirrors Announced",
        body: "How many mirrors have announced themselves to this tracker.",
    },
];

/// GET /api/v1/help/:key — return help text for a field or GUI element.
///
/// Keys use namespace prefixes:
/// - `field:package.name`     → recipe TOML field help
/// - `section:install`        → section-level intro text
/// - `gui:cache.purge`        → GUI element tooltip
///
/// Returns 404 with JSON if the key is not found.
pub async fn handle_get_help(Path(key): Path<String>) -> impl IntoResponse {
    match HELP_DATA.iter().find(|e| e.key == key) {
        Some(entry) => {
            let resp = HelpResponse {
                key: entry.key.to_string(),
                title: entry.title.to_string(),
                body: entry.body.to_string(),
            };
            (StatusCode::OK, axum::Json(resp)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": "No help available for this element.",
                "key": key,
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_recipe_field() {
        let entry = HELP_DATA.iter().find(|e| e.key == "field:package.name");
        assert!(entry.is_some());
        assert!(entry.unwrap().body.contains("binary name"));
    }

    #[test]
    fn test_lookup_section() {
        let entry = HELP_DATA.iter().find(|e| e.key == "section:install");
        assert!(entry.is_some());
        assert!(entry.unwrap().body.contains("extract"));
    }

    #[test]
    fn test_lookup_gui_element() {
        let entry = HELP_DATA.iter().find(|e| e.key == "gui:cache.purge");
        assert!(entry.is_some());
        assert!(entry.unwrap().body.contains("re-download"));
    }

    #[test]
    fn test_unknown_key_returns_none() {
        let entry = HELP_DATA.iter().find(|e| e.key == "field:nonexistent");
        assert!(entry.is_none());
    }

    #[test]
    fn test_no_duplicate_keys() {
        let mut keys: Vec<&str> = HELP_DATA.iter().map(|e| e.key).collect();
        keys.sort_unstable();
        for w in keys.windows(2) {
            assert_ne!(w[0], w[1], "Duplicate help key found: {}", w[0]);
        }
    }
}
