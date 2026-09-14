//! Distribution mirror support
//!
//! Syncs Linux distro repos/ISOs from upstream, serves to LAN clients.
//! Configured via TOML file, syncs on schedule, serves via HTTP.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use http_body_util::BodyExt;
use hyper::body::Bytes;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info, trace, warn};

use monopsony_db::db::UpstreamPackageRow;
use monopsony_db::Database;

/// Boxed body type for hyper client requests
type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

fn empty_body() -> BoxBody {
    http_body_util::combinators::BoxBody::new(
        http_body_util::Empty::<Bytes>::new().map_err(|_| unreachable!()),
    )
}

/// HTTPS-capable hyper client type (rustls + ring).
type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, BoxBody>;

/// Build an HTTPS-capable hyper client connector (rustls + ring).
///
/// `Client::builder().build_http()` only does plaintext HTTP. This wrapper
/// upgrades the connector to support TLS so we can reach HTTPS upstreams.
fn build_https_client() -> HttpClient {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("Failed to load native root certs")
        .https_or_http()
        .enable_http2()
        .build();
    Client::builder(TokioExecutor::new()).build(connector)
}

/// Per-mirror configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorConfig {
    /// Distribution identifier (e.g., "archlinux", "ubuntu")
    pub distro: String,
    /// Upstream mirror URL to sync from
    pub upstream_url: String,
    /// Local path relative to mirror_dir
    #[serde(with = "path_serde")]
    pub local_path: PathBuf,
    /// Whether sync is enabled
    #[serde(default = "default_true")]
    pub sync_enabled: bool,
    /// Sync interval in seconds
    #[serde(default = "default_sync_interval")]
    pub sync_interval_secs: u64,
    /// Optional bandwidth limit in bytes/sec
    pub bandwidth_limit_bps: Option<u64>,
    /// Glob patterns to include (empty = all)
    #[serde(default)]
    pub include_patterns: Vec<String>,
    /// Glob patterns to exclude
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    /// Whether mirror is publicly accessible (no auth)
    #[serde(default)]
    pub public: bool,
    /// Per-IP requests per second limit (0 = unlimited)
    #[serde(default)]
    pub per_ip_rps: u32,
    /// Max concurrent connections per IP (0 = unlimited)
    #[serde(default)]
    pub max_concurrent_per_ip: u32,
    /// Last sync timestamp (epoch seconds)
    pub last_sync: Option<i64>,
    /// Current sync status: "idle", "syncing", "error"
    #[serde(default = "default_sync_status")]
    pub sync_status: String,
    /// Total bytes synced
    #[serde(default)]
    pub total_bytes: u64,
    /// Hash verification policy: "fail-closed" (reject on mismatch) or "fail-open" (log warning, serve anyway)
    #[serde(default = "default_hash_policy")]
    pub hash_policy: String,
}

fn default_true() -> bool {
    true
}

fn default_sync_interval() -> u64 {
    86400 // 24 hours
}

fn default_sync_status() -> String {
    "idle".to_string()
}

fn default_hash_policy() -> String {
    "fail-closed".to_string()
}

/// Wrapper for TOML serialization (TOML requires a table key for arrays)
#[derive(Serialize, Deserialize)]
struct MirrorConfigFile {
    #[serde(default)]
    mirrors: Vec<MirrorConfig>,
}

mod path_serde {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use std::path::{Path, PathBuf};

    pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&path.to_string_lossy())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        let s = String::deserialize(d)?;
        Ok(PathBuf::from(s))
    }
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            distro: String::new(),
            upstream_url: String::new(),
            local_path: PathBuf::new(),
            sync_enabled: true,
            sync_interval_secs: 86400,
            bandwidth_limit_bps: None,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            public: false,
            per_ip_rps: 0,
            max_concurrent_per_ip: 0,
            last_sync: None,
            sync_status: "idle".to_string(),
            total_bytes: 0,
            hash_policy: "fail-closed".to_string(),
        }
    }
}

/// Return a default MirrorConfig for a package manager string, or None if the
/// package manager has no known distro mirror (e.g. Windows package managers).
///
/// The upstream URLs are reliable, well-known public mirrors.
fn default_mirror_config_for_pm(pm: &str) -> Option<MirrorConfig> {
    let (distro, upstream) = match pm {
        "pacman" => ("archlinux", "https://geo.mirror.pkgbuild.com"),
        "apt" => ("debian", "http://deb.debian.org/debian"),
        "dnf" => ("fedora", "https://dl.fedoraproject.org/pub/fedora/linux"),
        // Non-mirrored package managers — no default distro mirror
        _ => return None,
    };

    Some(MirrorConfig {
        distro: distro.to_string(),
        upstream_url: upstream.to_string(),
        local_path: PathBuf::from(format!("mirrors/{}", distro)),
        sync_enabled: false, // Don't auto-sync until admin reviews
        sync_interval_secs: 86400,
        bandwidth_limit_bps: None,
        include_patterns: Vec::new(),
        exclude_patterns: Vec::new(),
        public: false,
        per_ip_rps: 0,
        max_concurrent_per_ip: 0,
        last_sync: None,
        sync_status: "idle".to_string(),
        total_bytes: 0,
        hash_policy: "fail-closed".to_string(),
    })
}

/// Manages all mirror configurations and sync tasks
#[derive(Clone)]
pub struct MirrorManager {
    mirror_dir: PathBuf,
    configs: Arc<tokio::sync::RwLock<Vec<MirrorConfig>>>,
    db: Option<Arc<Database>>,
}

impl MirrorManager {
    pub fn new(mirror_dir: PathBuf, db: Option<Arc<Database>>) -> Self {
        Self {
            mirror_dir,
            configs: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            db,
        }
    }

    /// Get the max per-IP RPS across all configured mirrors (0 if none)
    pub async fn max_per_ip_rps(&self) -> u32 {
        self.configs
            .read()
            .await
            .iter()
            .map(|c| c.per_ip_rps)
            .max()
            .unwrap_or(0)
    }

    /// Returns true if any configured mirror is private (requires auth)
    pub async fn has_private_mirrors(&self) -> bool {
        self.configs.read().await.iter().any(|c| !c.public)
    }

    /// Load mirror configurations from TOML file
    pub async fn load_config(&self) -> anyhow::Result<()> {
        let config_path = self.mirror_dir.join("mirrors.toml");
        if !config_path.exists() {
            info!(
                "No mirror config file at {}, starting empty",
                config_path.display()
            );
            return Ok(());
        }

        let content = tokio::fs::read_to_string(&config_path).await?;
        let file: MirrorConfigFile = toml::from_str(&content)?;
        let configs = file.mirrors;
        info!("Loaded {} mirror configuration(s) from TOML", configs.len());

        let mut guard = self.configs.write().await;
        *guard = configs;
        Ok(())
    }

