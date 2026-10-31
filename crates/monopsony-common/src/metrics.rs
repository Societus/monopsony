//! Metrics definitions for Monopsony

use metrics::{counter, gauge, histogram};

/// Metric names for server
pub mod server {
    pub const CACHE_REQUESTS_TOTAL: &str = "monopsony_cache_requests_total";
    pub const CACHE_BYTES_SERVED: &str = "monopsony_cache_bytes_served_total";
    pub const CACHE_BYTES_CACHED: &str = "monopsony_cache_bytes_cached_total";
    /// Total bytes relayed through HTTP CONNECT tunnels (client + upstream)
    pub const PROXY_CONNECT_BYTES: &str = "monopsony_proxy_connect_bytes_total";
    pub const CACHE_ENTRIES: &str = "monopsony_cache_entries_current";
    pub const CLIENTS_CONNECTED: &str = "monopsony_clients_connected_current";
    pub const CLIENTS_REGISTERED: &str = "monopsony_clients_registered_total";
    pub const PACKAGES_CACHED: &str = "monopsony_packages_cached_current";
    pub const PACKAGES_PREFETCHED: &str = "monopsony_packages_prefetched_total";
    pub const PACKAGES_PURGED: &str = "monopsony_packages_purged_total";
    /// Sprint 23 Phase C: language-artifact retention/scrubber counters.
    pub const CACHE_EVICTED_TOTAL: &str = "monopsony_cache_evicted_total";
    pub const SCRUB_VERIFIED_TOTAL: &str = "monopsony_scrub_verified_total";
    pub const SCRUB_QUARANTINED_TOTAL: &str = "monopsony_scrub_quarantined_total";
    pub const GRPC_REQUESTS_TOTAL: &str = "monopsony_grpc_requests_total";
    pub const GRPC_REQUEST_DURATION: &str = "monopsony_grpc_request_duration_seconds";
    /// Sprint 27 Part A: seconds since a client's last heartbeat (per client).
    pub const CLIENT_HEARTBEAT_AGE: &str = "monopsony_client_last_heartbeat_age_seconds";
    /// Sprint 27 Part A: episodes where a client crossed the staleness
    /// threshold (bumped once per fresh→stale transition, not per sweep).
    pub const CLIENTS_STALE_TOTAL: &str = "monopsony_clients_stale_total";
    /// Sprint 27 Part A: how many clients are currently past the threshold.
    pub const CLIENTS_STALE_CURRENT: &str = "monopsony_clients_stale_current";
    /// Sprint 27 Part B: windowed update-failure count per client (counts-only).
    pub const CLIENT_FAILED_UPDATES: &str = "monopsony_client_failed_updates";
    /// Sprint 27 Part B: seconds since the client's last update attempt.
    pub const CLIENT_UPDATE_ATTEMPT_AGE: &str = "monopsony_client_last_update_attempt_age_seconds";
}

/// Metric names for mirror subsystem
pub mod mirror {
    pub const SYNC_TOTAL: &str = "monopsony_mirror_sync_total";
    pub const SYNC_DURATION: &str = "monopsony_mirror_sync_duration_seconds";
    pub const SYNC_BYTES: &str = "monopsony_mirror_sync_bytes_total";
    pub const SYNC_FILES: &str = "monopsony_mirror_sync_files_total";
    pub const SERVED_TOTAL: &str = "monopsony_mirror_served_total";
    pub const SERVED_BYTES: &str = "monopsony_mirror_served_bytes_total";
    /// Sync-integrity outcomes: "pass" | "mismatch_fail_closed" |
    /// "mismatch_fail_open" | "no_checksum".
    pub const HASH_VERIFY_TOTAL: &str = "monopsony_mirror_hash_verify_total";
    /// Metadata signature outcomes: "pass" | "fail" | "fail_open" |
    /// "no_keys" | "unsupported".
    pub const SIGNATURE_VERIFY_TOTAL: &str = "monopsony_mirror_signature_verify_total";
    /// Cross-mirror comparison outcomes: "match" | "divergence" |
    /// "unreachable" | "skipped".
    pub const CROSS_CHECK_TOTAL: &str = "monopsony_mirror_cross_check_total";
}

