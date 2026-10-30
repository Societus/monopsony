use anyhow::Result;
use chrono::Utc;
use monopsony_common::package_manager::PackageManagerKind;
use monopsony_common::{
    ApiKey, ApiKeyId, Client, ClientCert, ClientId, ClientPackage, Package, PackageId, PackageRef,
};
use tracing::{debug, info, warn};

use sqlx::{PgPool, Row};
use uuid::Uuid;

pub struct Database {
    pool: PgPool,
}

/// server_settings key holding the per-PM language enablement map (Sprint 23).
pub const LANGUAGE_PMS_KEY: &str = "language_pms";

/// Enablement state of one language package manager (Sprint 23 §2.4).
/// `source`: "default" | "auto" (auto-enable-on-first-use) | "dashboard" |
/// "config" (server.toml boot value).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LanguagePmState {
    pub enabled: bool,
    pub source: String,
}

/// Cache entry metadata for recording cache misses
#[derive(Debug, Clone)]
pub struct CacheMissConfig {
    pub url: String,
    pub cache_path: String,
    /// Cache class (Sprint 23). Callers default to "distro".
    pub cache_class: String,
    pub content_type: Option<String>,
    pub content_length: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cached_at: i64,
}

/// Mirror configuration for upsert operations
#[derive(Debug, Clone)]
pub struct MirrorUpsertConfig {
    pub distro: String,
    pub upstream_url: String,
    pub local_path: String,
    pub sync_enabled: bool,
    pub sync_interval_secs: i64,
    pub bandwidth_limit_bps: Option<i64>,
    pub include_patterns: serde_json::Value,
    pub exclude_patterns: serde_json::Value,
    pub public: bool,
}

/// Scan result record for writing to cache_scan_results.
#[derive(Debug, Clone)]
pub struct ScanResultRecord {
    pub cache_path: String,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
    pub scan_version: i64,
    pub clamav_result: Option<String>,
    pub yara_matches: Option<serde_json::Value>,
    pub max_entropy: Option<f64>,
    pub cve_matches: Option<serde_json::Value>,
    pub scan_status: String,
    pub blocked_reason: Option<String>,
    pub scanned_at: chrono::DateTime<Utc>,
}

/// Row from cache_scan_results.
#[derive(Debug, Clone)]
pub struct ScanResultRow {
    pub cache_path: String,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
    pub scan_version: i64,
    pub clamav_result: Option<String>,
    pub yara_matches: Option<serde_json::Value>,
    pub max_entropy: Option<f64>,
    pub cve_matches: Option<serde_json::Value>,
    pub scan_status: String,
    pub blocked_reason: Option<String>,
    pub scanned_at: chrono::DateTime<Utc>,
}

/// New tamper alert to insert into tamper_alerts.
#[derive(Debug, Clone)]
pub struct TamperAlertInsert {
    pub url: String,
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub detail: Option<String>,
    pub client_id: Option<String>,
    pub observed_at: i64,
}