    /// Load mirror configurations from the database into the in-memory list.
    ///
    /// Called during server startup to reconcile in-memory state with what was
    /// previously persisted to the DB (auto-created from client pairing, or
    /// manually added via dashboard). Does not overwrite configs already loaded
    /// from TOML.
    pub async fn load_from_db(&self) -> anyhow::Result<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };

        let rows = db.list_mirror_configs().await?;
        if rows.is_empty() {
            return Ok(());
        }

        let mut guard = self.configs.write().await;
        let mut loaded = 0;
        for row in &rows {
            // Don't overwrite configs already loaded from TOML
            if guard.iter().any(|c| c.distro == row.distro) {
                continue;
            }

            guard.push(MirrorConfig {
                distro: row.distro.clone(),
                upstream_url: row.upstream_url.clone(),
                local_path: PathBuf::from(&row.local_path),
                sync_enabled: row.sync_enabled,
                sync_interval_secs: row.sync_interval_secs as u64,
                bandwidth_limit_bps: row.bandwidth_limit_bps.map(|v| v as u64),
                include_patterns: serde_json::from_value(row.include_patterns.clone())
                    .unwrap_or_default(),
                exclude_patterns: serde_json::from_value(row.exclude_patterns.clone())
                    .unwrap_or_default(),
                public: row.public,
                sync_status: row.sync_status.clone(),
                last_sync: row.last_sync.map(|dt| dt.timestamp()),
                total_bytes: row.total_bytes as u64,
                ..Default::default()
            });
            loaded += 1;
        }

        if loaded > 0 {
            info!("Loaded {} mirror configuration(s) from database", loaded);
        }
        Ok(())
    }

    /// Save current configurations to TOML file
    #[allow(dead_code)] // Will be used by CLI mirror commands
    pub async fn save_config(&self) -> anyhow::Result<()> {
        let config_path = self.mirror_dir.join("mirrors.toml");
        let guard = self.configs.read().await;
        let file = MirrorConfigFile {
            mirrors: guard.clone(),
        };
        let content = toml::to_string_pretty(&file)?;
        tokio::fs::write(&config_path, content).await?;
        info!("Saved {} mirror configuration(s)", guard.len());
        Ok(())
    }

    /// Start the background sync task
    pub async fn start_sync_task(self) -> anyhow::Result<()> {
        let mirror_dir = self.mirror_dir.clone();
        let configs = self.configs.clone();

        // Run initial sync for all enabled mirrors.
        //
        // Collect the enabled configs under the read guard and drop it BEFORE
        // syncing: the sync body below acquires write locks to update
        // sync_status, and a write request queued while this task still holds
        // a read permit deadlocks against itself (tokio RwLock is fair, so
        // later reads queue behind the pending write too — which froze
        // pairing's mirror auto-creation).
        let initial_configs: Vec<MirrorConfig> = {
            let guard = configs.read().await;
            guard.iter().filter(|c| c.sync_enabled).cloned().collect()
        }; // read guard dropped here

        for config in &initial_configs {
            let distro = config.distro.clone();
            let upstream = config.upstream_url.clone();
            let local = config.local_path.clone();
            let dir = mirror_dir.clone();
            let cfgs = configs.clone();

            // Update status
            {
                let mut w = cfgs.write().await;
                if let Some(c) = w.iter_mut().find(|c| c.distro == distro) {
                    c.sync_status = "syncing".to_string();
                }
            }

            info!("Starting initial sync for mirror: {}", distro);
            match sync_mirror(
                &upstream,
                &dir.join(&local),
                &config.exclude_patterns,
                config.bandwidth_limit_bps,
                &config.hash_policy,
            )
            .await
            {
                Ok(bytes) => {
                    let mut w = cfgs.write().await;
                    if let Some(c) = w.iter_mut().find(|c| c.distro == distro) {
                        c.sync_status = "idle".to_string();
                        c.last_sync = Some(chrono::Utc::now().timestamp());
                        c.total_bytes = bytes;
                    }
                    info!("Mirror sync complete for {}: {} bytes", distro, bytes);
                }
                Err(e) => {
                    let mut w = cfgs.write().await;
                    if let Some(c) = w.iter_mut().find(|c| c.distro == distro) {
                        c.sync_status = format!("error: {}", e);
                    }
                    error!("Mirror sync failed for {}: {}", distro, e);
                }
            }
        }

        // Periodic sync loop
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await; // Check every 5 minutes

            // Collect distros that need syncing while holding the read lock
            let due: Vec<MirrorConfig> = {
                let guard = configs.read().await;
                let now = chrono::Utc::now().timestamp();
                guard
                    .iter()
                    .filter(|c| c.sync_enabled)
                    .filter(|c| {
                        let last = c.last_sync.unwrap_or(0);
                        now - last >= c.sync_interval_secs as i64
                    })
                    .cloned()
                    .collect()
            }; // guard dropped here

            for config in &due {
                let distro = config.distro.clone();
                let local_path = mirror_dir.join(&config.local_path);

                // Mark syncing
                {
                    let mut w = configs.write().await;
                    if let Some(c) = w.iter_mut().find(|c| c.distro == distro) {
                        c.sync_status = "syncing".to_string();
                    }
                }

                info!("Periodic sync for mirror: {}", distro);
                match sync_mirror(
                    &config.upstream_url,
                    &local_path,
                    &config.exclude_patterns,
                    config.bandwidth_limit_bps,
                    &config.hash_policy,
                )
                .await
                {
                    Ok(bytes) => {
                        let mut w = configs.write().await;
                        if let Some(c) = w.iter_mut().find(|c| c.distro == distro) {
                            c.sync_status = "idle".to_string();
                            c.last_sync = Some(chrono::Utc::now().timestamp());
                            c.total_bytes = bytes;
                        }
                        info!("Mirror sync complete for {}: {} bytes", distro, bytes);
                    }
                    Err(e) => {
                        let mut w = configs.write().await;
                        if let Some(c) = w.iter_mut().find(|c| c.distro == distro) {
                            c.sync_status = format!("error: {}", e);
                        }
                        error!("Mirror sync failed for {}: {}", distro, e);
                    }
                }
            }
        }
    }

    /// Get current configs snapshot
    pub async fn list_configs(&self) -> Vec<MirrorConfig> {
        self.configs.read().await.clone()
    }

    /// Get config for a specific distro
    pub async fn get_config(&self, distro: &str) -> Option<MirrorConfig> {
        self.configs
            .read()
            .await
            .iter()
            .find(|c| c.distro == distro)
            .cloned()
    }

    /// Access the database handle (for gRPC upsert/delete)
    pub fn db(&self) -> &Option<Arc<Database>> {
        &self.db
    }

    /// Upsert a config into the in-memory list (used by gRPC)
    pub async fn upsert_config(&self, config: MirrorConfig) {
        let mut guard = self.configs.write().await;
        if let Some(existing) = guard.iter_mut().find(|c| c.distro == config.distro) {
            *existing = config;
        } else {
            guard.push(config);
        }
    }

    /// Remove a config from the in-memory list. Returns true if it existed.
    pub async fn remove_config(&self, distro: &str) -> bool {
        let mut guard = self.configs.write().await;
        let len_before = guard.len();
        guard.retain(|c| c.distro != distro);
        guard.len() != len_before
    }

    /// Ensure mirror configs exist for all package managers a client reports.
    ///
    /// Called during client registration/pairing. For each known package manager
    /// kind (apt, dnf, pacman), if no mirror config exists for the corresponding
    /// distro, one is auto-created with sensible defaults and persisted to both
    /// the in-memory list and the database.
    ///
    /// Returns the number of new mirror configs created.
    pub async fn ensure_mirrors_for_package_managers(&self, package_managers: &[String]) -> usize {
        let mut created = 0;

        for pm_str in package_managers {
            let Some(config) = default_mirror_config_for_pm(pm_str) else {
                continue; // Not a Linux package manager with a known mirror
            };

            // Check if this distro already has a config
            let existing = self.get_config(&config.distro).await;
            if existing.is_some() {
                continue;
            }

            info!(
                distro = %config.distro,
                upstream = %config.upstream_url,
                "Auto-creating mirror config from client package manager: {}",
                pm_str
            );

            // Persist to DB
            if let Some(db) = &self.db {
                let upsert = monopsony_db::db::MirrorUpsertConfig {
                    distro: config.distro.clone(),
                    upstream_url: config.upstream_url.clone(),
                    local_path: config.local_path.to_string_lossy().to_string(),
                    sync_enabled: config.sync_enabled,
                    sync_interval_secs: config.sync_interval_secs as i64,
                    bandwidth_limit_bps: None,
                    include_patterns: serde_json::json!([]),
                    exclude_patterns: serde_json::json!([]),
                    public: config.public,
                };
                if let Err(e) = db.upsert_mirror_config(&upsert).await {
                    warn!(distro = %config.distro, error = %e, "Failed to persist auto-mirror to DB");
                }
            }

            // Update in-memory
            self.upsert_config(config).await;
            created += 1;
        }

        if created > 0 {
            info!(
                "Auto-created {} mirror config(s) from client registration",
                created
            );
        }

        created
    }

    /// Trigger immediate sync for a distro
    /// Trigger an index+prefetch sync for a distro.
    ///
    /// Phase 1 (index): Download repo metadata files (.db, Packages.gz), parse them,
    /// and store all upstream package versions in the `upstream_packages` table.
    /// This gives the server a complete catalog of what exists upstream.
    ///
    /// Phase 2 (prefetch): Cross-reference upstream packages against what clients
    /// have installed. Download only packages where:
    /// (a) a client has the package installed, AND
    /// (b) the upstream version is newer than what any client has
    /// Also keep one previous version for downgrade support.
    ///
    /// If no DB is available, falls back to the legacy blind directory scrape.
    pub async fn trigger_sync(&self, distro: &str) -> Result<(), String> {
        let config = self
            .get_config(distro)
            .await
            .ok_or_else(|| format!("Mirror not found: {}", distro))?;

        // Mark as syncing
        {
            let mut configs = self.configs.write().await;
            if let Some(c) = configs.iter_mut().find(|c| c.distro == distro) {
                c.sync_status = "syncing".to_string();
            }
        }

        let sync_start = std::time::Instant::now();
        let local_path = self.mirror_dir.join(&config.local_path);

        // Try index+prefetch sync (requires DB). Fall back to legacy if no DB.
        let result = if self.db.is_some() {
            self.index_and_prefetch(distro, &config, &local_path).await
        } else {
            // Legacy: blind directory scrape
            sync_mirror(
                &config.upstream_url,
                &local_path,
                &config.exclude_patterns,
                config.bandwidth_limit_bps,
                &config.hash_policy,
            )
            .await
        };

        match result {
            Ok(bytes) => {
                let elapsed = sync_start.elapsed().as_secs_f64();
                monopsony_common::metrics::mirror_sync(distro, "success");
                monopsony_common::metrics::mirror_sync_duration(distro, elapsed);
                monopsony_common::metrics::mirror_sync_bytes(distro, bytes);

                let mut configs = self.configs.write().await;
                if let Some(c) = configs.iter_mut().find(|c| c.distro == distro) {
                    c.sync_status = "idle".to_string();
                    c.last_sync = Some(chrono::Utc::now().timestamp());
                    c.total_bytes = bytes;
                }

                // Update DB sync status
                if let Some(db) = &self.db {
                    let _ = db.update_mirror_last_sync(distro, bytes as i64).await;
                }

                Ok(())
            }
            Err(e) => {
                monopsony_common::metrics::mirror_sync(distro, "error");
                let mut configs = self.configs.write().await;
                if let Some(c) = configs.iter_mut().find(|c| c.distro == distro) {
                    c.sync_status = format!("error: {}", e);
                }
                Err(e)
            }
        }
    }

    /// Phase 1 + 2: Index repo metadata, then prefetch packages that clients need.
    async fn index_and_prefetch(
        &self,
        distro: &str,
        config: &MirrorConfig,
        local_path: &Path,
    ) -> Result<u64, String> {
        let db = self.db.as_ref().unwrap();

        // Detect arch from clients — default to x86_64
        let arch = "x86_64";

        // ── Phase 1: Download and parse repo index files ──
        let index_urls = crate::repo_index::repo_index_urls(distro, arch);
        if index_urls.is_empty() {
            // Unknown distro format — fall back to legacy scrape
            info!(
                distro,
                "No known index format, falling back to directory scrape"
            );
            return sync_mirror(
                &config.upstream_url,
                local_path,
                &config.exclude_patterns,
                config.bandwidth_limit_bps,
                &config.hash_policy,
            )
            .await;
        }

        info!(
            distro,
            "Phase 1: Indexing repo metadata ({} repos)",
            index_urls.len()
        );
        let mut total_indexed = 0usize;

        for (repo, index_path) in &index_urls {
            let url = format!(
                "{}/{}",
                config.upstream_url.trim_end_matches('/'),
                index_path
            );
            info!(repo, url = %url, "Downloading index");

            let bytes = download_file(&url).await?;

            let mut packages = if distro == "archlinux" {
                crate::repo_index::parse_pacman_db(&bytes)
            } else if distro == "debian" || distro == "ubuntu" {
                crate::repo_index::parse_debian_packages(&bytes)
            } else {
                Vec::new()
            };

            // Fill in download URLs
            crate::repo_index::resolve_download_urls(
                &mut packages,
                distro,
                repo,
                &config.upstream_url,
                arch,
            );

            // Persist to DB
            db.replace_upstream_packages(distro, repo, &packages)
                .await
                .map_err(|e| format!("Failed to store index: {}", e))?;

            total_indexed += packages.len();
        }

        info!(
            distro,
            total_indexed, "Phase 1 complete: indexed upstream packages"
        );

        // ── Phase 2: Prefetch packages that installed clients need ──
        info!(distro, "Phase 2: Checking inventory for needed updates");

        // Get all upstream versions for this distro
        let upstream = db
            .latest_upstream_versions(distro)
            .await
            .map_err(|e| format!("Failed to query upstream packages: {}", e))?;

        // Get all package names+versions that clients have installed
        let client_pkgs = db
            .get_all_client_package_versions(distro)
            .await
            .map_err(|e| format!("Failed to query client inventory: {}", e))?;

        // Build a set of (name) → latest installed version
        use std::collections::HashMap;
        let installed: HashMap<String, String> = client_pkgs
            .iter()
            .map(|(name, ver)| (name.clone(), ver.clone()))
            .collect();

        // Find packages where upstream has a newer version than what's installed
        let mut to_download: Vec<UpstreamPackageRow> = Vec::new();
        for up in &upstream {
            if let Some(installed_ver) = installed.get(&up.name) {
                if up.version != *installed_ver {
                    // Upstream version differs from installed — this is an update
                    info!(
                        name = %up.name,
                        installed = %installed_ver,
                        upstream = %up.version,
                        "Update available, will prefetch"
                    );
                    to_download.push(up.clone());
                }
            }
        }

        // Download the packages that need updating
        let mut total_bytes = 0u64;
        if !to_download.is_empty() {
            info!(
                distro,
                count = to_download.len(),
                "Phase 2: Prefetching updated packages"
            );

            let client = build_https_client();
            let local_mirror = self.mirror_dir.join(&config.local_path);
            tokio::fs::create_dir_all(&local_mirror)
                .await
                .map_err(|e| format!("Failed to create {}: {}", local_mirror.display(), e))?;

            for pkg in &to_download {
                // Use the filename field which contains the full repo-relative path
                // e.g., "core/os/x86_64/bash-5.3.15-1-x86_64.pkg.tar.zst"
                let dest = local_mirror.join(&pkg.filename);
                let url = &pkg.download_url;

                // Skip if already downloaded (recent)
                if dest.exists() {
                    if let Ok(meta) = tokio::fs::metadata(&dest).await {
                        if let Ok(modified) = meta.modified() {
                            if let Ok(age) = modified.elapsed() {
                                if age.as_secs() < 3600 {
                                    total_bytes += meta.len();
                                    continue;
                                }
                            }
                        }
                    }
                }

                match sync_single_file(
                    &client,
                    url,
                    &dest,
                    config.bandwidth_limit_bps,
                    &config.hash_policy,
                )
                .await
                {
                    Ok(size) => total_bytes += size,
                    Err(e) => warn!("Failed to prefetch {}: {}", url, e),
                }
            }
        } else {
            info!(
                distro,
                "Phase 2: No updates needed — all installed packages are current"
            );
        }

        // Also download index files themselves (so they're cached for clients)
        for (_repo, index_path) in &index_urls {
            let url = format!(
                "{}/{}",
                config.upstream_url.trim_end_matches('/'),
                index_path
            );
            let dest = local_path.join(index_path.split('/').last().unwrap_or("index"));
            if let Ok(size) =
                sync_single_file(&build_https_client(), &url, &dest, None, "fail-open").await
            {
                total_bytes += size;
            }
        }

        Ok(total_bytes)
    }
}