/// Metric names for client
pub mod client {
    pub const PROXY_REQUESTS_TOTAL: &str = "monopsony_client_proxy_requests_total";
    pub const BYTES_SAVED: &str = "monopsony_client_bytes_saved_total";
    pub const GRPC_CONNECTIONS: &str = "monopsony_client_grpc_connections_total";
}

/// Metric names for scan subsystem
pub mod scan {
    pub const SCAN_TOTAL: &str = "monopsony_scan_total";
    pub const CVE_LOOKUPS_TOTAL: &str = "monopsony_scan_cve_lookup_total";
    pub const POLICY_DECISIONS_TOTAL: &str = "monopsony_scan_policy_decision_total";
}

/// Increment cache request counter
pub fn cache_request_hit() {
    counter!(server::CACHE_REQUESTS_TOTAL, "result" => "hit").increment(1);
}

pub fn cache_request_miss() {
    counter!(server::CACHE_REQUESTS_TOTAL, "result" => "miss").increment(1);
}

/// Record bytes served from cache
pub fn cache_bytes_served(bytes: u64) {
    counter!(server::CACHE_BYTES_SERVED).increment(bytes);
}

/// Record bytes cached
pub fn cache_bytes_cached(bytes: u64) {
    counter!(server::CACHE_BYTES_CACHED).increment(bytes);
}

/// Sprint 23 Phase C: record one retention eviction (blob deleted over budget).
pub fn cache_evicted() {
    counter!(server::CACHE_EVICTED_TOTAL).increment(1);
}

/// Sprint 23 Phase C: record scrubber verification outcome.
pub fn scrub_verified(count: u64) {
    counter!(server::SCRUB_VERIFIED_TOTAL).increment(count);
}

pub fn scrub_quarantined(count: u64) {
    counter!(server::SCRUB_QUARANTINED_TOTAL).increment(count);
}

/// Record bytes relayed through HTTP CONNECT tunnels
pub fn proxy_connect_bytes(bytes: u64) {
    counter!(server::PROXY_CONNECT_BYTES).increment(bytes);
}

/// Set current cache entry count
pub fn set_cache_entries(count: u64) {
    gauge!(server::CACHE_ENTRIES).set(count as f64);
}

/// Set connected clients count
pub fn set_clients_connected(count: u64) {
    gauge!(server::CLIENTS_CONNECTED).set(count as f64);
}

/// Increment registered clients
pub fn client_registered() {
    counter!(server::CLIENTS_REGISTERED).increment(1);
}

/// Sprint 27 Part B: set per-client windowed update-failure count (gauge).
pub fn set_client_failed_updates(client_id: &str, count: u32) {
    gauge!(server::CLIENT_FAILED_UPDATES, "client_id" => client_id.to_string()).set(count as f64);
}

/// Sprint 27 Part B: set seconds since a client's last update attempt.
/// Callers must skip this when the client reported no attempt (ts == 0).
pub fn set_client_update_attempt_age(client_id: &str, age_secs: u64) {
    gauge!(server::CLIENT_UPDATE_ATTEMPT_AGE, "client_id" => client_id.to_string())
        .set(age_secs as f64);
}

/// Sprint 27 Part A: set per-client heartbeat age gauge (seconds since last_seen).
pub fn set_client_heartbeat_age(client_id: &str, age_secs: u64) {
    gauge!(server::CLIENT_HEARTBEAT_AGE, "client_id" => client_id.to_string()).set(age_secs as f64);
}

/// Sprint 27 Part A: record one fresh→stale transition episode.
pub fn client_stale_episode() {
    counter!(server::CLIENTS_STALE_TOTAL).increment(1);
}

/// Sprint 27 Part A: set current count of clients past the staleness threshold.
pub fn set_clients_stale(count: u64) {
    gauge!(server::CLIENTS_STALE_CURRENT).set(count as f64);
}

/// Set cached packages count
pub fn set_packages_cached(manager: &'static str, count: u64) {
    gauge!(server::PACKAGES_CACHED, "manager" => manager).set(count as f64);
}

