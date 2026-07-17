/// Help system API endpoint for dashboard tooltips.
/// Returns field descriptions formatted per Part 4 of help-system-llm-instructions.md.
use axum::{extract::Path, http::StatusCode, response::IntoResponse};
use std::sync::RwLock;

/// Pre-populated field data from Part 2 of the spec, formatted per output format.
const FIELD_HELP_DATA: &[(&str, &str)] = &[
    // ProxyConfig section
    ("listen_address", "### Listen Address\n\nThe IP address and port where the HTTP proxy listens.\n\n**Default:** `127.0.0.1:3128`\n\n**Example:** `0.0.0.0:3128` to listen on all interfaces (not recommended for production).\n\n> **Security note:** Only bind to specific IPs you control. Binding to 0.0.0.0 exposes the proxy to your entire network.\n\n**Related fields:** `tls.enabled`, `listen_address_tls`"),

    ("listen_address_tls", "### TLS Listen Address\n\nThe IP address and port for HTTPS CONNECT tunneling (Docker mirroring).\n\n**Default:** None (disabled)\n\n**Example:** `127.0.0.1:8443`\n\n> **Note:** Requires TLS certificates configured in the `[tls]` section.\n\n**Related fields:** `listen_address`, `tls.cert_path`, `tls.key_path`"),

    ("max_clients", "### Max Clients\n\nMaximum number of concurrent proxy connections before returning 503 Service Unavailable.\n\n**Default:** `100`\n\n**Example:** `200` for larger deployments with many clients.\n\n> **Performance note:** Each client connection holds memory for request/response buffers. Set based on available RAM (estimate ~5MB per active client).\n\n**Related fields:** `connection_timeout_seconds`"),

    ("max_body_size_bytes", "### Max Body Size\n\nMaximum allowed response body size in bytes before truncating and returning 413 Request Entity Too Large.\n\n**Default:** `524288000` (500 MB)\n\n**Example:** `1073741824` for 1 GB limit (large package repositories).\n\n> **Security note:** Limits prevent disk space exhaustion from malicious oversized responses.\n\n**Related fields:** `bandwidth_throttle_bytes_per_second`"),

    ("connection_timeout_seconds", "### Connection Timeout\n\nTimeout in seconds for client connections. If no data received within this window, the connection is closed.\n\n**Default:** `60`\n\n**Example:** `120` for slow networks or large package downloads.\n\n> **Note:** Longer timeouts may hold resources longer. Balance between reliability and resource usage.\n\n**Related fields:** `max_clients`, `prefetch.request_timeout_seconds`"),

    ("rate_limit_requests_per_second", "### Rate Limit Requests/sec\n\nPer-IP request rate limit for the proxy endpoint. Prevents abuse and ensures fair sharing among clients.\n\n**Default:** `100`\n\n**Example:** `50` for constrained environments, `200` for high-performance setups.\n\n> **Note:** Rate limiting uses a token-bucket algorithm with burst allowance of 1.3x the rate limit.\n\n**Related fields:** `bandwidth_throttle_bytes_per_second`, `rate_limit_requests_per_minute`"),

    ("rate_limit_requests_per_minute", "### Rate Limit Requests/min\n\nPer-IP request count limit per minute window. Complements requests/second for burst control.\n\n**Default:** `1000`\n\n**Example:** `2000` for high-throughput clients.\n\n> **Note:** Both rate limits must be exceeded to trigger a 429 Too Many Requests response.\n\n**Related fields:** `rate_limit_requests_per_second`, `bandwidth_throttle_bytes_per_second`"),

    ("bandwidth_throttle_bytes_per_second", "### Bandwidth Throttle (bytes/sec)\n\nMaximum bandwidth per client connection in bytes per second. Prevents a single client from saturating the upstream link.\n\n**Default:** `0` (unlimited)\n\n**Example:** `10485760` for 10 MB/s limit, `5242880` for 5 MB/s.\n\n> **Use case:** When sharing bandwidth with other services. Set based on available upstream capacity divided by max_clients.\n\n**Related fields:** `rate_limit_requests_per_second`, `max_body_size_bytes`"),

    ("trust_proxy_headers", "### Trust Proxy Headers\n\nWhether to trust X-Forwarded-For, X-Real-IP, and similar headers from trusted sources. When true, these headers override the actual connection IP for rate limiting and logging.\n\n**Default:** `false`\n\n**Example:** `true` when behind a load balancer that sets these headers.\n\n> **Security warning:** Only enable if you control all traffic between your proxy and upstream clients. Trusting untrusted sources allows IP spoofing.\n\n**Related fields:** `trust_proxy_sources`, `rate_limit_requests_per_second`"),

    ("trust_proxy_sources", "### Trust Proxy Sources\n\nList of CIDR ranges from which proxy headers (X-Forwarded-For, etc.) are trusted. Only applies when `trust_proxy_headers = true`.\n\n**Default:** None\n\n**Example:** `[\"10.0.0.0/8\", \"172.16.0.0/12\"]`\n\n> **Note:** If empty, all sources are trusted (not recommended). Specify explicit ranges for security.\n\n**Related fields:** `trust_proxy_headers`, `listen_address`"),

    // TelemetryConfig section
    ("telemetry_enabled", "### Telemetry Enabled\n\nWhether to collect anonymous usage telemetry. When enabled, the server periodically sends:\n- Installation timestamp (one-time)\n- Package manager types used\n- Cache hit/miss statistics (aggregated, not per-client)\n\n**Default:** `false`\n\n**Example:** `true` to contribute data for improving monopsony.\n\n> **Privacy note:** No client inventory details, package names, or user data is collected. Only aggregate usage patterns.\n\n**Related fields:** `telemetry_install_tracking`, `telemetry_interval_hours`"),

    ("telemetry_install_tracking", "### Telemetry Install Tracking\n\nWhether to track when monopsony is installed (one-time event). Used for adoption statistics only.\n\n**Default:** `true`\n\n**Example:** `false` if you want to disable install tracking specifically while keeping other telemetry on.\n\n> **Note:** This field has no effect if `telemetry_enabled = false`.\n\n**Related fields:** `telemetry_enabled`, `telemetry_install_source`"),

    ("telemetry_interval_hours", "### Telemetry Interval (hours)\n\nHow often to send telemetry data in hours. Set to 0 to disable periodic sending.\n\n**Default:** `24`\n\n**Example:** `168` for weekly reports, `0` to fully disable (keep only install tracking).\n\n> **Note:** Telemetry is sent over HTTPS to a configurable endpoint. No client-specific data transmitted.\n\n**Related fields:** `telemetry_enabled`, `telemetry_endpoint_url`"),

    ("telemetry_install_source", "### Telemetry Install Source\n\nWhere monopsony was installed from. Used for adoption analytics only.\n\n**Default:** `unknown`\n\n**Example values:** `aur`, `github-release`, `source-build`, `docker-hub`, `manual-install`\n\n> **Note:** This field is set automatically during installation when possible. Manual override allowed.\n\n**Related fields:** `telemetry_enabled`, `telemetry_install_tracking`"),

    ("telemetry_endpoint_url", "### Telemetry Endpoint URL\n\nHTTPS URL where telemetry data is sent. Set to empty string to disable periodic telemetry (install tracking still works if enabled).\n\n**Default:** `https://telemetry.monopsony.dev/collect`\n\n**Example:** `https://your-instance.example.com/telemetry` for self-hosted collection.\n\n> **Security note:** Must be HTTPS. HTTP endpoints are rejected for security reasons.\n\n**Related fields:** `telemetry_enabled`, `telemetry_interval_hours`"),

    // DashboardConfig section
    ("dashboard_listen_address", "### Dashboard Listen Address\n\nThe IP address and port where the SvelteKit dashboard API serves.\n\n**Default:** `127.0.0.1:3130`\n\n**Example:** `0.0.0.0:3130` to expose on all interfaces (use with caution).\n\n> **Security note:** The dashboard has tiered auth — reads optional in LAN/serve mode, writes always require login.\n\n**Related fields:** `dashboard_auth_mode`, `listen_address`"),

    ("dashboard_auth_mode", "### Dashboard Auth Mode\n\nAuthentication requirement for dashboard access:\n- **funnel**: Login required for ALL operations (default for direct LAN)\n- **serve**: Auto-trust Tailscale clients in serve mode, login required for writes\n- **lan**: Trust all private IPs, login only for config writes\n\n**Default:** `funnel`\n\n**Example values:** `serve`, `lan`, `funnel`\n\n> **Note:** Detection uses /var/run/tailscale/tailscaled.sock when available. Falls back to manual configuration.\n\n**Related fields:** `dashboard_listen_address`, `admin_username`"),

    ("dashboard_session_timeout_hours", "### Dashboard Session Timeout (hours)\n\nHow long a dashboard session remains valid before requiring re-authentication.\n\n**Default:** `24`\n\n**Example:** `8` for work hours only, `168` for weekly sessions.\n\n> **Security note:** Longer sessions increase risk if a device is compromised. Balance convenience with security.\n\n**Related fields:** `dashboard_auth_mode`, `admin_username`"),

    ("admin_username", "### Admin Username\n\nUsername for the dashboard admin account.\n\n**Default:** `admin`\n\n**Example:** `monopsony-admin` or your preferred username.\n\n> **Security note:** Change from default immediately after initial setup. Combined with admin_password for authentication.\n\n**Related fields:** `admin_password`, `dashboard_auth_mode`"),

    ("admin_password", "### Admin Password\n\nPassword for the dashboard admin account.\n\n**Default:** None (must be set)\n\n> **Security note:** Use a strong password. Minimum 8 characters recommended. Consider using a password manager to generate one.\n\n**Related fields:** `admin_username`, `dashboard_auth_mode`"),

    // ShadowConfig section
    ("shadow_enabled", "### Shadow Enabled\n\nWhether the shadow self-update mechanism is active. When enabled, monopsony can update itself from configured recipes with health checks and automatic rollback on failure.\n\n**Default:** `false`\n\n**Example:** `true` to enable automated updates.\n\n> **Note:** Requires at least one recipe configured in `[shadow]` section below.\n\n**Related fields:** `shadow_recipes`, `shadow_update_interval_hours`"),

    ("shadow_update_interval_hours", "### Shadow Update Interval (hours)\n\nHow often to check for new versions across all shadow recipes. Set to 0 to disable periodic checks.\n\n**Default:** `24`\n\n**Example:** `6` for more frequent updates, `168` for weekly checks.\n\n> **Note:** Updates are applied during the maintenance window defined by `shadow_maintenance_window_start/end` if configured.\n\n**Related fields:** `shadow_enabled`, `shadow_recipes`"),

    ("shadow_max_concurrent_updates", "### Shadow Max Concurrent Updates\n\nMaximum number of parallel self-update operations. Prevents resource exhaustion when multiple recipes trigger updates simultaneously.\n\n**Default:** `1`\n\n**Example:** `3` for faster multi-recipe updates (requires more disk I/O).\n\n> **Warning:** Setting too high may cause disk thrashing or memory pressure during updates.\n\n**Related fields:** `shadow_enabled`, `shadow_recipes`"),

    ("shadow_health_check_timeout_seconds", "### Shadow Health Check Timeout (seconds)\n\nHow long to wait for a service to become healthy after applying an update before rolling back.\n\n**Default:** `30`\n\n**Example:** `60` for services with longer startup times.\n\n> **Note:** If the health check fails within this window, the update is automatically rolled back via `on_failure` hooks.\n\n**Related fields:** `shadow_enabled`, `shadow_recipes`"),

    ("shadow_maintenance_window_start", "### Shadow Maintenance Window Start\n\nEarliest time (24h format) when updates can be applied. Outside this window, updates are queued but not started.\n\n**Default:** None (no restriction)\n\n**Example:** `\"02:00\"` for 2 AM maintenance window.\n\n> **Use case:** Apply updates during off-hours to minimize impact on clients.\n\n**Related fields:** `shadow_maintenance_window_end`, `shadow_update_interval_hours`"),

    ("shadow_maintenance_window_end", "### Shadow Maintenance Window End\n\nLatest time (24h format) when updates can be applied. Updates started before this time must complete by then, or they're rolled back.\n\n**Default:** None\n\n**Example:** `\"05:00\"` for a 3-hour maintenance window starting at 2 AM.\n\n> **Note:** If `shadow_maintenance_window_start` is set but end is not, updates have no deadline (not recommended).\n\n**Related fields:** `shadow_maintenance_window_start`, `shadow_enabled`"),

    ("shadow_recipes", "### Shadow Recipes\n\nList of update recipes for automated self-updates. Each recipe defines a source URL and optional health check.\n\n**Default:** Empty list\n\n**Example:**\n```toml\n[[shadow.recipes]]\nname = \"latest-release\"\ntype = \"url\"\nurl = \"https://github.com/societus/monopsony/releases/latest/download/monopsony-{{VERSION}}-x86_64-linux\"\nhealth_check_port = 3128\n```\n\n> **Note:** Each recipe can have pre_update, post_update, and on_failure hooks for custom logic.\n\n**Related fields:** `shadow_enabled`, `shadow_max_concurrent_updates`"),

    ("telemetry_enabled", "### Telemetry Enabled (Duplicate Key)\n\n⚠️ **Duplicate field detected.** This key appears twice in the configuration. The last definition wins.\n\nCheck your server.toml for duplicate `[telemetry]` sections or copy-paste errors."),
];

/// Get help text for a specific field from Part 2 data.
pub fn get_field_help(field: &str) -> Option<String> {
    FIELD_HELP_DATA
        .iter()
        .find(|(key, _)| *key == field)
        .map(|(_, desc)| desc.to_string())
}

/// Get all available fields for the help system.
pub fn get_available_fields() -> Vec<&'static str> {
    FIELD_HELP_DATA.iter().map(|(key, _)| *key).collect()
}