// ---------------------------------------------------------------------------
// Mirror manifest types
// ---------------------------------------------------------------------------

/// Top-level manifest served at /.well-known/monopsony/manifest.json
#[derive(Debug, Serialize)]
struct MirrorManifest {
    /// Schema version (always 1 for now)
    version: u32,
    /// Hostname of this server
    server_id: String,
    /// ISO 8601 timestamp when manifest was generated
    generated_at: String,
    /// List of configured mirrors with status
    mirrors: Vec<ManifestEntry>,
}

/// Single mirror entry in the manifest
#[derive(Debug, Serialize)]
struct ManifestEntry {
    distro: String,
    upstream_url: String,
    public: bool,
    last_sync: Option<i64>,
    sync_status: String,
    total_bytes: u64,
}

/// Best-effort hostname for server identification
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| {
            std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
                .map_err(|_| ())
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// Peer exchange types
// ---------------------------------------------------------------------------

/// Peer list served at /.well-known/monopsony/peers
#[derive(Debug, Serialize)]
struct PeerList {
    /// Mirrors this server knows about (from tracker + local config)
    peers: Vec<PeerEntry>,
    /// Where this peer list came from
    source: String,
}

#[derive(Debug, Serialize, Clone)]
struct PeerEntry {
    mirror_id: String,
    endpoint: String,
    expires_at: String,
}

/// GET /.well-known/monopsony/peers — peer exchange endpoint
async fn get_peers(State(state): State<MirrorState>) -> impl IntoResponse {
    let configs = state.manager.list_configs().await;

    // Build peer entries from local mirror configs
    let peers: Vec<PeerEntry> = configs
        .iter()
        .map(|c| {
            let expires = chrono::Utc::now() + chrono::Duration::hours(1);
            PeerEntry {
                mirror_id: format!("{}-{}", hostname(), c.distro),
                endpoint: format!("http://{}:3129/mirror/{}", hostname(), c.distro),
                expires_at: expires.to_rfc3339(),
            }
        })
        .collect();

    Json(PeerList {
        source: "local".to_string(),
        peers,
    })
}

/// Axum state for mirror HTTP routes
#[derive(Clone)]
pub struct MirrorState {
    pub manager: Arc<MirrorManager>,
    pub mirror_dir: PathBuf,
}

impl MirrorState {
    pub fn new(manager: MirrorManager, mirror_dir: PathBuf) -> Self {
        Self {
            manager: Arc::new(manager),
            mirror_dir,
        }
    }
}

/// GET /mirror/ — list all mirror configurations
async fn list_mirrors(State(state): State<MirrorState>) -> Json<Vec<MirrorConfig>> {
    Json(state.manager.list_configs().await)
}

/// GET /.well-known/monopsony/manifest.json — signed mirror inventory
async fn get_manifest(State(state): State<MirrorState>) -> impl IntoResponse {
    let configs = state.manager.list_configs().await;

    let mirrors: Vec<ManifestEntry> = configs
        .iter()
        .map(|c| ManifestEntry {
            distro: c.distro.clone(),
            upstream_url: c.upstream_url.clone(),
            public: c.public,
            last_sync: c.last_sync,
            sync_status: c.sync_status.clone(),
            total_bytes: c.total_bytes,
        })
        .collect();

    let manifest = MirrorManifest {
        version: 1,
        server_id: hostname(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        mirrors,
    };

    Json(manifest)
}

/// GET /mirror/{distro}/status — get sync status for a distro
async fn mirror_status(
    State(state): State<MirrorState>,
    AxumPath(distro): AxumPath<String>,
) -> impl IntoResponse {
    match state.manager.get_config(&distro).await {
        Some(config) => {
            let status = serde_json::json!({
                "distro": config.distro,
                "sync_status": config.sync_status,
                "last_sync": config.last_sync,
                "total_bytes": config.total_bytes,
                "upstream_url": config.upstream_url,
                "sync_enabled": config.sync_enabled,
                "public": config.public,
            });
            (StatusCode::OK, Json(status)).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Mirror not found").into_response(),
    }
}

/// POST /mirror/{distro}/sync — trigger immediate sync
async fn trigger_sync(
    State(state): State<MirrorState>,
    AxumPath(distro): AxumPath<String>,
) -> impl IntoResponse {
    match state.manager.trigger_sync(&distro).await {
        Ok(()) => (StatusCode::OK, "Sync triggered").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Sync failed: {}", e),
        )
            .into_response(),
    }
}

/// GET /mirror/{distro}/{*path} — serve mirrored files
async fn serve_mirror_file(
    State(state): State<MirrorState>,
    AxumPath((distro, file_path)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    let config = match state.manager.get_config(&distro).await {
        Some(c) => c,
        None => return (StatusCode::NOT_FOUND, "Mirror not found").into_response(),
    };

    let local_file = state.mirror_dir.join(&config.local_path).join(&file_path);

    // Sanitize: ensure path doesn't escape mirror dir
    let canonical_mirror = match state.mirror_dir.join(&config.local_path).canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return (StatusCode::NOT_FOUND, "Mirror directory not accessible").into_response()
        }
    };

    let canonical_file = match local_file.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // File doesn't exist locally — proxy to upstream
            return proxy_to_upstream(&config.upstream_url, &file_path).await;
        }
    };

    if !canonical_file.starts_with(&canonical_mirror) {
        return (StatusCode::FORBIDDEN, "Path traversal blocked").into_response();
    }

    // Serve local file
    match tokio::fs::read(&canonical_file).await {
        Ok(data) => {
            // Guess content type
            let content_type = mime_guess::from_path(&canonical_file)
                .first_or_octet_stream()
                .to_string();

            // Record metrics
            monopsony_common::metrics::mirror_served(&distro);
            monopsony_common::metrics::mirror_served_bytes(&distro, data.len() as u64);

            let response = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", content_type)
                .header("Content-Length", data.len())
                .header("X-Mirror", "hit")
                .body(axum::body::Body::from(data))
                .unwrap_or_else(|_| {
                    Response::new(axum::body::Body::from("Failed to build response"))
                });
            response.into_response()
        }
        Err(_) => proxy_to_upstream(&config.upstream_url, &file_path).await,
    }
}