/// Increment prefetched packages
pub fn package_prefetched(manager: &'static str) {
    counter!(server::PACKAGES_PREFETCHED, "manager" => manager).increment(1);
}

/// Increment purged packages
pub fn package_purged(manager: &'static str) {
    counter!(server::PACKAGES_PURGED, "manager" => manager).increment(1);
}

/// Record gRPC request
pub fn grpc_request(method: &'static str) {
    counter!(server::GRPC_REQUESTS_TOTAL, "method" => method).increment(1);
}

/// Record gRPC request duration
pub fn grpc_request_duration(method: &'static str, duration_secs: f64) {
    histogram!(server::GRPC_REQUEST_DURATION, "method" => method).record(duration_secs);
}

// Client metrics

/// Record client proxy request
pub fn client_proxy_request(result: &'static str) {
    counter!(client::PROXY_REQUESTS_TOTAL, "result" => result).increment(1);
}

/// Record bytes saved by using cache
pub fn client_bytes_saved(bytes: u64) {
    counter!(client::BYTES_SAVED).increment(bytes);
}

/// Record gRPC connection attempt
pub fn client_grpc_connection(success: bool) {
    let result = if success { "success" } else { "failure" };
    counter!(client::GRPC_CONNECTIONS, "result" => result).increment(1);
}

// Mirror metrics

/// Record mirror sync completion
pub fn mirror_sync(distro: &str, result: &'static str) {
    counter!(mirror::SYNC_TOTAL, "distro" => distro.to_string(), "result" => result).increment(1);
}

/// Record mirror sync duration
pub fn mirror_sync_duration(distro: &str, duration_secs: f64) {
    histogram!(mirror::SYNC_DURATION, "distro" => distro.to_string()).record(duration_secs);
}

/// Record mirror sync bytes transferred
pub fn mirror_sync_bytes(distro: &str, bytes: u64) {
    counter!(mirror::SYNC_BYTES, "distro" => distro.to_string()).increment(bytes);
}

/// Record mirror sync files transferred
pub fn mirror_sync_files(distro: &str, count: u64) {
    counter!(mirror::SYNC_FILES, "distro" => distro.to_string()).increment(count);
}

/// Record mirror file served
pub fn mirror_served(distro: &str) {
    counter!(mirror::SERVED_TOTAL, "distro" => distro.to_string()).increment(1);
}

/// Record mirror bytes served
pub fn mirror_served_bytes(distro: &str, bytes: u64) {
    counter!(mirror::SERVED_BYTES, "distro" => distro.to_string()).increment(bytes);
}

/// Record one hash-verification outcome (sync or serve path).
pub fn mirror_hash_verify(distro: &str, result: &'static str) {
    counter!(mirror::HASH_VERIFY_TOTAL, "distro" => distro.to_string(), "result" => result)
        .increment(1);
}

/// Record one metadata signature-verification outcome.
pub fn mirror_signature_verify(distro: &str, result: &'static str) {
    counter!(mirror::SIGNATURE_VERIFY_TOTAL, "distro" => distro.to_string(), "result" => result)
        .increment(1);
}

/// Record one cross-mirror hash-comparison outcome.
pub fn mirror_cross_check(distro: &str, result: &'static str) {
    counter!(mirror::CROSS_CHECK_TOTAL, "distro" => distro.to_string(), "result" => result)
        .increment(1);
}

// Scan metrics

/// Record scan outcome
pub fn scan_outcome(status: &str) {
    counter!(scan::SCAN_TOTAL, "status" => status.to_string()).increment(1);
}

/// Record CVE lookup
pub fn scan_cve_lookup(cve_count: usize) {
    counter!(scan::CVE_LOOKUPS_TOTAL).increment(1);
    if cve_count > 0 {
        counter!(scan::CVE_LOOKUPS_TOTAL, "result" => "found").increment(1);
    }
}

/// Record policy decision
pub fn scan_policy_decision(decision: &str) {
    counter!(scan::POLICY_DECISIONS_TOTAL, "decision" => decision.to_string()).increment(1);
}