/// Row from tamper_alerts.
#[derive(Debug, Clone)]
pub struct TamperAlertRow {
    pub id: i64,
    pub url: String,
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub detail: Option<String>,
    pub client_id: Option<String>,
    pub observed_at: i64,
    pub recorded_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Database {
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl Database {
    pub async fn connect(database_url: &str) -> Result<Self> {
        info!("Connecting to PostgreSQL");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(database_url)
            .await?;
        info!("Connected to PostgreSQL");
        Ok(Self { pool })
    }

    pub async fn run_migrations(&self) -> Result<()> {
        info!("Running database migrations");

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS clients (
                id UUID PRIMARY KEY,
                hostname TEXT NOT NULL UNIQUE,
                os TEXT NOT NULL,
                os_version TEXT NOT NULL,
                package_managers JSONB NOT NULL,
                last_seen TIMESTAMPTZ NOT NULL,
                registered_at TIMESTAMPTZ NOT NULL,
                cert_serial TEXT NULL,
                paired_at TIMESTAMPTZ NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created clients table");

        // Sprint 29 B1 (grounding D1): per-client update counts + reported
        // distro identity, persisted as additive columns (NOT server_settings
        // — that is for global flags, not per-client rows). Append-only
        // ALTER converges pre-existing DBs; fresh CREATE TABLEs above have
        // no such columns, so the ALTER is the single source.
        sqlx::query(
            "ALTER TABLE clients ADD COLUMN IF NOT EXISTS update_security INT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE clients ADD COLUMN IF NOT EXISTS update_other INT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE clients ADD COLUMN IF NOT EXISTS update_total INT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE clients ADD COLUMN IF NOT EXISTS update_reported_at TIMESTAMPTZ NULL",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("ALTER TABLE clients ADD COLUMN IF NOT EXISTS reported_distro TEXT NULL")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "ALTER TABLE clients ADD COLUMN IF NOT EXISTS reported_distro_version TEXT NULL",
        )
        .execute(&self.pool)
        .await?;
        // Sprint 29 Phase 3: client-reported vendor preflight verdict
        // (ready flag + reasons, notes stored as a JSON array string).
        sqlx::query("ALTER TABLE clients ADD COLUMN IF NOT EXISTS update_ready BOOLEAN NULL")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "ALTER TABLE clients ADD COLUMN IF NOT EXISTS update_readiness_notes TEXT NULL",
        )
        .execute(&self.pool)
        .await?;
        debug!("clients update-count columns ensured");

        // client_certs tracks every issued client certificate by serial so
        // individual certs can be revoked (red-team F3: short-lived certs
        // with revocation path, replacing the old 10-year un-revocable certs).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS client_certs (
                serial TEXT PRIMARY KEY,
                client_id UUID NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
                issued_at TIMESTAMPTZ NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                revoked_at TIMESTAMPTZ NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created client_certs table");

        // pair_approval_audit records every approve/deny action on v2 pairing
        // requests (P4 approval audit view).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS pair_approval_audit (
                id          BIGSERIAL PRIMARY KEY,
                request_id  TEXT NOT NULL,
                action      TEXT NOT NULL,
                approver    TEXT NOT NULL,
                detail      TEXT NULL,
                created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created pair_approval_audit table");

        // enrollment_keys (P5): Tailscale-auth-key pattern for fleet-scale
        // pairing. Plaintext keys are shown once at creation; only the
        // SHA-256 hash is stored. Use-count + expiry bound the blast radius.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS enrollment_keys (
                id          UUID PRIMARY KEY,
                key_hash    TEXT NOT NULL UNIQUE,
                label       TEXT NOT NULL,
                max_uses    BIGINT NOT NULL DEFAULT 1,
                use_count   BIGINT NOT NULL DEFAULT 0,
                created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
                expires_at  TIMESTAMPTZ NULL,
                revoked_at  TIMESTAMPTZ NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created enrollment_keys table");

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS packages (
                id UUID PRIMARY KEY,
                manager TEXT NOT NULL,
                name TEXT NOT NULL,
                version TEXT NOT NULL,
                arch TEXT NULL,
                file_path TEXT NOT NULL,
                hash TEXT NULL,
                upstream_url TEXT NOT NULL,
                size_bytes BIGINT NOT NULL,
                cached_at TIMESTAMPTZ NOT NULL,
                last_accessed TIMESTAMPTZ NOT NULL,
                UNIQUE (manager, name, version, arch)
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created packages table");

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS client_packages (
                client_id UUID NOT NULL REFERENCES clients(id),
                package_id UUID NOT NULL REFERENCES packages(id),
                installed_at TIMESTAMPTZ NOT NULL,
                PRIMARY KEY(client_id, package_id)
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created client_packages table");

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS cache_entries (
                url TEXT PRIMARY KEY,
                cache_path TEXT NOT NULL,
                content_type TEXT NULL,
                content_length BIGINT NOT NULL,
                etag TEXT NULL,
                last_modified TEXT NULL,
                cached_at BIGINT NOT NULL,
                last_accessed BIGINT NOT NULL,
                hit_count BIGINT NOT NULL DEFAULT 0,
                sha256 TEXT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created cache_entries table");

        // Sprint 23: cache class column. Additive ALTER for pre-Sprint-23
        // DBs — swallow only the exact duplicate-column error so fresh
        // CREATE TABLEs (column inline above for NEW deployments would
        // conflict with this ALTER; the ALTER is the single source) and
        // existing DBs converge on one schema. Fresh DBs: CREATE TABLE has
        // no class column, ALTER adds it — one code path for both.
        match sqlx::query(
            "ALTER TABLE cache_entries ADD COLUMN IF NOT EXISTS cache_class TEXT NOT NULL DEFAULT 'distro'",
        )
        .execute(&self.pool)
        .await
        {
            Ok(_) => debug!("cache_entries.cache_class column ensured"),
            Err(e) => warn!("cache_class column ensure failed (may already exist): {}", e),
        }

        // Per-PM language enablement audit trail (Sprint 23 §2.4). One row
        // per enablement transition (auto-on-first-use, dashboard toggle,
        // config-file boot win). Outcome: "enabled" | "disabled".
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS pm_enablement_audit (
                id BIGSERIAL PRIMARY KEY,
                pm TEXT NOT NULL,
                outcome TEXT NOT NULL,
                source TEXT NOT NULL,
                reason TEXT NULL,
                recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created pm_enablement_audit table");

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS api_keys (
                id UUID PRIMARY KEY,
                label TEXT NOT NULL,
                key_hash TEXT NOT NULL UNIQUE,
                created_at TIMESTAMPTZ NOT NULL,
                last_used TIMESTAMPTZ NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created api_keys table");

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mirror_configs (
                distro TEXT PRIMARY KEY,
                upstream_url TEXT NOT NULL,
                local_path TEXT NOT NULL,
                sync_enabled BOOLEAN NOT NULL DEFAULT true,
                sync_interval_secs BIGINT NOT NULL DEFAULT 86400,
                bandwidth_limit_bps BIGINT NULL,
                include_patterns JSONB NOT NULL DEFAULT '[]',
                exclude_patterns JSONB NOT NULL DEFAULT '[]',
                public BOOLEAN NOT NULL DEFAULT false,
                last_sync TIMESTAMPTZ NULL,
                sync_status TEXT NOT NULL DEFAULT 'idle',
                total_bytes BIGINT NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created mirror_configs table");

        // Upstream package index — populated by parsing repo metadata (pacman .db,
        // Debian Packages.gz, Fedora repomd.xml). Used for inventory-aware prefetch.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS upstream_packages (
                id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                distro          TEXT NOT NULL,
                repo            TEXT NOT NULL,
                name            TEXT NOT NULL,
                version         TEXT NOT NULL,
                architecture    TEXT NOT NULL,
                filename        TEXT NOT NULL,
                download_url    TEXT NOT NULL,
                size_bytes      BIGINT NOT NULL DEFAULT 0,
                sha256          TEXT,
                indexed_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                UNIQUE (distro, repo, name, version, architecture)
            )",
        )
        .execute(&self.pool)
        .await?;
        info!("Created upstream_packages table");

        // Shadow package tables
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS shadow_packages (
                id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                client_id       UUID REFERENCES clients(id) ON DELETE CASCADE,
                name            TEXT NOT NULL,
                version         TEXT NOT NULL,
                architecture    TEXT NOT NULL,
                os_family       TEXT NOT NULL,
                libc            TEXT,
                source_type     TEXT NOT NULL,
                source_url      TEXT NOT NULL,
                recipe_path     TEXT NOT NULL,
                install_paths   JSONB NOT NULL,
                config_paths    JSONB,
                service_name    TEXT,
                masked_from_pm  TEXT,
                registered_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
                last_updated_at TIMESTAMPTZ,
                last_checked_at TIMESTAMPTZ
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_shadow_packages_client ON shadow_packages(client_id)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_shadow_packages_name ON shadow_packages(name)")
            .execute(&self.pool)
            .await?;

        // P4a: binary_sha256 content-address column. CREATE TABLE IF NOT EXISTS
        // does not add columns to a pre-existing table, so an additive ALTER
        // converges shadow_packages onto the new schema on every startup.
        match sqlx::query("ALTER TABLE shadow_packages ADD COLUMN IF NOT EXISTS binary_sha256 TEXT")
            .execute(&self.pool)
            .await
        {
            Ok(_) => debug!("shadow_packages.binary_sha256 column ensured"),
            Err(e) => warn!(
                "binary_sha256 column ensure failed (may already exist): {}",
                e
            ),
        }

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS shadow_package_versions (
                id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                package_id      UUID REFERENCES shadow_packages(id) ON DELETE CASCADE,
                version         TEXT NOT NULL,
                tag             TEXT,
                release_notes_url TEXT,
                assets          JSONB,
                cached          BOOLEAN DEFAULT FALSE,
                discovered_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
                UNIQUE(package_id, version)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS shadow_update_history (
                id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                package_id      UUID REFERENCES shadow_packages(id) ON DELETE CASCADE,
                from_version    TEXT NOT NULL,
                to_version      TEXT NOT NULL,
                status          TEXT NOT NULL,
                backup_path     TEXT,
                error_message   TEXT,
                applied_at      TIMESTAMPTZ,
                rolled_back_at  TIMESTAMPTZ
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_shadow_update_history_package ON shadow_update_history(package_id)")
            .execute(&self.pool)
            .await?;

        // Scan results for malware/vulnerability detection
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS cache_scan_results (
                id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                cache_path      TEXT NOT NULL,
                package_name    TEXT,
                package_version TEXT,
                scan_version    BIGINT NOT NULL DEFAULT 1,
                clamav_result   TEXT,
                yara_matches    JSONB,
                max_entropy     FLOAT,
                cve_matches     JSONB,
                scan_status     TEXT NOT NULL DEFAULT 'pending',
                blocked_reason  TEXT,
                scanned_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                UNIQUE(cache_path, scan_version)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_cache_scan_results_path ON cache_scan_results(cache_path)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_cache_scan_results_status ON cache_scan_results(scan_status)")
            .execute(&self.pool)
            .await?;

        // Server settings (feature flags, monetization) — single-row key/value
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS server_settings (
                key         TEXT PRIMARY KEY,
                value       JSONB NOT NULL,
                updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await?;

        // Pin attestations (Sprint 20 Phase 3): signed origin-integrity pins
        // keyed by canonical cache URL — same key as cache_entries.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS cache_pins (
                url             TEXT PRIMARY KEY,
                expected_sha256 TEXT NOT NULL,
                origin          TEXT NOT NULL,
                key_id          TEXT NOT NULL,
                observed_at     BIGINT NOT NULL,
                signature       TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_cache_pins_key_id ON cache_pins(key_id)")
            .execute(&self.pool)
            .await?;

        // Tamper alarms raised by clients (Sprint 20 Phase 3): one row per
        // client-reported pin mismatch; quarantine bookkeeping links here.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tamper_alerts (
                id              BIGSERIAL PRIMARY KEY,
                url             TEXT NOT NULL,
                expected_sha256 TEXT NOT NULL,
                actual_sha256   TEXT NOT NULL,
                detail          TEXT NULL,
                client_id       TEXT NULL,
                observed_at     BIGINT NOT NULL,
                recorded_at     TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_tamper_alerts_url ON tamper_alerts(url)")
            .execute(&self.pool)
            .await?;

        // Sourcedb observation log (Sprint 21): append-only version
        // observations per canonical package_key.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS observations (
                id              BIGSERIAL PRIMARY KEY,
                package_key     TEXT NOT NULL,
                source          TEXT NOT NULL,
                version         TEXT NOT NULL,
                version_ord     TEXT NOT NULL,
                scheme          TEXT NOT NULL,
                observed_at     BIGINT NOT NULL,
                observed_by     TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_observations_package_key ON observations(package_key)",
        )
        .execute(&self.pool)
        .await?;

        // Per-key resolution state (Sprint 21): chosen source, method, and
        // poll bookkeeping for each canonical package_key.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS package_map (
                package_key     TEXT PRIMARY KEY,
                source          TEXT,
                resolved_at     BIGINT,
                resolve_method  TEXT,
                etag            TEXT,
                last_poll       BIGINT
            )",
        )
        .execute(&self.pool)
        .await?;

        // Sprint 22 (22-C1): server-global shadow recipe catalog.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS shadow_recipes (
                name                   TEXT PRIMARY KEY,
                recipe_toml            TEXT NOT NULL,
                signature              TEXT NOT NULL,
                key_id                 TEXT NOT NULL,
                catalog_hash           TEXT NOT NULL,
                published_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
                provenance_client_id   UUID,
                updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS shadow_scan_results (
                id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                client_id         UUID REFERENCES clients(id) ON DELETE CASCADE,
                recipe_name       TEXT NOT NULL,
                installed         BOOLEAN NOT NULL,
                detected_version  TEXT,
                method            TEXT,
                scanned_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
                UNIQUE(client_id, recipe_name)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_shadow_scan_results_client ON shadow_scan_results(client_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_shadow_scan_results_recipe ON shadow_scan_results(recipe_name)")
            .execute(&self.pool)
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS shadow_assignment_audit (
                id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                client_id      UUID,
                recipe_name    TEXT NOT NULL,
                outcome        TEXT NOT NULL,
                reason         TEXT,
                acted_at       TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_shadow_assignment_audit_client ON shadow_assignment_audit(client_id)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS pending_shadow_assignments (
                id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                client_id    UUID REFERENCES clients(id) ON DELETE CASCADE,
                recipe_name  TEXT NOT NULL,
                status       TEXT NOT NULL DEFAULT 'pending',
                created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
                delivered_at TIMESTAMPTZ,
                resolved_at  TIMESTAMPTZ,
                UNIQUE(client_id, recipe_name)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_pending_shadow_assignments_client ON pending_shadow_assignments(client_id)",
        )
        .execute(&self.pool)
        .await?;

        // Shadow staging: packages the server has staged to a client that await
        // client-side approval (review of recipe, install location, accessory
        // directories, firewall rules, health detection) before installation.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS shadow_staging (
                id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                client_id            UUID REFERENCES clients(id) ON DELETE CASCADE,
                recipe_name          TEXT NOT NULL,
                status               TEXT NOT NULL DEFAULT 'pending',
                suggested_target_dir TEXT NOT NULL,
                suggested_accessory_dirs JSONB NOT NULL DEFAULT '[]',
                suggested_firewall_rules JSONB NOT NULL DEFAULT '[]',
                suggested_health_check JSONB,
                client_target_dir    TEXT,
                client_accessory_dirs JSONB DEFAULT '[]',
                client_firewall_rules JSONB DEFAULT '[]',
                client_health_check  JSONB,
                created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
                reviewed_at          TIMESTAMPTZ,
                resolved_at          TIMESTAMPTZ,
                UNIQUE(client_id, recipe_name)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_shadow_staging_client ON shadow_staging(client_id)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_shadow_staging_status ON shadow_staging(status)",
        )
        .execute(&self.pool)
        .await?;

        info!("Database migrations complete");
        Ok(())
    }

    pub async fn register_client(
        &self,
        hostname: &str,
        os: &str,
        os_version: &str,
        package_managers: &[String],
    ) -> Result<ClientId> {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let pm_json = serde_json::to_value(package_managers)?;

        let row = sqlx::query(
            "INSERT INTO clients (id, hostname, os, os_version, package_managers, last_seen, registered_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (hostname) DO UPDATE SET
                 os = EXCLUDED.os,
                 os_version = EXCLUDED.os_version,
                 package_managers = EXCLUDED.package_managers,
                 last_seen = EXCLUDED.last_seen
             RETURNING id",
        )
        .bind(id)
        .bind(hostname)
        .bind(os)
        .bind(os_version)
        .bind(pm_json)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        let returned_id: Uuid = row.get("id");
        info!("Registered client {} (hostname={})", returned_id, hostname);
        Ok(ClientId(returned_id))
    }

    pub async fn heartbeat(&self, client_id: &ClientId) -> Result<()> {
        let now = Utc::now();
        let result = sqlx::query("UPDATE clients SET last_seen = $1 WHERE id = $2")
            .bind(now)
            .bind(client_id.0)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            warn!("Heartbeat for unknown client {}", client_id);
        } else {
            debug!("Heartbeat from client {}", client_id);
        }
        Ok(())
    }

    pub async fn record_inventory(
        &self,
        client_id: &ClientId,
        packages: &[PackageRef],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();

        // Get current package IDs for this client
        let existing: Vec<Uuid> =
            sqlx::query("SELECT package_id FROM client_packages WHERE client_id = $1")
                .bind(client_id.0)
                .fetch_all(&mut *tx)
                .await?
                .iter()
                .map(|r| r.get::<Uuid, _>("package_id"))
                .collect();

        let mut current_ids = std::collections::HashSet::new();

        for pkg in packages {
            // Find existing package by unique key, or insert new
            let row = sqlx::query(
                "SELECT id FROM packages WHERE manager = $1 AND name = $2 AND version = $3 AND arch IS NOT DISTINCT FROM $4",
            )
            .bind(pkg.manager.as_str())
            .bind(&pkg.name)
            .bind(pkg.version.as_deref().unwrap_or(""))
            .bind(&pkg.architecture)
            .fetch_optional(&mut *tx)
            .await?;

            let package_id = if let Some(r) = row {
                r.get::<Uuid, _>("id")
            } else {
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO packages (id, manager, name, version, arch, file_path, upstream_url, size_bytes, cached_at, last_accessed)
                     VALUES ($1, $2, $3, $4, $5, '', '', 0, $6, $6)",
                )
                .bind(id)
                .bind(pkg.manager.as_str())
                .bind(&pkg.name)
                .bind(pkg.version.as_deref().unwrap_or(""))
                .bind(&pkg.architecture)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                id
            };

            current_ids.insert(package_id);

            sqlx::query(
                "INSERT INTO client_packages (client_id, package_id, installed_at) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(client_id.0)
            .bind(package_id)
            .bind(if pkg.installed_at > 0 {
                chrono::DateTime::from_timestamp(pkg.installed_at, 0).unwrap_or(now)
            } else {
                now
            })
            .execute(&mut *tx)
            .await?;
        }

        // Remove packages no longer installed
        for old_id in &existing {
            if !current_ids.contains(old_id) {
                sqlx::query("DELETE FROM client_packages WHERE client_id = $1 AND package_id = $2")
                    .bind(client_id.0)
                    .bind(old_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        tx.commit().await?;
        debug!(
            "Recorded inventory for client {}: {} packages",
            client_id,
            packages.len()
        );
        Ok(())
    }

    /// Apply an incremental inventory delta (added/removed packages)
    pub async fn apply_inventory_delta(
        &self,
        client_id: &ClientId,
        added: &[PackageRef],
        removed: &[(PackageManagerKind, String)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();

        // Add new packages
        for pkg in added {
            let package_id = Uuid::new_v4();

            sqlx::query(
                "INSERT INTO packages (id, manager, name, version, arch, file_path, upstream_url, size_bytes, cached_at, last_accessed)
                 VALUES ($1, $2, $3, $4, $5, '', '', 0, $6, $6)
                 ON CONFLICT DO NOTHING",
            )
            .bind(package_id)
            .bind(pkg.manager.as_str())
            .bind(&pkg.name)
            .bind(pkg.version.as_deref().unwrap_or(""))
            .bind(&pkg.architecture)
            .bind(now)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "INSERT INTO client_packages (client_id, package_id, installed_at)
                 VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
            )
            .bind(client_id.0)
            .bind(package_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        // Remove packages
        for (manager, package_id_str) in removed {
            // Parse the package ID string to extract components
            // Format: "manager:name:version:arch" or "manager:name"
            let parts: Vec<&str> = package_id_str.split(':').collect();
            if parts.len() >= 2 {
                let name = parts[1];

                // Delete from client_packages by matching the package
                sqlx::query(
                    "DELETE FROM client_packages
                     WHERE client_id = $1
                     AND package_id IN (
                         SELECT id FROM packages
                         WHERE manager = $2 AND name = $3
                     )",
                )
                .bind(client_id.0)
                .bind(manager.as_str())
                .bind(name)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        debug!(
            "Applied inventory delta for client {}: +{} -{}",
            client_id,
            added.len(),
            removed.len()
        );
        Ok(())
    }

    #[allow(dead_code)] // Admin API query method
    pub async fn get_client(&self, client_id: &ClientId) -> Result<Option<Client>> {
        let row = sqlx::query(
            "SELECT id, hostname, os, os_version, package_managers, last_seen, registered_at, cert_serial,
                    update_security, update_other, update_total, update_reported_at,
                    reported_distro, reported_distro_version,
                    update_ready, update_readiness_notes
             FROM clients WHERE id = $1",
        )
        .bind(client_id.0)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => {
                let pm_value: serde_json::Value = row.get("package_managers");
                let package_managers: Vec<String> = serde_json::from_value(pm_value)?;
                Ok(Some(Self::client_from_row(row, package_managers)))
            }
            None => Ok(None),
        }
    }

    /// Map a `clients` row onto the shared Client model (Sprint 29 B1 added
    /// the update-count / reported-distro columns).
    fn client_from_row(row: sqlx::postgres::PgRow, package_managers: Vec<String>) -> Client {
        Client {
            id: ClientId(row.get("id")),
            hostname: row.get("hostname"),
            os: row.get("os"),
            os_version: row.get("os_version"),
            package_managers,
            last_seen: row.get("last_seen"),
            registered_at: row.get("registered_at"),
            cert_serial: row.get("cert_serial"),
            update_security: row.get("update_security"),
            update_other: row.get("update_other"),
            update_total: row.get("update_total"),
            update_reported_at: row.get("update_reported_at"),
            reported_distro: row.get("reported_distro"),
            reported_distro_version: row.get("reported_distro_version"),
            // Sprint 29 Phase 3: vendor preflight verdict (notes stored as
            // a JSON array string; unparsable content degrades to None).
            update_ready: row.get("update_ready"),
            update_readiness_notes: row
                .get::<Option<String>, _>("update_readiness_notes")
                .and_then(|s| serde_json::from_str(&s).ok()),
        }
    }

    #[allow(dead_code)] // Admin API query method
    pub async fn list_clients(&self) -> Result<Vec<Client>> {
        let rows = sqlx::query(
            "SELECT id, hostname, os, os_version, package_managers, last_seen, registered_at, cert_serial,
                    update_security, update_other, update_total, update_reported_at,
                    reported_distro, reported_distro_version,
                    update_ready, update_readiness_notes
             FROM clients ORDER BY registered_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut clients = Vec::with_capacity(rows.len());
        for row in rows {
            let pm_value: serde_json::Value = row.get("package_managers");
            let package_managers: Vec<String> = serde_json::from_value(pm_value)?;
            clients.push(Self::client_from_row(row, package_managers));
        }

        debug!("Listed {} clients", clients.len());
        Ok(clients)
    }

    /// Persist an UpdateReport from a heartbeat (Sprint 29 B1, grounding D1):
    /// counts + client-reported distro identity land as columns on the
    /// client row, stamped with the report time. Phase 3 adds the optional
    /// vendor-preflight verdict (ready flag + JSON-encoded reason notes).
    #[allow(clippy::too_many_arguments)]
    pub async fn record_update_report(
        &self,
        client_id: &ClientId,
        security: i32,
        other: i32,
        total: i32,
        distro: &str,
        distro_version: &str,
        ready: Option<bool>,
        readiness_notes: Option<&[String]>,
    ) -> Result<()> {
        let notes_json = readiness_notes
            .map(|notes| serde_json::to_string(notes).unwrap_or_else(|_| "[]".to_string()));
        let result = sqlx::query(
            "UPDATE clients SET
                 update_security = $1,
                 update_other = $2,
                 update_total = $3,
                 update_reported_at = now(),
                 reported_distro = $4,
                 reported_distro_version = $5,
                 update_ready = $6,
                 update_readiness_notes = $7
             WHERE id = $8",
        )
        .bind(security)
        .bind(other)
        .bind(total)
        .bind(distro)
        .bind(distro_version)
        .bind(ready)
        .bind(notes_json)
        .bind(client_id.0)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            warn!("record_update_report: unknown client {}", client_id);
        } else {
            debug!(
                "Recorded update report for client {}: security={} other={} distro={}:{}",
                client_id, security, other, distro, distro_version
            );
        }
        Ok(())
    }

    pub async fn count_clients(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) as count FROM clients")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get("count"))
    }

    /// Record the issued certificate serial on a client row (red-team F3:
    /// serial tracking enables per-cert revocation).
    #[allow(dead_code)] // Admin API / revocation path
    pub async fn update_client_cert_serial(
        &self,
        client_id: &ClientId,
        serial: &str,
    ) -> Result<()> {
        let result = sqlx::query("UPDATE clients SET cert_serial = $1 WHERE id = $2")
            .bind(serial)
            .bind(client_id.0)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            warn!("update_client_cert_serial: unknown client {}", client_id);
        } else {
            debug!("Recorded cert serial {} for client {}", serial, client_id);
        }
        Ok(())
    }

    /// Insert a newly issued client certificate into the tracking table
    /// (red-team F3: serial + expiry recorded for revocation).
    #[allow(dead_code)] // Admin API / revocation path
    pub async fn insert_client_cert(
        &self,
        serial: &str,
        client_id: &ClientId,
        issued_at: chrono::DateTime<Utc>,
        expires_at: chrono::DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO client_certs (serial, client_id, issued_at, expires_at, status)
             VALUES ($1, $2, $3, $4, 'active')",
        )
        .bind(serial)
        .bind(client_id.0)
        .bind(issued_at)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        debug!(
            "Inserted client_cert serial={} for client {}",
            serial, client_id
        );
        Ok(())
    }

    /// Look up a client certificate by serial (red-team F3: revocation check).
    #[allow(dead_code)] // Admin API / TLS accept-path revocation check
    pub async fn get_client_cert_by_serial(&self, serial: &str) -> Result<Option<ClientCert>> {
        let row = sqlx::query(
            "SELECT serial, client_id, issued_at, expires_at, status, revoked_at
             FROM client_certs WHERE serial = $1",
        )
        .bind(serial)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| ClientCert {
            serial: r.get("serial"),
            client_id: ClientId(r.get("client_id")),
            issued_at: r.get("issued_at"),
            expires_at: r.get("expires_at"),
            status: r.get("status"),
            revoked_at: r.get("revoked_at"),
        }))
    }

    /// Revoke a client certificate by serial (red-team F3: revoke API).
    #[allow(dead_code)] // Admin API / revocation path
    pub async fn revoke_client_cert(&self, serial: &str) -> Result<bool> {
        let now = Utc::now();
        let result = sqlx::query(
            "UPDATE client_certs SET status = 'revoked', revoked_at = $1
             WHERE serial = $2 AND status = 'active'",
        )
        .bind(now)
        .bind(serial)
        .execute(&self.pool)
        .await?;
        let revoked = result.rows_affected() > 0;
        if revoked {
            info!("Revoked client cert serial={}", serial);
        }
        Ok(revoked)
    }

    /// List all certificates for a client (admin dashboard / audit).
    #[allow(dead_code)] // Admin API query method
    pub async fn list_client_certs_for_client(
        &self,
        client_id: &ClientId,
    ) -> Result<Vec<ClientCert>> {
        let rows = sqlx::query(
            "SELECT serial, client_id, issued_at, expires_at, status, revoked_at
             FROM client_certs WHERE client_id = $1 ORDER BY issued_at DESC",
        )
        .bind(client_id.0)
        .fetch_all(&self.pool)
        .await?;

        let certs: Vec<ClientCert> = rows
            .iter()
            .map(|r| ClientCert {
                serial: r.get("serial"),
                client_id: ClientId(r.get("client_id")),
                issued_at: r.get("issued_at"),
                expires_at: r.get("expires_at"),
                status: r.get("status"),
                revoked_at: r.get("revoked_at"),
            })
            .collect();
        debug!(
            "Listed {} client certs for client {}",
            certs.len(),
            client_id
        );
        Ok(certs)
    }

    /// Check whether a certificate serial is revoked (TLS accept-path check).
    #[allow(dead_code)] // TLS accept-path revocation check
    pub async fn is_cert_revoked(&self, serial: &str) -> Result<bool> {
        let row = sqlx::query("SELECT status FROM client_certs WHERE serial = $1")
            .bind(serial)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some_and(|r| r.get::<String, _>("status") == "revoked"))
    }

    /// Return all currently revoked cert serials (for caching in the gRPC
    /// accept-path revocation set).
    #[allow(dead_code)] // Admin API / revocation path
    pub async fn get_revoked_cert_serials(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT serial FROM client_certs WHERE status = 'revoked'")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get("serial")).collect())
    }

    /// Highest issued client-cert serial as a u64, used to seed the in-memory
    /// serial counter after a restart so serials are never re-issued
    /// (red-team F3). Serials are stored as hex text, so the max is computed
    /// in Rust rather than relying on TEXT ordering.
    #[allow(dead_code)] // TlsManager startup seeding
    pub async fn max_client_cert_serial_u64(&self) -> Result<Option<u64>> {
        let rows = sqlx::query("SELECT serial FROM client_certs")
            .fetch_all(&self.pool)
            .await?;
        let mut max: Option<u64> = None;
        for r in rows {
            let serial: String = r.get("serial");
            if let Ok(v) = u64::from_str_radix(&serial, 16) {
                max = Some(max.map_or(v, |m| m.max(v)));
            }
        }
        Ok(max)
    }

    /// Record a pairing approval/deny audit entry (P4 approval audit view).
    #[allow(dead_code)] // Admin API / approval audit view
    pub async fn record_pair_approval_audit(
        &self,
        request_id: &str,
        action: &str,   // "approved" | "denied"
        approver: &str, // username or "system"
        detail: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO pair_approval_audit (request_id, action, approver, detail, created_at)
             VALUES ($1, $2, $3, $4, now())",
        )
        .bind(request_id)
        .bind(action)
        .bind(approver)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List pairing approval audit entries, newest first (P4 approval audit view).
    #[allow(dead_code)] // Admin API / approval audit view
    pub async fn list_pair_approval_audit(&self, limit: i64) -> Result<Vec<PairApprovalAuditRow>> {
        let rows = sqlx::query(
            "SELECT request_id, action, approver, detail, created_at
             FROM pair_approval_audit
             ORDER BY created_at DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let out: Vec<PairApprovalAuditRow> = rows
            .iter()
            .map(|r| PairApprovalAuditRow {
                request_id: r.get("request_id"),
                action: r.get("action"),
                approver: r.get("approver"),
                detail: r.get("detail"),
                created_at: r
                    .get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                    .timestamp(),
            })
            .collect();
        Ok(out)
    }

    // ─── Enrollment keys (P5) ─────────────────────────────────────────────

    /// Create an enrollment key. `key_hash` is the hex SHA-256 of the
    /// plaintext key; the plaintext is never stored. Returns the key id.
    #[allow(dead_code)] // Admin API / enrollment key path
    pub async fn create_enrollment_key(
        &self,
        key_hash: &str,
        label: &str,
        max_uses: i64,
        expires_at: Option<chrono::DateTime<Utc>>,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO enrollment_keys (id, key_hash, label, max_uses, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(key_hash)
        .bind(label)
        .bind(max_uses)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        info!(
            "Created enrollment key {} (label={}, max_uses={})",
            id, label, max_uses
        );
        Ok(id)
    }

    /// List all enrollment keys (dashboard key manager). Hashes are included
    /// for uniqueness display but plaintext keys are never recoverable.
    #[allow(dead_code)] // Admin API / enrollment key path
    pub async fn list_enrollment_keys(&self) -> Result<Vec<EnrollmentKeyRow>> {
        let rows = sqlx::query(
            "SELECT id, key_hash, label, max_uses, use_count, created_at, expires_at, revoked_at
             FROM enrollment_keys ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        let out: Vec<EnrollmentKeyRow> = rows
            .iter()
            .map(|r| EnrollmentKeyRow {
                id: r.get("id"),
                key_hash: r.get("key_hash"),
                label: r.get("label"),
                max_uses: r.get("max_uses"),
                use_count: r.get("use_count"),
                created_at: r.get::<chrono::DateTime<Utc>, _>("created_at").timestamp(),
                expires_at: r
                    .try_get::<Option<chrono::DateTime<Utc>>, _>("expires_at")
                    .ok()
                    .flatten()
                    .map(|t| t.timestamp()),
                revoked_at: r
                    .try_get::<Option<chrono::DateTime<Utc>>, _>("revoked_at")
                    .ok()
                    .flatten()
                    .map(|t| t.timestamp()),
            })
            .collect();
        Ok(out)
    }

    /// Revoke an enrollment key. Returns true if a live key was revoked.
    #[allow(dead_code)] // Admin API / enrollment key path
    pub async fn revoke_enrollment_key(&self, key_id: &Uuid) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE enrollment_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(key_id)
        .execute(&self.pool)
        .await?;
        let revoked = result.rows_affected() > 0;
        if revoked {
            info!("Revoked enrollment key {}", key_id);
        }
        Ok(revoked)
    }

    /// Validate a presented enrollment key by hash. Returns the key row when
    /// it exists, is not revoked, is not expired, and has uses remaining.
    #[allow(dead_code)] // Enrollment pairing path
    pub async fn validate_enrollment_key(
        &self,
        key_hash: &str,
    ) -> Result<Option<EnrollmentKeyRow>> {
        let row = sqlx::query(
            "SELECT id, key_hash, label, max_uses, use_count, created_at, expires_at, revoked_at
             FROM enrollment_keys
             WHERE key_hash = $1
               AND revoked_at IS NULL
               AND use_count < max_uses
               AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| EnrollmentKeyRow {
            id: r.get("id"),
            key_hash: r.get("key_hash"),
            label: r.get("label"),
            max_uses: r.get("max_uses"),
            use_count: r.get("use_count"),
            created_at: r.get::<chrono::DateTime<Utc>, _>("created_at").timestamp(),
            expires_at: r
                .try_get::<Option<chrono::DateTime<Utc>>, _>("expires_at")
                .ok()
                .flatten()
                .map(|t| t.timestamp()),
            revoked_at: r
                .try_get::<Option<chrono::DateTime<Utc>>, _>("revoked_at")
                .ok()
                .flatten()
                .map(|t| t.timestamp()),
        }))
    }

    /// Increment the use counter on an enrollment key after successful use.
    #[allow(dead_code)] // Enrollment pairing path
    pub async fn increment_enrollment_key_use(&self, key_id: &Uuid) -> Result<()> {
        sqlx::query("UPDATE enrollment_keys SET use_count = use_count + 1 WHERE id = $1")
            .bind(key_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<bool> {
        match sqlx::query("SELECT 1").fetch_one(&self.pool).await {
            Ok(_) => Ok(true),
            Err(e) => {
                warn!("Database ping failed: {}", e);
                Ok(false)
            }
        }
    }

    // --- Cache entry methods ---

    pub async fn record_cache_miss(&self, config: &CacheMissConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO cache_entries (url, cache_path, content_type, content_length, etag, last_modified, cached_at, last_accessed, hit_count, cache_class)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0, $9)
                     ON CONFLICT (url) DO UPDATE SET
                         cache_path = EXCLUDED.cache_path,
                         content_type = EXCLUDED.content_type,
                         content_length = EXCLUDED.content_length,
                         etag = EXCLUDED.etag,
                         last_modified = EXCLUDED.last_modified,
                         cached_at = EXCLUDED.cached_at,
                         last_accessed = EXCLUDED.last_accessed,
                         hit_count = 0,
                         cache_class = EXCLUDED.cache_class",
                )
                .bind(&config.url)
                .bind(&config.cache_path)
                .bind(&config.content_type)
                .bind(config.content_length)
                .bind(&config.etag)
                .bind(&config.last_modified)
                .bind(config.cached_at)
                .bind(config.cached_at)
                .bind(&config.cache_class)
        .execute(&self.pool)
        .await?;

        debug!("Recorded cache miss for: {}", config.url);
        Ok(())
    }

    pub async fn record_cache_hit(&self, url: &str, last_accessed: i64) -> Result<()> {
        let result = sqlx::query(
            "UPDATE cache_entries SET last_accessed = $1, hit_count = hit_count + 1 WHERE url = $2",
        )
        .bind(last_accessed)
        .bind(url)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            debug!("Cache hit recorded for unknown URL: {}", url);
        }
        Ok(())
    }

    /// Update the SHA256 checksum for a cached URL (tamper detection).
    pub async fn update_cache_sha256(&self, url: &str, sha256: &str) -> Result<()> {
        sqlx::query("UPDATE cache_entries SET sha256 = $1 WHERE url = $2")
            .bind(sha256)
            .bind(url)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_cache_entry(&self, url: &str) -> Result<Option<CacheEntryRow>> {
        let row = sqlx::query(
            "SELECT url, cache_path, content_type, content_length, etag, last_modified, cached_at, last_accessed, hit_count, sha256
             FROM cache_entries WHERE url = $1",
        )
        .bind(url)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| CacheEntryRow {
            url: r.get("url"),
            cache_path: r.get("cache_path"),
            content_type: r.get("content_type"),
            content_length: r.get("content_length"),
            etag: r.get("etag"),
            last_modified: r.get("last_modified"),
            cached_at: r.get("cached_at"),
            last_accessed: r.get("last_accessed"),
            hit_count: r.get("hit_count"),
            sha256: r.get("sha256"),
        }))
    }

    /// Record a shadow recipe remote-assignment audit entry (Sprint 22 Phase F).
    pub async fn record_shadow_assignment_audit(
        &self,
        client_id: Option<Uuid>,
        recipe_name: &str,
        outcome: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO shadow_assignment_audit (client_id, recipe_name, outcome, reason)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(client_id)
        .bind(recipe_name)
        .bind(outcome)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        debug!(
            "Shadow assignment audit: client={:?} recipe={} outcome={} reason={:?}",
            client_id, recipe_name, outcome, reason
        );
        Ok(())
    }

    /// Record a client-reported tamper alert. Returns the new row id.
    pub async fn record_tamper_alert(&self, alert: &TamperAlertInsert) -> Result<i64> {
        let row = sqlx::query(
            "INSERT INTO tamper_alerts (url, expected_sha256, actual_sha256, detail, client_id, observed_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id",
        )
        .bind(&alert.url)
        .bind(&alert.expected_sha256)
        .bind(&alert.actual_sha256)
        .bind(&alert.detail)
        .bind(&alert.client_id)
        .bind(alert.observed_at)
        .fetch_one(&self.pool)
        .await?;
        let id: i64 = row.get("id");
        warn!(
            "Tamper alert recorded: id={} url={} expected={} actual={}",
            id, alert.url, alert.expected_sha256, alert.actual_sha256
        );
        Ok(id)
    }

    /// True if any tamper alert has been recorded for the URL (quarantine check).
    pub async fn has_tamper_alert(&self, url: &str) -> Result<bool> {
        let row =
            sqlx::query("SELECT EXISTS(SELECT 1 FROM tamper_alerts WHERE url = $1) as present")
                .bind(url)
                .fetch_one(&self.pool)
                .await?;
        let present: bool = row.get("present");
        Ok(present)
    }

    /// List recent tamper alerts, newest first.
    pub async fn list_tamper_alerts(&self, limit: i64) -> Result<Vec<TamperAlertRow>> {
        let rows = sqlx::query(
            "SELECT id, url, expected_sha256, actual_sha256, detail, client_id, observed_at, recorded_at
             FROM tamper_alerts ORDER BY recorded_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let alerts: Vec<TamperAlertRow> = rows
            .iter()
            .map(|r| TamperAlertRow {
                id: r.get("id"),
                url: r.get("url"),
                expected_sha256: r.get("expected_sha256"),
                actual_sha256: r.get("actual_sha256"),
                detail: r.get("detail"),
                client_id: r.get("client_id"),
                observed_at: r.get("observed_at"),
                recorded_at: r.get("recorded_at"),
            })
            .collect();

        debug!("Listed {} tamper alerts", alerts.len());
        Ok(alerts)
    }

    /// Aggregate cache statistics from the database.
    pub async fn cache_stats(&self) -> Result<(u64, u64)> {
        let row = sqlx::query(
            "SELECT COUNT(*) as count, COALESCE(SUM(content_length), 0)::bigint as total_size FROM cache_entries",
        )
        .fetch_one(&self.pool)
        .await?;
        let count: i64 = row.get("count");
        let total_size: i64 = row.get("total_size");
        Ok((count as u64, total_size as u64))
    }

    #[allow(dead_code)] // Admin API cache listing
    pub async fn list_cache_entries(&self) -> Result<Vec<CacheEntryRow>> {
        let rows = sqlx::query(
            "SELECT url, cache_path, content_type, content_length, etag, last_modified, cached_at, last_accessed, hit_count, sha256
             FROM cache_entries ORDER BY cached_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        let entries: Vec<CacheEntryRow> = rows
            .iter()
            .map(|r| CacheEntryRow {
                url: r.get("url"),
                cache_path: r.get("cache_path"),
                content_type: r.get("content_type"),
                content_length: r.get("content_length"),
                etag: r.get("etag"),
                last_modified: r.get("last_modified"),
                cached_at: r.get("cached_at"),
                last_accessed: r.get("last_accessed"),
                hit_count: r.get("hit_count"),
                sha256: r.get("sha256"),
            })
            .collect();

        debug!("Listed {} cache entries", entries.len());
        Ok(entries)
    }

    #[allow(dead_code)] // Admin API cache management
    pub async fn delete_cache_entry(&self, url: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM cache_entries WHERE url = $1")
            .bind(url)
            .execute(&self.pool)
            .await?;
        let deleted = result.rows_affected() > 0;
        if deleted {
            debug!("Deleted cache entry for: {}", url);
        }
        Ok(deleted)
    }

    // --- API key methods ---

    pub async fn create_api_key(&self, label: &str, key_hash: &str) -> Result<ApiKeyId> {
        let id = Uuid::new_v4();
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO api_keys (id, label, key_hash, created_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(label)
        .bind(key_hash)
        .bind(now)
        .execute(&self.pool)
        .await?;

        info!("Created API key {} (label={})", id, label);
        Ok(ApiKeyId(id))
    }

    #[allow(dead_code)] // Will be used when API key auth is wired to mirror middleware
    pub async fn validate_api_key(&self, key_hash: &str) -> Result<Option<ApiKeyId>> {
        let row =
            sqlx::query("UPDATE api_keys SET last_used = $1 WHERE key_hash = $2 RETURNING id")
                .bind(Utc::now())
                .bind(key_hash)
                .fetch_optional(&self.pool)
                .await?;

        match row {
            Some(r) => {
                let id: Uuid = r.get("id");
                debug!("Validated API key {}", id);
                Ok(Some(ApiKeyId(id)))
            }
            None => Ok(None),
        }
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>> {
        let rows = sqlx::query(
            "SELECT id, label, key_hash, created_at, last_used FROM api_keys ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        let keys: Vec<ApiKey> = rows
            .iter()
            .map(|r| ApiKey {
                id: ApiKeyId(r.get("id")),
                label: r.get("label"),
                key_hash: r.get("key_hash"),
                created_at: r.get("created_at"),
                last_used: r.get("last_used"),
            })
            .collect();

        debug!("Listed {} API keys", keys.len());
        Ok(keys)
    }

    pub async fn revoke_api_key(&self, key_id: &ApiKeyId) -> Result<bool> {
        let result = sqlx::query("DELETE FROM api_keys WHERE id = $1")
            .bind(key_id.0)
            .execute(&self.pool)
            .await?;
        let deleted = result.rows_affected() > 0;
        if deleted {
            info!("Revoked API key {}", key_id);
        }
        Ok(deleted)
    }

    pub async fn record_pairing(
        &self,
        client_id: &ClientId,
        paired_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let result = sqlx::query("UPDATE clients SET paired_at = $1, last_seen = $1 WHERE id = $2")
            .bind(paired_at)
            .bind(client_id.0)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            warn!(
                "record_pairing: 0 rows affected for client {} — row was not created by register_client()",
                client_id
            );
        } else {
            info!("Recorded pairing for client {}", client_id);
        }
        Ok(())
    }

    // --- Package query methods ---

    #[allow(dead_code)] // Admin API query method
    pub async fn get_package(&self, package_id: &PackageId) -> Result<Option<Package>> {
        let row = sqlx::query(
            "SELECT id, manager, name, version, arch, file_path, hash, upstream_url, size_bytes, cached_at, last_accessed
             FROM packages WHERE id = $1",
        )
        .bind(package_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_package(&r)))
    }

    #[allow(dead_code)] // Admin API query method
    pub async fn list_packages(&self) -> Result<Vec<Package>> {
        let rows = sqlx::query(
            "SELECT id, manager, name, version, arch, file_path, hash, upstream_url, size_bytes, cached_at, last_accessed
             FROM packages ORDER BY cached_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        let packages: Vec<Package> = rows.iter().map(row_to_package).collect();
        debug!("Listed {} packages", packages.len());
        Ok(packages)
    }

    #[allow(dead_code)] // Admin API query method
    pub async fn get_packages_for_client(&self, client_id: &ClientId) -> Result<Vec<Package>> {
        let rows = sqlx::query(
            "SELECT p.id, p.manager, p.name, p.version, p.arch, p.file_path, p.hash, p.upstream_url, p.size_bytes, p.cached_at, p.last_accessed
             FROM packages p
             INNER JOIN client_packages cp ON p.id = cp.package_id
             WHERE cp.client_id = $1
             ORDER BY cp.installed_at DESC",
        )
        .bind(client_id.0)
        .fetch_all(&self.pool)
        .await?;

        let packages: Vec<Package> = rows.iter().map(row_to_package).collect();
        debug!(
            "Listed {} packages for client {}",
            packages.len(),
            client_id
        );
        Ok(packages)
    }

    #[allow(dead_code)] // Admin API query method
    pub async fn get_client_packages(&self, client_id: &ClientId) -> Result<Vec<ClientPackage>> {
        let rows = sqlx::query(
            "SELECT client_id, package_id, installed_at
             FROM client_packages WHERE client_id = $1
             ORDER BY installed_at DESC",
        )
        .bind(client_id.0)
        .fetch_all(&self.pool)
        .await?;

        let associations: Vec<ClientPackage> = rows
            .iter()
            .map(|r| ClientPackage {
                client_id: ClientId(r.get("client_id")),
                package_id: PackageId(r.get("package_id")),
                installed_at: r.get("installed_at"),
            })
            .collect();

        debug!(
            "Listed {} client-package associations for client {}",
            associations.len(),
            client_id
        );
        Ok(associations)
    }
}

/// Convert DB row to Package
#[allow(dead_code)] // Helper for DB query methods above
fn row_to_package(row: &sqlx::postgres::PgRow) -> Package {
    Package {
        id: PackageId(row.get("id")),
        manager: row.get("manager"),
        name: row.get("name"),
        version: row.get("version"),
        arch: row.get("arch"),
        file_path: row.get("file_path"),
        hash: row.get("hash"),
        upstream_url: row.get("upstream_url"),
        size_bytes: row.get("size_bytes"),
        cached_at: row.get("cached_at"),
        last_accessed: row.get("last_accessed"),
    }
}

#[allow(dead_code)] // DB row type for admin API cache listing
pub struct CacheEntryRow {
    pub url: String,
    pub cache_path: String,
    pub content_type: Option<String>,
    pub content_length: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cached_at: i64,
    pub last_accessed: i64,
    pub hit_count: i64,
    pub sha256: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
/// Mirror configuration stored in DB
#[allow(dead_code)] // DB row type for mirror gRPC handlers
pub struct MirrorConfigRow {
    pub distro: String,
    pub upstream_url: String,
    pub local_path: String,
    pub sync_enabled: bool,
    pub sync_interval_secs: i64,
    pub bandwidth_limit_bps: Option<i64>,
    pub include_patterns: serde_json::Value,
    pub exclude_patterns: serde_json::Value,
    pub public: bool,
    pub last_sync: Option<chrono::DateTime<chrono::Utc>>,
    pub sync_status: String,
    pub total_bytes: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Input type for bulk-upserting upstream package metadata.
#[derive(Debug, Clone)]
pub struct UpstreamPackage {
    pub name: String,
    pub version: String,
    pub architecture: String,
    pub filename: String,
    pub download_url: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
}

/// Row type for querying upstream package data.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UpstreamPackageRow {
    pub id: uuid::Uuid,
    pub distro: String,
    pub repo: String,
    pub name: String,
    pub version: String,
    pub architecture: String,
    pub filename: String,
    pub download_url: String,
    pub size_bytes: i64,
    pub sha256: Option<String>,
    pub indexed_at: chrono::DateTime<chrono::Utc>,
}

impl Database {
    pub async fn upsert_mirror_config(&self, config: &MirrorUpsertConfig) -> Result<()> {
        sqlx::query(
            "INSERT INTO mirror_configs (distro, upstream_url, local_path, sync_enabled, sync_interval_secs, bandwidth_limit_bps, include_patterns, exclude_patterns, public)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (distro) DO UPDATE SET
                 upstream_url = EXCLUDED.upstream_url,
                 local_path = EXCLUDED.local_path,
                 sync_enabled = EXCLUDED.sync_enabled,
                 sync_interval_secs = EXCLUDED.sync_interval_secs,
                 bandwidth_limit_bps = EXCLUDED.bandwidth_limit_bps,
                 include_patterns = EXCLUDED.include_patterns,
                 exclude_patterns = EXCLUDED.exclude_patterns,
                 public = EXCLUDED.public",
        )
        .bind(&config.distro)
        .bind(&config.upstream_url)
        .bind(&config.local_path)
        .bind(config.sync_enabled)
        .bind(config.sync_interval_secs)
        .bind(config.bandwidth_limit_bps)
        .bind(&config.include_patterns)
        .bind(&config.exclude_patterns)
        .bind(config.public)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[allow(dead_code)] // Will be used by mirror gRPC handlers
    pub async fn get_mirror_config(&self, distro: &str) -> Result<Option<MirrorConfigRow>> {
        let row =
            sqlx::query_as::<_, MirrorConfigRow>("SELECT * FROM mirror_configs WHERE distro = $1")
                .bind(distro)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row)
    }

    #[allow(dead_code)] // Will be used by mirror gRPC handlers
    pub async fn list_mirror_configs(&self) -> Result<Vec<MirrorConfigRow>> {
        let rows =
            sqlx::query_as::<_, MirrorConfigRow>("SELECT * FROM mirror_configs ORDER BY distro")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    #[allow(dead_code)] // Will be used by mirror gRPC handlers
    pub async fn update_mirror_sync_status(&self, distro: &str, status: &str) -> Result<()> {
        sqlx::query("UPDATE mirror_configs SET sync_status = $1 WHERE distro = $2")
            .bind(status)
            .bind(distro)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[allow(dead_code)] // Will be used by mirror gRPC handlers
    pub async fn update_mirror_last_sync(&self, distro: &str, total_bytes: i64) -> Result<()> {
        let now = Utc::now();
        sqlx::query(
            "UPDATE mirror_configs SET last_sync = $1, total_bytes = $2, sync_status = 'idle' WHERE distro = $3",
        )
        .bind(now)
        .bind(total_bytes)
        .bind(distro)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_mirror_config(&self, distro: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM mirror_configs WHERE distro = $1")
            .bind(distro)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    // ── Upstream package index CRUD ──

    /// Bulk upsert upstream package entries from repo metadata parsing.
    /// Replaces all entries for a given distro+repo atomically.
    pub async fn replace_upstream_packages(
        &self,
        distro: &str,
        repo: &str,
        packages: &[UpstreamPackage],
    ) -> Result<usize> {
        let mut tx = self.pool.begin().await?;

        // Delete old entries for this distro+repo
        sqlx::query("DELETE FROM upstream_packages WHERE distro = $1 AND repo = $2")
            .bind(distro)
            .bind(repo)
            .execute(&mut *tx)
            .await?;

        let mut count = 0usize;
        for pkg in packages {
            sqlx::query(
                "INSERT INTO upstream_packages (distro, repo, name, version, architecture, filename, download_url, size_bytes, sha256)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT (distro, repo, name, version, architecture) DO UPDATE SET
                     filename = EXCLUDED.filename,
                     download_url = EXCLUDED.download_url,
                     size_bytes = EXCLUDED.size_bytes,
                     sha256 = EXCLUDED.sha256,
                     indexed_at = NOW()",
            )
            .bind(distro)
            .bind(repo)
            .bind(&pkg.name)
            .bind(&pkg.version)
            .bind(&pkg.architecture)
            .bind(&pkg.filename)
            .bind(&pkg.download_url)
            .bind(pkg.size_bytes as i64)
            .bind(&pkg.sha256)
            .execute(&mut *tx)
            .await?;
            count += 1;
        }

        tx.commit().await?;
        info!("Indexed {} packages for {}/{}", count, distro, repo);
        Ok(count)
    }

    /// Get the latest upstream version for packages matching a distro and
    /// optional name filter. Returns the highest version per name+arch.
    pub async fn latest_upstream_versions(&self, distro: &str) -> Result<Vec<UpstreamPackageRow>> {
        let rows = sqlx::query_as::<_, UpstreamPackageRow>(
            "SELECT DISTINCT ON (name, architecture)
                    id, distro, repo, name, version, architecture, filename,
                    download_url, size_bytes, sha256, indexed_at
             FROM upstream_packages
             WHERE distro = $1
             ORDER BY name, architecture, indexed_at DESC",
        )
        .bind(distro)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Count upstream packages for a distro.
    pub async fn count_upstream_packages(&self, distro: &str) -> Result<i64> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM upstream_packages WHERE distro = $1")
                .bind(distro)
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    /// Get all (name, version) pairs that clients have installed for a given distro.
    /// Used by inventory-aware prefetch to determine what needs updating.
    /// The distro parameter maps to a package manager: archlinux→pacman, debian→apt.
    pub async fn get_all_client_package_versions(
        &self,
        distro: &str,
    ) -> Result<Vec<(String, String)>> {
        let manager = match distro {
            "archlinux" => "pacman",
            "debian" | "ubuntu" => "apt",
            "fedora" => "dnf",
            _ => distro,
        };

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT p.name, p.version
             FROM packages p
             INNER JOIN client_packages cp ON p.id = cp.package_id
             WHERE p.manager = $1 AND p.version != ''
             ORDER BY p.name",
        )
        .bind(manager)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    // ── Shadow package CRUD ──

    #[allow(clippy::too_many_arguments)]
    pub async fn create_shadow_package(
        &self,
        client_id: &uuid::Uuid,
        name: &str,
        version: &str,
        architecture: &str,
        os_family: &str,
        libc: Option<&str>,
        source_type: &str,
        source_url: &str,
        recipe_path: &str,
        install_paths: &serde_json::Value,
        config_paths: Option<&serde_json::Value>,
        service_name: Option<&str>,
        masked_from_pm: Option<&str>,
        binary_sha256: Option<&str>,
    ) -> Result<uuid::Uuid> {
        let id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO shadow_packages (id, client_id, name, version, architecture, os_family, libc, source_type, source_url, recipe_path, install_paths, config_paths, service_name, masked_from_pm, binary_sha256)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)"
        )
        .bind(id)
        .bind(client_id)
        .bind(name)
        .bind(version)
        .bind(architecture)
        .bind(os_family)
        .bind(libc)
        .bind(source_type)
        .bind(source_url)
        .bind(recipe_path)
        .bind(install_paths)
        .bind(config_paths)
        .bind(service_name)
        .bind(masked_from_pm)
        .bind(binary_sha256)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn get_shadow_package(
        &self,
        id: &uuid::Uuid,
    ) -> Result<Option<sqlx::types::Json<serde_json::Value>>> {
        let row = sqlx::query_as::<_, (sqlx::types::Json<serde_json::Value>,)>(
            "SELECT row_to_json(shadow_packages.*) FROM shadow_packages WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }

    pub async fn list_shadow_packages_for_client(
        &self,
        client_id: &uuid::Uuid,
    ) -> Result<Vec<sqlx::types::Json<serde_json::Value>>> {
        let rows = sqlx::query_as::<_, (sqlx::types::Json<serde_json::Value>,)>(
            "SELECT row_to_json(shadow_packages.*) FROM shadow_packages WHERE client_id = $1 ORDER BY name"
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Recipe TOML for one catalog recipe, by name (update-check source dispatch).
    pub async fn get_shadow_recipe_toml(&self, name: &str) -> Result<Option<String>> {
        let row = sqlx::query_as::<_, (Option<String>,)>(
            "SELECT recipe_toml FROM shadow_recipes WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| r.0))
    }

    pub async fn update_shadow_package_version(
        &self,
        id: &uuid::Uuid,
        version: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE shadow_packages SET version = $1, last_updated_at = now() WHERE id = $2",
        )
        .bind(version)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_shadow_package_checked(&self, id: &uuid::Uuid) -> Result<()> {
        sqlx::query("UPDATE shadow_packages SET last_checked_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_shadow_package(&self, id: &uuid::Uuid) -> Result<bool> {
        let result = sqlx::query("DELETE FROM shadow_packages WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// P4a: QueryShadowRegistry backing query. Matches shadow packages by
    /// binary_sha256 (exact content address) or by recipe name with an
    /// optional service_name similarity hint, or by source_url, and assembles
    /// fleet installs (hostname, version, install_paths, last_seen) from the
    /// clients table. Signing state comes from the shadow_recipes catalog.
    pub async fn query_shadow_registry(
        &self,
        binary_sha256: Option<&str>,
        name: Option<&str>,
        service_name: Option<&str>,
        source_url: Option<&str>,
    ) -> Result<Vec<ShadowRegistryMatchRow>> {
        let base = "SELECT sp.name AS recipe_name, sp.version AS recipe_version,
             sp.install_paths::text AS install_paths, c.hostname AS hostname,
             c.last_seen AS last_seen, sr.signature AS signature, sr.key_id AS key_id
          FROM shadow_packages sp
          LEFT JOIN clients c ON c.id = sp.client_id
          LEFT JOIN shadow_recipes sr ON sr.name = sp.name";
        let rows: Vec<_> = match (binary_sha256, name, source_url) {
            (Some(hash), ..) => {
                sqlx::query(&format!("{} WHERE sp.binary_sha256 = $1", base))
                    .bind(hash)
                    .fetch_all(&self.pool)
                    .await?
            }
            (None, Some(n), ..) => {
                let svc = service_name.unwrap_or("");
                sqlx::query(&format!(
                    "{} WHERE sp.name = $1 AND ($2 = '' OR sp.service_name = $2)",
                    base
                ))
                .bind(n)
                .bind(svc)
                .fetch_all(&self.pool)
                .await?
            }
            (None, None, Some(url)) => {
                sqlx::query(&format!("{} WHERE sp.source_url = $1", base))
                    .bind(url)
                    .fetch_all(&self.pool)
                    .await?
            }
            (None, None, None) => Vec::new(),
        };

        let mut matches: Vec<ShadowRegistryMatchRow> = Vec::new();
        for row in rows {
            let recipe_name: String = row.get("recipe_name");
            let recipe_version: String = row.get("recipe_version");
            let install_paths: Vec<String> =
                serde_json::from_str(&row.get::<String, _>("install_paths")).unwrap_or_default();
            let hostname: Option<String> = row.get("hostname");
            let last_seen: Option<chrono::DateTime<Utc>> = row.get("last_seen");
            let signature: Option<String> = row.get("signature");
            let key_id: Option<String> = row.get("key_id");

            let fleet = FleetInstallRow {
                hostname: hostname.unwrap_or_default(),
                version: recipe_version.clone(),
                install_paths,
                last_seen: last_seen.map(|dt| dt.timestamp()).unwrap_or(0),
            };

            let signed = signature.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
                && key_id.as_deref().map(|k| k != "unsigned").unwrap_or(false);

            match matches
                .iter_mut()
                .find(|m| m.recipe_name == recipe_name && m.recipe_version == recipe_version)
            {
                Some(m) => m.fleet_installs.push(fleet),
                None => matches.push(ShadowRegistryMatchRow {
                    recipe_name,
                    recipe_version,
                    signed,
                    key_id: key_id.unwrap_or_default(),
                    fleet_installs: vec![fleet],
                }),
            }
        }
        Ok(matches)
    }

    pub async fn record_shadow_package_version(
        &self,
        package_id: &uuid::Uuid,
        version: &str,
        tag: Option<&str>,
        release_notes_url: Option<&str>,
        assets: Option<&serde_json::Value>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO shadow_package_versions (package_id, version, tag, release_notes_url, assets)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (package_id, version) DO UPDATE SET assets = COALESCE(EXCLUDED.assets, shadow_package_versions.assets)"
        )
        .bind(package_id)
        .bind(version)
        .bind(tag)
        .bind(release_notes_url)
        .bind(assets)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_shadow_update(
        &self,
        package_id: &uuid::Uuid,
        from_version: &str,
        to_version: &str,
        status: &str,
        backup_path: Option<&str>,
    ) -> Result<uuid::Uuid> {
        let id = uuid::Uuid::new_v4();
        let applied_at = if status == "applied" { "now()" } else { "NULL" };
        sqlx::query(&format!(
            "INSERT INTO shadow_update_history (id, package_id, from_version, to_version, status, backup_path, applied_at)
             VALUES ($1, $2, $3, $4, $5, $6, {applied_at})"
        ))
        .bind(id)
        .bind(package_id)
        .bind(from_version)
        .bind(to_version)
        .bind(status)
        .bind(backup_path)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn update_shadow_update_status(
        &self,
        id: &uuid::Uuid,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let applied = if status == "applied" {
            ", applied_at = now()"
        } else {
            ""
        };
        let rolled = if status == "rolled_back" {
            ", rolled_back_at = now()"
        } else {
            ""
        };
        sqlx::query(&format!(
            "UPDATE shadow_update_history SET status = $1, error_message = $2{applied}{rolled} WHERE id = $3"
        ))
        .bind(status)
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ========================================================================
    // Scan Results
    // ========================================================================

    /// Record a scan result for a cached package.
    pub async fn record_scan_result(&self, record: &ScanResultRecord) -> Result<()> {
        let yara_json = record.yara_matches.as_ref().map(|v| v.to_string());
        let cve_json = record.cve_matches.as_ref().map(|v| v.to_string());

        sqlx::query(
            "INSERT INTO cache_scan_results
                (cache_path, package_name, package_version, scan_version,
                 clamav_result, yara_matches, max_entropy, cve_matches,
                 scan_status, blocked_reason, scanned_at)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7, $8::jsonb, $9, $10, $11)",
        )
        .bind(&record.cache_path)
        .bind(&record.package_name)
        .bind(&record.package_version)
        .bind(record.scan_version)
        .bind(&record.clamav_result)
        .bind(&yara_json)
        .bind(record.max_entropy)
        .bind(&cve_json)
        .bind(&record.scan_status)
        .bind(&record.blocked_reason)
        .bind(record.scanned_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get the latest scan result for a cache path.
    pub async fn get_latest_scan_result(&self, cache_path: &str) -> Result<Option<ScanResultRow>> {
        let row = sqlx::query(
            "SELECT cache_path, package_name, package_version, scan_version,
                    clamav_result, yara_matches, max_entropy, cve_matches,
                    scan_status, blocked_reason, scanned_at
             FROM cache_scan_results
             WHERE cache_path = $1
             ORDER BY scan_version DESC LIMIT 1",
        )
        .bind(cache_path)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => {
                let yara: Option<serde_json::Value> = r.try_get("yara_matches").unwrap_or(None);
                let cve: Option<serde_json::Value> = r.try_get("cve_matches").unwrap_or(None);
                Ok(Some(ScanResultRow {
                    cache_path: r.get("cache_path"),
                    package_name: r.try_get("package_name").unwrap_or(None),
                    package_version: r.try_get("package_version").unwrap_or(None),
                    scan_version: r.get("scan_version"),
                    clamav_result: r.try_get("clamav_result").unwrap_or(None),
                    yara_matches: yara,
                    max_entropy: r.try_get("max_entropy").unwrap_or(None),
                    cve_matches: cve,
                    scan_status: r.get("scan_status"),
                    blocked_reason: r.try_get("blocked_reason").unwrap_or(None),
                    scanned_at: r.get("scanned_at"),
                }))
            }
            None => Ok(None),
        }
    }

    // ─── Server Settings ──────────────────────────────────────────────

    /// Get a JSON setting by key. Returns None if not found.
    pub async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let row = sqlx::query("SELECT value FROM server_settings WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;

        match row {
            Some(r) => {
                let value: serde_json::Value = r.try_get("value")?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Set a JSON setting by key. Upserts (insert or update).
    pub async fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        sqlx::query(
            "INSERT INTO server_settings (key, value, updated_at)
             VALUES ($1, $2, now())
             ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ─── Language PM enablement (Sprint 23 §2.4) ──────────────────────

    /// Read the `language_pms` runtime setting: a map of PM name →
    /// {enabled, source}. Returns an empty map when unset (all disabled
    /// by default; the proxy auto-enables on first matching request).
    pub async fn get_language_pms(
        &self,
    ) -> Result<std::collections::HashMap<String, LanguagePmState>> {
        let mut out = std::collections::HashMap::new();
        if let Some(v) = self.get_setting(LANGUAGE_PMS_KEY).await? {
            if let Some(obj) = v.as_object() {
                for (pm, state) in obj {
                    if let (Some(enabled), Some(source)) = (
                        state.get("enabled").and_then(|e| e.as_bool()),
                        state.get("source").and_then(|s| s.as_str()),
                    ) {
                        out.insert(
                            pm.clone(),
                            LanguagePmState {
                                enabled,
                                source: source.to_string(),
                            },
                        );
                    }
                }
            }
        }
        Ok(out)
    }

    /// Write the full `language_pms` map (dashboard toggle, auto-enable).
    pub async fn set_language_pms(
        &self,
        states: &std::collections::HashMap<String, LanguagePmState>,
    ) -> Result<()> {
        let mut obj = serde_json::Map::new();
        for (pm, state) in states {
            obj.insert(
                pm.clone(),
                serde_json::json!({"enabled": state.enabled, "source": state.source}),
            );
        }
        self.set_setting(LANGUAGE_PMS_KEY, &serde_json::Value::Object(obj))
            .await
    }

    /// Record one enablement transition in the audit trail.
    pub async fn record_pm_enablement_audit(
        &self,
        pm: &str,
        outcome: &str,
        source: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO pm_enablement_audit (pm, outcome, source, reason)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(pm)
        .bind(outcome)
        .bind(source)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        debug!(
            "PM enablement audit: pm={} outcome={} source={} reason={:?}",
            pm, outcome, source, reason
        );
        Ok(())
    }

    /// Set the cache class on one cache entry (Sprint 23 classification).
    pub async fn update_cache_class(&self, url: &str, cache_class: &str) -> Result<()> {
        sqlx::query("UPDATE cache_entries SET cache_class = $1 WHERE url = $2")
            .bind(cache_class)
            .bind(url)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// List cache entries of one class with size + last_accessed, oldest
    /// access first. Retention/scrubber janitor query surface.
    pub async fn list_cache_entries_by_class(
        &self,
        cache_class: &str,
    ) -> Result<Vec<(String, i64, i64)>> {
        let rows = sqlx::query(
            "SELECT url, content_length, last_accessed FROM cache_entries
             WHERE cache_class = $1 ORDER BY last_accessed ASC",
        )
        .bind(cache_class)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<String, _>("url"),
                    r.get::<i64, _>("content_length"),
                    r.get::<i64, _>("last_accessed"),
                )
            })
            .collect())
    }

    /// Append one observation to the sourcedb observation log. The log is
    /// append-only: duplicates by (package_key, source, version) are resolved
    /// at read time via floor_for_package / merge, never mutated here.
    // 8 scalar fields mirror the observations table columns 1:1 — a params
    // struct would just move the arity, not reduce it.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_observation(
        &self,
        package_key: &str,
        source: &str,
        version: &str,
        version_ord: &str,
        scheme: &str,
        observed_at: i64,
        observed_by: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO observations (package_key, source, version, version_ord, scheme, observed_at, observed_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(package_key)
        .bind(source)
        .bind(version)
        .bind(version_ord)
        .bind(scheme)
        .bind(observed_at)
        .bind(observed_by)
        .execute(&self.pool)
        .await?;
        debug!("Recorded observation for {}", package_key);
        Ok(())
    }

    /// The floor (max by version_ord) observation for a package_key.
    /// version_ord is lexicographically sortable, so plain DESC string order.
    pub async fn floor_for_package(&self, package_key: &str) -> Result<Option<ObservationRow>> {
        let row = sqlx::query(
            "SELECT package_key, source, version, version_ord, scheme, observed_at, observed_by
             FROM observations WHERE package_key = $1
             ORDER BY version_ord DESC LIMIT 1",
        )
        .bind(package_key)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| ObservationRow {
            package_key: r.get("package_key"),
            source: r.get("source"),
            version: r.get("version"),
            version_ord: r.get("version_ord"),
            scheme: r.get("scheme"),
            observed_at: r.get("observed_at"),
            observed_by: r.get("observed_by"),
        }))
    }

    /// All floors — the max-by-version_ord observation for EVERY package_key
    /// — for oracle delta generation. Companion to floor_for_package.
    pub async fn all_floors(&self) -> Result<Vec<ObservationRow>> {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (package_key)
                    package_key, source, version, version_ord, scheme, observed_at, observed_by
             FROM observations
             ORDER BY package_key, version_ord DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ObservationRow {
                package_key: r.get("package_key"),
                source: r.get("source"),
                version: r.get("version"),
                version_ord: r.get("version_ord"),
                scheme: r.get("scheme"),
                observed_at: r.get("observed_at"),
                observed_by: r.get("observed_by"),
            })
            .collect())
    }

    /// MAX(resolved_at) from package_map — freshest oracle resolution time
    /// (unix ms), for the dashboard stat + delta metadata. None when empty.
    pub async fn oracle_last_resolved(&self) -> Result<Option<i64>> {
        let row: (Option<i64>,) = sqlx::query_as("SELECT MAX(resolved_at) FROM package_map")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// Upsert per-key resolution state.
    pub async fn upsert_package_map(
        &self,
        package_key: &str,
        source: Option<&str>,
        resolved_at: Option<i64>,
        resolve_method: Option<&str>,
        etag: Option<&str>,
        last_poll: Option<i64>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO package_map (package_key, source, resolved_at, resolve_method, etag, last_poll)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (package_key) DO UPDATE SET
                 source = EXCLUDED.source,
                 resolved_at = EXCLUDED.resolved_at,
                 resolve_method = EXCLUDED.resolve_method,
                 etag = EXCLUDED.etag,
                 last_poll = EXCLUDED.last_poll",
        )
        .bind(package_key)
        .bind(source)
        .bind(resolved_at)
        .bind(resolve_method)
        .bind(etag)
        .bind(last_poll)
        .execute(&self.pool)
        .await?;
        debug!("Upserted package_map for {}", package_key);
        Ok(())
    }

    /// Fetch per-key resolution state, if any.
    pub async fn get_package_map(&self, package_key: &str) -> Result<Option<PackageMapRow>> {
        let row = sqlx::query(
            "SELECT package_key, source, resolved_at, resolve_method, etag, last_poll
             FROM package_map WHERE package_key = $1",
        )
        .bind(package_key)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| PackageMapRow {
            package_key: r.get("package_key"),
            source: r.get("source"),
            resolved_at: r.get("resolved_at"),
            resolve_method: r.get("resolve_method"),
            etag: r.get("etag"),
            last_poll: r.get("last_poll"),
        }))
    }

    // -----------------------------------------------------------------------
    // Shadow recipe catalog (Sprint 22, 22-C1)
    // -----------------------------------------------------------------------

    /// Insert or update a server-global shadow recipe catalog entry.
    pub async fn upsert_shadow_recipe(
        &self,
        name: &str,
        recipe_toml: &str,
        signature: &str,
        key_id: &str,
        catalog_hash: &str,
        provenance_client_id: Option<&uuid::Uuid>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO shadow_recipes (name, recipe_toml, signature, key_id, catalog_hash, provenance_client_id, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, now())
             ON CONFLICT (name) DO UPDATE SET
                 recipe_toml = EXCLUDED.recipe_toml,
                 signature = EXCLUDED.signature,
                 key_id = EXCLUDED.key_id,
                 catalog_hash = EXCLUDED.catalog_hash,
                 provenance_client_id = EXCLUDED.provenance_client_id,
                 updated_at = now()",
        )
        .bind(name)
        .bind(recipe_toml)
        .bind(signature)
        .bind(key_id)
        .bind(catalog_hash)
        .bind(provenance_client_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List every shadow recipe catalog entry, ordered by name.
    pub async fn list_shadow_catalog(&self) -> Result<Vec<ShadowRecipeRow>> {
        let rows = sqlx::query(
            "SELECT name, recipe_toml, signature, key_id, catalog_hash, published_at, provenance_client_id
             FROM shadow_recipes ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ShadowRecipeRow {
                name: r.get("name"),
                recipe_toml: r.get("recipe_toml"),
                signature: r.get("signature"),
                key_id: r.get("key_id"),
                catalog_hash: r.get("catalog_hash"),
                published_at: r
                    .get::<chrono::DateTime<Utc>, _>("published_at")
                    .timestamp(),
                provenance_client_id: r.get("provenance_client_id"),
            })
            .collect())
    }

    /// Insert or update a per-client shadow scan finding.
    pub async fn upsert_shadow_scan_result(
        &self,
        client_id: &uuid::Uuid,
        recipe_name: &str,
        installed: bool,
        detected_version: Option<&str>,
        method: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO shadow_scan_results (client_id, recipe_name, installed, detected_version, method, scanned_at)
             VALUES ($1, $2, $3, $4, $5, now())
             ON CONFLICT (client_id, recipe_name) DO UPDATE SET
                 installed = EXCLUDED.installed,
                 detected_version = EXCLUDED.detected_version,
                 method = EXCLUDED.method,
                 scanned_at = now()",
        )
        .bind(client_id)
        .bind(recipe_name)
        .bind(installed)
        .bind(detected_version)
        .bind(method)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List all shadow scan results (dashboard coverage view).
    pub async fn list_shadow_scan_results(&self) -> Result<Vec<ShadowScanResultRow>> {
        let rows = sqlx::query(
            "SELECT id, client_id, recipe_name, installed, detected_version, method, scanned_at
             FROM shadow_scan_results ORDER BY recipe_name, client_id",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ShadowScanResultRow {
                id: r.get("id"),
                client_id: r.get("client_id"),
                recipe_name: r.get("recipe_name"),
                installed: r.get("installed"),
                detected_version: r.get("detected_version"),
                method: r.get("method"),
                scanned_at: r.get::<chrono::DateTime<Utc>, _>("scanned_at").timestamp(),
            })
            .collect())
    }

    // ── Pending shadow assignments (Sprint 22 slice 22-E2a) ──

    /// Queue a shadow recipe for push to a client. Upsert: re-assigning an
    /// existing (client, recipe) pair resets the row to 'pending' and clears
    /// delivery/resolution timestamps.
    pub async fn create_pending_assignment(
        &self,
        client_id: Uuid,
        recipe_name: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO pending_shadow_assignments (client_id, recipe_name, status)
             VALUES ($1, $2, 'pending')
             ON CONFLICT (client_id, recipe_name)
             DO UPDATE SET status = 'pending', delivered_at = NULL, resolved_at = NULL",
        )
        .bind(client_id)
        .bind(recipe_name)
        .execute(&self.pool)
        .await?;
        debug!(
            "Pending shadow assignment created: client={} recipe={}",
            client_id, recipe_name
        );
        Ok(())
    }

    /// Fetch and mark-delivery pending assignments for a client.
    /// Rows keep status 'pending' (resolution happens via
    /// complete_pending_assignment when the client acks); delivered_at
    /// records the push. Only never-delivered rows are returned — a
    /// delivered-but-un-acked row must not re-push every tick (caught
    /// live: re-delivery created 1800+ duplicate registrations).
    pub async fn take_pending_assignments(
        &self,
        client_id: Uuid,
    ) -> Result<Vec<PendingAssignmentRow>> {
        let rows = sqlx::query(
            "UPDATE pending_shadow_assignments
             SET delivered_at = now()
             WHERE client_id = $1 AND status = 'pending' AND delivered_at IS NULL
             RETURNING id, client_id, recipe_name, status, created_at, delivered_at, resolved_at",
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await?;

        let out: Vec<PendingAssignmentRow> = rows
            .into_iter()
            .map(|r| PendingAssignmentRow {
                id: r.get("id"),
                client_id: r.get("client_id"),
                recipe_name: r.get("recipe_name"),
                status: r.get("status"),
                created_at: r.get::<chrono::DateTime<Utc>, _>("created_at").timestamp(),
                delivered_at: r
                    .try_get::<Option<chrono::DateTime<Utc>>, _>("delivered_at")
                    .ok()
                    .flatten()
                    .map(|t| t.timestamp()),
                resolved_at: r
                    .try_get::<Option<chrono::DateTime<Utc>>, _>("resolved_at")
                    .ok()
                    .flatten()
                    .map(|t| t.timestamp()),
            })
            .collect();
        debug!(
            "Took {} pending shadow assignments for client {}",
            out.len(),
            client_id
        );
        Ok(out)
    }

    /// Mark an assignment resolved: status 'applied' | 'refused'.
    pub async fn complete_pending_assignment(
        &self,
        client_id: Uuid,
        recipe_name: &str,
        status: &str,
    ) -> Result<()> {
        debug_assert!(status == "applied" || status == "refused");
        sqlx::query(
            "UPDATE pending_shadow_assignments
             SET status = $3, resolved_at = now()
             WHERE client_id = $1 AND recipe_name = $2",
        )
        .bind(client_id)
        .bind(recipe_name)
        .bind(status)
        .execute(&self.pool)
        .await?;
        debug!(
            "Pending shadow assignment completed: client={} recipe={} status={}",
            client_id, recipe_name, status
        );
        Ok(())
    }

    /// Create a shadow staging row for a client, derived from a signed recipe.
    /// Returns Err if a staging row already exists for this (client, recipe).
    pub async fn create_shadow_staging(
        &self,
        client_id: Uuid,
        recipe_name: &str,
        suggested_target_dir: &str,
        suggested_accessory_dirs: &[String],
        suggested_firewall_rules: &[FirewallRuleRow],
        suggested_health_check: Option<&str>,
    ) -> Result<()> {
        let acc_json = serde_json::to_value(suggested_accessory_dirs)
            .map_err(|e| anyhow::anyhow!("serializing accessory dirs: {e}"))?;
        let fw_json = serde_json::to_value(suggested_firewall_rules)
            .map_err(|e| anyhow::anyhow!("serializing firewall rules: {e}"))?;
        // suggested_health_check is a JSONB column: parse the string into a
        // JSON value before binding (S30 live-bench fix — a plain &str bind
        // fails against jsonb, and the old to_value(&str) double-encoded the
        // payload as a JSON string scalar).
        let hc_json: Option<serde_json::Value> =
            suggested_health_check.and_then(|s| serde_json::from_str(s).ok());

        sqlx::query(
            "INSERT INTO shadow_staging (client_id, recipe_name, status, suggested_target_dir,
                 suggested_accessory_dirs, suggested_firewall_rules, suggested_health_check)
             VALUES ($1, $2, 'pending', $3, $4, $5, $6)
             ON CONFLICT (client_id, recipe_name) DO NOTHING",
        )
        .bind(client_id)
        .bind(recipe_name)
        .bind(suggested_target_dir)
        .bind(acc_json)
        .bind(fw_json)
        .bind(hc_json)
        .execute(&self.pool)
        .await?;

        debug!(
            "Shadow staging created: client={} recipe={} target={}",
            client_id, recipe_name, suggested_target_dir
        );
        Ok(())
    }

    /// List all staging rows for a client, newest first.
    pub async fn list_shadow_staging_for_client(
        &self,
        client_id: Uuid,
    ) -> Result<Vec<ShadowStagingRow>> {
        let rows = sqlx::query(
            "SELECT id, client_id, recipe_name, status, suggested_target_dir,
                    suggested_accessory_dirs, suggested_firewall_rules, suggested_health_check,
                    client_target_dir, client_accessory_dirs, client_firewall_rules,
                    client_health_check, created_at, reviewed_at, resolved_at
             FROM shadow_staging
             WHERE client_id = $1
             ORDER BY created_at DESC",
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await?;

        let out: Vec<ShadowStagingRow> = rows
            .into_iter()
            .map(|r| {
                let acc: Vec<String> = r
                    .try_get::<Option<serde_json::Value>, _>("suggested_accessory_dirs")
                    .ok()
                    .flatten()
                    .map(|v| serde_json::from_value(v).unwrap_or_default())
                    .unwrap_or_default();
                let fw: Vec<(u16, String, String, String)> = r
                    .try_get::<Option<serde_json::Value>, _>("suggested_firewall_rules")
                    .ok()
                    .flatten()
                    .map(|v| {
                        serde_json::from_value::<Vec<(u16, String, String, String)>>(v)
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                let client_acc: Vec<String> = r
                    .try_get::<Option<serde_json::Value>, _>("client_accessory_dirs")
                    .ok()
                    .flatten()
                    .map(|v| serde_json::from_value(v).unwrap_or_default())
                    .unwrap_or_default();
                let client_fw: Vec<(u16, String, String, String)> = r
                    .try_get::<Option<serde_json::Value>, _>("client_firewall_rules")
                    .ok()
                    .flatten()
                    .map(|v| {
                        serde_json::from_value::<Vec<(u16, String, String, String)>>(v)
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                ShadowStagingRow {
                    id: r.get("id"),
                    client_id: r.try_get("client_id").ok().flatten(),
                    recipe_name: r.get("recipe_name"),
                    status: r.get("status"),
                    suggested_target_dir: r.get("suggested_target_dir"),
                    suggested_accessory_dirs: acc,
                    suggested_firewall_rules: fw,
                    suggested_health_check: r
                        .try_get::<Option<serde_json::Value>, _>("suggested_health_check")
                        .ok()
                        .flatten()
                        .map(|v| v.to_string()),
                    client_target_dir: r
                        .try_get::<Option<String>, _>("client_target_dir")
                        .ok()
                        .flatten(),
                    client_accessory_dirs: client_acc,
                    client_firewall_rules: client_fw,
                    client_health_check: r
                        .try_get::<Option<serde_json::Value>, _>("client_health_check")
                        .ok()
                        .flatten()
                        .map(|v| v.to_string()),
                    created_at: r
                        .get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                        .timestamp(),
                    reviewed_at: r
                        .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("reviewed_at")
                        .ok()
                        .flatten()
                        .map(|t| t.timestamp()),
                    resolved_at: r
                        .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("resolved_at")
                        .ok()
                        .flatten()
                        .map(|t| t.timestamp()),
                }
            })
            .collect();
        Ok(out)
    }

    /// Resolve a staging row: mark approved or rejected, store client overrides,
    /// and record the resolution timestamp.
    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_shadow_staging(
        &self,
        client_id: Uuid,
        recipe_name: &str,
        status: &str,
        client_target_dir: Option<&str>,
        client_accessory_dirs: &[String],
        client_firewall_rules: &[(u16, String, String, String)],
        client_health_check: Option<&str>,
    ) -> Result<()> {
        debug_assert!(status == "approved" || status == "rejected");
        let acc_json = serde_json::to_value(client_accessory_dirs)
            .map_err(|e| anyhow::anyhow!("serializing accessory dirs: {e}"))?;
        let fw_json: Vec<serde_json::Value> = client_firewall_rules
            .iter()
            .map(|(port, protocol, direction, description)| {
                serde_json::json!({"port": port, "protocol": protocol, "direction": direction, "description": description})
            })
            .collect();
        let fw_json = serde_json::to_value(&fw_json)
            .map_err(|e| anyhow::anyhow!("serializing firewall rules: {e}"))?;
        // client_health_check is a JSONB column: parse before binding (S30
        // live-bench fix — binding &str against jsonb fails at runtime).
        let hc_json: Option<serde_json::Value> =
            client_health_check.and_then(|s| serde_json::from_str(s).ok());

        sqlx::query(
            "UPDATE shadow_staging
             SET status = $3, reviewed_at = now(), resolved_at = now(),
                 client_target_dir = $4, client_accessory_dirs = $5,
                 client_firewall_rules = $6, client_health_check = $7
             WHERE client_id = $1 AND recipe_name = $2",
        )
        .bind(client_id)
        .bind(recipe_name)
        .bind(status)
        .bind(client_target_dir)
        .bind(acc_json)
        .bind(fw_json)
        .bind(hc_json)
        .execute(&self.pool)
        .await?;

        debug!(
            "Shadow staging resolved: client={} recipe={} status={}",
            client_id, recipe_name, status
        );
        Ok(())
    }
}

/// Row from observations (Sprint 21 sourcedb observation log).
#[derive(Debug, Clone)]
pub struct ObservationRow {
    pub package_key: String,
    pub source: String,
    pub version: String,
    pub version_ord: String,
    pub scheme: String,
    pub observed_at: i64,
    pub observed_by: String,
}

/// Row from package_map: per-key source resolution state.
#[derive(Debug, Clone)]
pub struct PackageMapRow {
    pub package_key: String,
    pub source: Option<String>,
    pub resolved_at: Option<i64>,
    pub resolve_method: Option<String>,
    pub etag: Option<String>,
    pub last_poll: Option<i64>,
}

/// Row from shadow_recipes: one server-global shadow recipe catalog entry.
#[derive(Debug, Clone)]
pub struct ShadowRecipeRow {
    pub name: String,
    pub recipe_toml: String,
    pub signature: String,
    pub key_id: String,
    pub catalog_hash: String,
    pub published_at: i64,
    pub provenance_client_id: Option<Uuid>,
}

/// Row from shadow_scan_results: one per-client shadow scan finding.
#[derive(Debug, Clone)]
pub struct ShadowScanResultRow {
    pub id: Uuid,
    pub client_id: Option<Uuid>,
    pub recipe_name: String,
    pub installed: bool,
    pub detected_version: Option<String>,
    pub method: Option<String>,
    pub scanned_at: i64,
}

/// Row from pending_shadow_assignments (Sprint 22 slice 22-E2a):
/// a shadow recipe queued for push to a client via control-stream Command.
#[derive(Debug, Clone)]
pub struct PendingAssignmentRow {
    pub id: Uuid,
    pub client_id: Option<Uuid>,
    pub recipe_name: String,
    /// 'pending' | 'applied' | 'refused'
    pub status: String,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
    pub resolved_at: Option<i64>,
}

/// Row from shadow_staging: a shadow recipe staged to a client that awaits
/// client-side approval (review of recipe, install location, accessory
/// directories, firewall rules, health detection) before installation.
#[derive(Debug, Clone)]
pub struct ShadowStagingRow {
    pub id: Uuid,
    pub client_id: Option<Uuid>,
    pub recipe_name: String,
    /// 'pending' | 'approved' | 'rejected'
    pub status: String,
    pub suggested_target_dir: String,
    pub suggested_accessory_dirs: Vec<String>,
    pub suggested_firewall_rules: Vec<(u16, String, String, String)>,
    pub suggested_health_check: Option<String>, // serialized HealthCheck JSON
    pub client_target_dir: Option<String>,
    pub client_accessory_dirs: Vec<String>,
    pub client_firewall_rules: Vec<(u16, String, String, String)>,
    pub client_health_check: Option<String>,
    pub created_at: i64,
    pub reviewed_at: Option<i64>,
    pub resolved_at: Option<i64>,
}

/// Serializable firewall rule for staging rows (JSONB round-trip).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FirewallRuleRow {
    pub port: u16,
    pub protocol: String,
    pub direction: String,
    pub description: String,
}

/// Row from pair_approval_audit: one approve/deny action on a v2 pairing
/// request (P4 approval audit view).
#[derive(Debug, Clone)]
pub struct PairApprovalAuditRow {
    pub request_id: String,
    pub action: String,   // "approved" | "denied"
    pub approver: String, // username or "system"
    pub detail: Option<String>,
    pub created_at: i64, // epoch seconds
}

/// Row from enrollment_keys (P5): a fleet-scale pre-approval key.
#[derive(Debug, Clone)]
pub struct EnrollmentKeyRow {
    pub id: Uuid,
    pub key_hash: String, // hex SHA-256 of the plaintext key
    pub label: String,
    pub max_uses: i64,
    pub use_count: i64,
    pub created_at: i64,         // epoch seconds
    pub expires_at: Option<i64>, // epoch seconds
    pub revoked_at: Option<i64>, // epoch seconds
}

/// One fleet client install of a shadow package, for QueryShadowRegistry.
#[derive(Debug, Clone)]
pub struct FleetInstallRow {
    pub hostname: String,
    pub version: String,
    pub install_paths: Vec<String>,
    pub last_seen: i64, // epoch seconds of the client's last heartbeat
}

/// One QueryShadowRegistry match: recipe name/version, signing state, and the
/// fleet of clients that have installed that recipe version.
#[derive(Debug, Clone)]
pub struct ShadowRegistryMatchRow {
    pub recipe_name: String,
    pub recipe_version: String,
    pub signed: bool,
    pub key_id: String,
    pub fleet_installs: Vec<FleetInstallRow>,
}