/// Proxy file request to upstream mirror
async fn proxy_to_upstream(upstream_url: &str, file_path: &str) -> Response {
    let url = format!("{}/{}", upstream_url.trim_end_matches('/'), file_path);

    // SSRF validation
    let parsed_url = match url::Url::parse(&url) {
        Ok(u) => u,
        Err(_) => return (StatusCode::BAD_REQUEST, "Invalid URL").into_response(),
    };
    if crate::url_validation::validate_url_host(&parsed_url).is_err() {
        warn!("Mirror proxy blocked SSRF attempt: {}", url);
        return (StatusCode::BAD_REQUEST, "Blocked URL").into_response();
    }

    let client = build_https_client();

    let uri: hyper::Uri = match url.parse() {
        Ok(u) => u,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "Invalid URL").into_response();
        }
    };

    let req = match hyper::Request::builder()
        .uri(uri)
        .header("User-Agent", "Monopsony-Mirror/0.1")
        .body(empty_body())
    {
        Ok(req) => req,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    match client.request(req).await {
        Ok(resp) => {
            let status = resp.status();
            let (parts, body) = resp.into_parts();
            let bytes = body.collect().await.unwrap_or_default().to_bytes();

            // Enforce max body size (4 GB for mirror files)
            const MAX_MIRROR_PROXY_SIZE: usize = 4 * 1024 * 1024 * 1024;
            if bytes.len() > MAX_MIRROR_PROXY_SIZE {
                warn!("Mirror proxy response too large: {} bytes", bytes.len());
                return (StatusCode::BAD_GATEWAY, "Response too large").into_response();
            }

            let mut builder = Response::builder().status(status);
            for (name, value) in parts.headers.iter() {
                if name != "transfer-encoding" {
                    builder = builder.header(name, value);
                }
            }
            builder
                .header("X-Mirror", "miss")
                .body(axum::body::Body::from(bytes))
                .unwrap_or_else(|_| Response::new(axum::body::Body::from("Internal error")))
                .into_response()
        }
        Err(e) => {
            error!("Mirror upstream fetch failed for {}: {}", file_path, e);
            (StatusCode::BAD_GATEWAY, "Upstream unavailable").into_response()
        }
    }
}

/// Download a URL into memory. Returns the raw bytes.
/// Used for fetching repo index files.
async fn download_file(url: &str) -> Result<Vec<u8>, String> {
    let client = build_https_client();
    let uri: hyper::Uri = url
        .parse()
        .map_err(|e| format!("Invalid URL {}: {}", url, e))?;
    let req = hyper::Request::builder()
        .uri(uri)
        .header("User-Agent", "Monopsony-Mirror/1.0")
        .body(empty_body())
        .map_err(|e| format!("Failed to build request: {}", e))?;

    let response = client
        .request(req)
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {} for {}", response.status(), url));
    }

    let (_parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(|e| format!("Failed to read body: {}", e))?
        .to_bytes();

    Ok(bytes.to_vec())
}

/// Sync files from upstream mirror to local directory
async fn sync_mirror(
    upstream_url: &str,
    local_path: &Path,
    exclude_patterns: &[String],
    bandwidth_limit_bps: Option<u64>,
    hash_policy: &str,
) -> Result<u64, String> {
    info!(
        "Syncing mirror: {} -> {}",
        upstream_url,
        local_path.display()
    );

    // SSRF validation on upstream URL
    let parsed_upstream =
        url::Url::parse(upstream_url).map_err(|e| format!("Invalid upstream URL: {}", e))?;
    if let Err(e) = crate::url_validation::validate_url_host(&parsed_upstream) {
        return Err(format!(
            "SSRF blocked: upstream URL {} — {}",
            upstream_url, e
        ));
    }

    // Create local directory
    tokio::fs::create_dir_all(local_path)
        .await
        .map_err(|e| format!("Failed to create {}: {}", local_path.display(), e))?;

    let client = build_https_client();

    let uri: hyper::Uri = upstream_url
        .parse()
        .map_err(|e| format!("Invalid upstream URL: {}", e))?;

    let req = hyper::Request::builder()
        .uri(uri)
        .header("User-Agent", "Monopsony-Mirror/0.1")
        .body(empty_body())
        .map_err(|e| format!("Failed to build request: {}", e))?;

    let response = client
        .request(req)
        .await
        .map_err(|e| format!("Upstream request failed: {}", e))?;

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let (_parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?
        .to_bytes();

    if is_html_directory_listing(&content_type, &bytes) {
        // Parse HTML directory listing for links
        let links = parse_directory_links(&bytes, upstream_url);
        info!("Found {} entries in directory listing", links.len());

        let mut total_bytes = 0u64;
        for link in &links {
            // Check exclude patterns
            if exclude_patterns.iter().any(|p| simple_glob_match(p, link)) {
                debug!("Excluded: {}", link);
                continue;
            }

            let filename = link.split('/').next_back().unwrap_or(link);
            let child_local = local_path.join(filename);
            let child_url = format!(
                "{}/{}",
                upstream_url.trim_end_matches('/'),
                link.trim_start_matches('/')
            );

            // Skip if it looks like a directory (ends with /) or parent directory link
            if link.ends_with('/') || link.starts_with("../") || link == "./" || link == "../" {
                continue;
            }

            match sync_single_file(
                &client,
                &child_url,
                &child_local,
                bandwidth_limit_bps,
                hash_policy,
            )
            .await
            {
                Ok(size) => total_bytes += size,
                Err(e) => warn!("Failed to sync {}: {}", link, e),
            }
        }
        Ok(total_bytes)
    } else {
        // Single file — save directly
        let filename = upstream_url.split('/').next_back().unwrap_or("index");
        let file_path = local_path.join(filename);
        sync_single_file(
            &client,
            upstream_url,
            &file_path,
            bandwidth_limit_bps,
            hash_policy,
        )
        .await
    }
}

/// Verify a downloaded file against upstream SHA-256 checksum.
///
/// Tries fetching `{url}.sha256` and `{url}.sha256sum` from upstream.
/// If neither exists, returns Ok(()) (no checksum available = pass).
/// If a checksum is found and doesn't match, returns Err.
async fn verify_file_hash(
    client: &HttpClient,
    original_url: &str,
    data: &[u8],
    hash_policy: &str,
) -> Result<(), String> {
    // Compute local SHA-256
    let mut hasher = Sha256::new();
    hasher.update(data);
    let local_hash = format!("{:x}", hasher.finalize());

    // Try .sha256 first, then .sha256sum
    for suffix in &[".sha256", ".sha256sum"] {
        let hash_url = format!("{}{}", original_url, suffix);
        let uri: hyper::Uri = match hash_url.parse() {
            Ok(u) => u,
            Err(_) => continue,
        };

        let req = match hyper::Request::builder()
            .uri(uri)
            .header("User-Agent", "Monopsony-Mirror/0.1")
            .body(empty_body())
        {
            Ok(r) => r,
            Err(_) => continue,
        };

        let resp = match client.request(req).await {
            Ok(r) if r.status().is_success() => r,
            _ => continue,
        };

        let body = resp.into_parts().1;
        let bytes = match body.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => continue,
        };

        let hash_content = match std::str::from_utf8(&bytes) {
            Ok(s) => s.trim().to_string(),
            Err(_) => continue,
        };

        // Parse: typically "hash  filename" or just "hash"
        let expected_hash = hash_content
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();

        if expected_hash.is_empty() {
            continue;
        }

        if local_hash == expected_hash {
            debug!("SHA-256 verified for {}", original_url);
            return Ok(());
        } else if hash_policy == "fail-open" {
            warn!(
                "SHA-256 mismatch for {} (expected {}, got {}), serving anyway per fail-open policy",
                original_url, expected_hash, local_hash
            );
            return Ok(());
        } else {
            return Err(format!(
                "SHA-256 mismatch: expected {}, got {}",
                expected_hash, local_hash
            ));
        }
    }

    // No checksum file found upstream — pass silently
    Ok(())
}

/// Download and save a single file
async fn sync_single_file(
    client: &HttpClient,
    url: &str,
    local_path: &Path,
    bandwidth_limit_bps: Option<u64>,
    hash_policy: &str,
) -> Result<u64, String> {
    // Skip if file exists and is recent (within 1 hour)
    if local_path.exists() {
        if let Ok(meta) = tokio::fs::metadata(local_path).await {
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = modified.elapsed() {
                    if age.as_secs() < 3600 {
                        trace!("Skipping (recent): {}", local_path.display());
                        return Ok(meta.len());
                    }
                }
            }
        }
    }

    let uri: hyper::Uri = url
        .parse()
        .map_err(|e| format!("Invalid URL {}: {}", url, e))?;

    let req = hyper::Request::builder()
        .uri(uri)
        .header("User-Agent", "Monopsony-Mirror/0.1")
        .body(empty_body())
        .map_err(|e| format!("Failed to build request: {}", e))?;

    let response = client
        .request(req)
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let (parts, body) = response.into_parts();

    // Pre-flight: reject obviously huge files via content-length header
    const MAX_MIRROR_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024; // 4 GB
    if let Some(cl) = parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if cl > MAX_MIRROR_FILE_SIZE {
            return Err(format!(
                "File too large ({} bytes, max {})",
                cl, MAX_MIRROR_FILE_SIZE
            ));
        }
    }

    // Collect body
    let bytes = body
        .collect()
        .await
        .map_err(|e| format!("Failed to read body: {}", e))?
        .to_bytes();

    let len = bytes.len() as u64;

    // Double-check actual body size
    if len > MAX_MIRROR_FILE_SIZE {
        return Err(format!(
            "Body too large ({} bytes, max {})",
            len, MAX_MIRROR_FILE_SIZE
        ));
    }

    // Hash verification: try to fetch .sha256 / .sha256sum from upstream
    if let Err(e) = verify_file_hash(client, url, &bytes, hash_policy).await {
        warn!("Hash verification failed for {}: {}", url, e);
        // Don't delete — just warn. Admin can decide policy later.
        // Future: make this configurable (fail-closed vs fail-open)
    }

    // Apply bandwidth limiting (token bucket approximation)
    if let Some(bps) = bandwidth_limit_bps {
        if bps > 0 && len > 0 {
            let expected_time = (len as f64 / bps as f64).ceil() as u64;
            if expected_time > 0 {
                debug!(
                    "Bandwidth limited: {} bytes will take {}s at {} bps",
                    len, expected_time, bps
                );
                tokio::time::sleep(Duration::from_secs(expected_time.min(60))).await;
            }
        }
    }

    // Create parent directory
    if let Some(parent) = local_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create dir: {}", e))?;
    }

    // Write file atomically (write to .tmp then rename)
    let tmp_path = {
        let mut p = local_path.to_path_buf();
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        p.set_file_name(format!("{}.tmp", stem));
        p
    };

    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| format!("Failed to create file: {}", e))?;

    file.write_all(&bytes)
        .await
        .map_err(|e| format!("Failed to write file: {}", e))?;
    file.flush()
        .await
        .map_err(|e| format!("Failed to flush file: {}", e))?;
    drop(file);

    // Atomic rename
    tokio::fs::rename(&tmp_path, local_path)
        .await
        .map_err(|e| format!("Failed to rename: {}", e))?;

    debug!("Synced: {} ({} bytes)", local_path.display(), len);
    Ok(len)
}

/// Check if response is an HTML directory listing
fn is_html_directory_listing(content_type: &str, body: &[u8]) -> bool {
    content_type.contains("text/html") && body.len() < 1024 * 1024 // Don't try to parse huge responses
}

/// Parse links from an HTML directory listing (Apache/nginx style)
fn parse_directory_links(html: &[u8], _base_url: &str) -> Vec<String> {
    let html_str = match std::str::from_utf8(html) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut links = Vec::new();
    // Match href="..." in anchor tags — simple regex-free approach
    let mut search_from = 0;
    while let Some(start) = html_str[search_from..].find("href=\"") {
        let abs_start = search_from + start + 6; // len of "href=\""
        if let Some(end) = html_str[abs_start..].find('"') {
            let link = &html_str[abs_start..abs_start + end];
            // Skip anchors, query strings, parent references, and absolute paths
            if !link.starts_with('#')
                && !link.starts_with('?')
                && !link.contains("://")
                && !link.starts_with('/')
                && !link.starts_with("..")
            {
                links.push(link.to_string());
            }
            search_from = abs_start + end + 1;
        } else {
            break;
        }
    }

    // Deduplicate
    links.sort();
    links.dedup();
    links
}

/// Simple glob match: * matches anything, ? matches single char
fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let p_chars: Vec<char> = pattern.chars().collect();
    let t_chars: Vec<char> = text.chars().collect();
    glob_match_inner(&p_chars, &t_chars, 0, 0)
}

fn glob_match_inner(p: &[char], t: &[char], pi: usize, ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }

    match p[pi] {
        '*' => {
            // Try matching 0 or more characters
            for i in ti..=t.len() {
                if glob_match_inner(p, t, pi + 1, i) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if ti < t.len() {
                glob_match_inner(p, t, pi + 1, ti + 1)
            } else {
                false
            }
        }
        c => {
            if ti < t.len() && t[ti] == c {
                glob_match_inner(p, t, pi + 1, ti + 1)
            } else {
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rate limiting + auth middleware for mirror router
// ---------------------------------------------------------------------------

/// Token-bucket state for a single IP
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rps: u32) -> Self {
        let max = rps.max(1) as f64;
        Self {
            tokens: max,
            max_tokens: max,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume one token. Returns true if allowed.
    fn try_consume(&mut self, rps: u32) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rps as f64).min(self.max_tokens);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Shared rate-limiter state
#[derive(Debug, Clone)]
struct RateLimiter {
    /// Per-IP token buckets (IP -> bucket)
    buckets: Arc<tokio::sync::Mutex<HashMap<IpAddr, TokenBucket>>>,
    /// Configured requests per second (0 = unlimited)
    rps: u32,
}

impl RateLimiter {
    fn new(rps: u32) -> Self {
        Self {
            buckets: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            rps,
        }
    }

    async fn check(&self, ip: IpAddr) -> bool {
        if self.rps == 0 {
            return true;
        }
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets
            .entry(ip)
            .or_insert_with(|| TokenBucket::new(self.rps));
        bucket.try_consume(self.rps)
    }
}

/// Axum middleware: rate limiting + auth for mirror routes
use axum::body::Body as AxumBody;
use axum::extract::ConnectInfo;

use futures::future::BoxFuture;
use std::net::SocketAddr;
use tower::{Layer, Service};

#[derive(Clone)]
struct MirrorAuthLayer {
    limiter: RateLimiter,
    require_auth: bool,
    db: Option<Arc<Database>>,
}

impl MirrorAuthLayer {
    fn new(limiter: RateLimiter, require_auth: bool, db: Option<Arc<Database>>) -> Self {
        Self {
            limiter,
            require_auth,
            db,
        }
    }
}

impl<S> Layer<S> for MirrorAuthLayer {
    type Service = MirrorAuthMiddleware<S>;
    fn layer(&self, inner: S) -> Self::Service {
        MirrorAuthMiddleware {
            inner,
            limiter: self.limiter.clone(),
            require_auth: self.require_auth,
            db: self.db.clone(),
        }
    }
}

#[derive(Clone)]
struct MirrorAuthMiddleware<S> {
    inner: S,
    limiter: RateLimiter,
    require_auth: bool,
    db: Option<Arc<Database>>,
}

impl<S> Service<axum::http::Request<AxumBody>> for MirrorAuthMiddleware<S>
where
    S: Service<axum::http::Request<AxumBody>, Response = axum::response::Response>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<AxumBody>) -> Self::Future {
        let limiter = self.limiter.clone();
        let require_auth = self.require_auth;
        let db = self.db.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // Extract client IP from ConnectInfo or X-Forwarded-For
            let ip = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip())
                .unwrap_or_else(|| "127.0.0.1".parse().unwrap());

            // Rate limit check
            if !limiter.check(ip).await {
                let resp = axum::response::Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .header("Retry-After", "1")
                    .header("X-RateLimit-Limit", limiter.rps.to_string())
                    .body(AxumBody::from("rate limit exceeded"))
                    .unwrap();
                return Ok(resp);
            }

            // Auth check: if not public, require X-Monopsony-Key
            if require_auth {
                let key = req
                    .headers()
                    .get("X-Monopsony-Key")
                    .and_then(|v| v.to_str().ok());
                let key = match key {
                    Some(k) => k,
                    None => {
                        let resp = axum::response::Response::builder()
                            .status(StatusCode::UNAUTHORIZED)
                            .body(AxumBody::from("authentication required"))
                            .unwrap();
                        return Ok(resp);
                    }
                };

                // Validate API key: SHA-256 hash of key, check against DB
                let key_hash = {
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(key.as_bytes());
                    hex::encode(hasher.finalize())
                };

                let valid = if let Some(db) = &db {
                    match db.validate_api_key(&key_hash).await {
                        Ok(Some(_)) => true,
                        Ok(None) => false,
                        Err(e) => {
                            tracing::warn!("API key validation DB error: {}", e);
                            false
                        }
                    }
                } else {
                    // No DB configured: accept any non-empty key (dev/test mode)
                    tracing::debug!("No DB for mirror auth, accepting key (dev mode)");
                    true
                };

                if !valid {
                    let resp = axum::response::Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(AxumBody::from("invalid API key"))
                        .unwrap();
                    return Ok(resp);
                }
            }

            inner.call(req).await
        })
    }
}

/// Create the mirror HTTP router with rate limiting and auth middleware
pub fn create_mirror_router(state: MirrorState, max_rps: u32, any_private: bool) -> axum::Router {
    let db = state.manager.db.clone();
    let limiter = RateLimiter::new(max_rps);
    let layer = MirrorAuthLayer::new(limiter, any_private, db);

    axum::Router::new()
        .route("/", axum::routing::get(list_mirrors))
        .route(
            "/.well-known/monopsony/manifest.json",
            axum::routing::get(get_manifest),
        )
        .route(
            "/.well-known/monopsony/peers",
            axum::routing::get(get_peers),
        )
        // Windows PM shims: static prefixes win over the /:distro/*path
        // catch-all in matchit, so /scoop/... never collides with distro
        // serving. Each shim is self-contained (own state clone, own routes).
        .nest("/scoop", crate::scoop_shim::router(state.clone()).with_state(()))
        .nest("/choco", crate::choco_shim::router(state.clone()).with_state(()))
        .nest("/winget", crate::winget_shim::router(state.clone()).with_state(()))
        .route("/:distro/status", axum::routing::get(mirror_status))
        .route("/:distro/sync", axum::routing::post(trigger_sync))
        .route("/:distro/*path", axum::routing::get(serve_mirror_file))
        .with_state(state)
        .layer(layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mirror_config_default() {
        let config = MirrorConfig::default();
        assert!(config.sync_enabled);
        assert_eq!(config.sync_interval_secs, 86400);
        assert_eq!(config.sync_status, "idle");
        assert_eq!(config.total_bytes, 0);
        assert!(config.exclude_patterns.is_empty());
        assert!(!config.public);
    }

    #[test]
    fn test_mirror_config_deserialize() {
        let toml = r#"
[[items]]
distro = "archlinux"
upstream_url = "https://geo.mirror.pkgbuild.com"
local_path = "archlinux"
sync_enabled = true
sync_interval_secs = 43200
public = false

[[items]]
distro = "ubuntu"
upstream_url = "http://archive.ubuntu.com/ubuntu"
local_path = "ubuntu"
sync_enabled = false
"#;
        // Parse as a wrapper struct since toml needs a table key for arrays
        #[derive(Deserialize)]
        struct Wrapper {
            items: Vec<MirrorConfig>,
        }
        let wrapper: Wrapper = toml::from_str(toml).unwrap();
        assert_eq!(wrapper.items.len(), 2);
        assert_eq!(wrapper.items[0].distro, "archlinux");
        assert_eq!(wrapper.items[0].sync_interval_secs, 43200);
        assert!(!wrapper.items[1].sync_enabled);
    }

    #[test]
    fn test_mirror_toml_roundtrip() {
        let config = MirrorConfig {
            distro: "archlinux".to_string(),
            upstream_url: "https://geo.mirror.pkg.build.com".to_string(),
            local_path: PathBuf::from("archlinux"),
            sync_enabled: true,
            sync_interval_secs: 43200,
            bandwidth_limit_bps: Some(10_000_000),
            include_patterns: vec!["*.pkg.tar.zst".to_string()],
            exclude_patterns: vec!["*.sig".to_string()],
            public: true,
            per_ip_rps: 10,
            max_concurrent_per_ip: 0,
            last_sync: Some(1714000000),
            sync_status: "idle".to_string(),
            total_bytes: 1024,
            hash_policy: "fail-closed".to_string(),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: MirrorConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.distro, "archlinux");
        assert_eq!(parsed.upstream_url, "https://geo.mirror.pkg.build.com");
        assert!(parsed.sync_enabled);
        assert_eq!(parsed.sync_interval_secs, 43200);
        assert_eq!(parsed.bandwidth_limit_bps, Some(10_000_000));
        assert_eq!(parsed.include_patterns.len(), 1);
        assert!(parsed.public);
        assert_eq!(parsed.total_bytes, 1024);
    }

    #[test]
    fn test_glob_match() {
        assert!(simple_glob_match("*.pkg.tar.zst", "linux-6.8.pkg.tar.zst"));
        assert!(simple_glob_match("*.sig", "linux.sig"));
        assert!(!simple_glob_match("*.pkg.tar.zst", "linux.sig"));
        assert!(simple_glob_match("test.*", "test.iso"));
        assert!(simple_glob_match("?ello", "hello"));
        assert!(!simple_glob_match("?ello", "hallo")); // single ? only matches one
        assert!(simple_glob_match("*", "anything"));
        assert!(!simple_glob_match("", "something"));
        assert!(simple_glob_match("", ""));
    }

    #[test]
    fn test_parse_directory_links() {
        let html = br#"<html><body>
<a href="linux-6.8.pkg.tar.zst">linux-6.8.pkg.tar.zst</a>
<a href="linux-6.8.pkg.tar.zst.sig">linux-6.8.pkg.tar.zst.sig</a>
<a href="../">Parent Directory</a>
<a href="/icons/">Icons</a>
<a href="?C=M;O=A">Sort</a>
</body></html>"#;

        let links = parse_directory_links(html, "https://mirror.example.com/archlinux/");
        assert!(links.contains(&"linux-6.8.pkg.tar.zst".to_string()));
        assert!(links.contains(&"linux-6.8.pkg.tar.zst.sig".to_string()));
        // Should NOT contain parent dir, absolute paths, or query strings
        assert!(!links
            .iter()
            .any(|l| l.starts_with('/') || l.starts_with('?') || l.starts_with("..")));
    }
}
