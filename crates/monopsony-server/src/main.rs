//! Monopsony Server - Caching proxy and coordination server

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

// Crate imports
use monopsony_admin::{AdminState, DashboardConfig};
use monopsony_control::{announce, discovery, grpc, metrics};
use monopsony_db::Database;
use monopsony_mirror::mirror;
use monopsony_proxy::{cache_storage, object_store_backend, prefetch, proxy, purge};
use monopsony_shadow::ShadowPackageManager;

mod ca_guard;
mod self_update;
mod setup;

#[derive(Parser, Debug)]
#[command(name = "monopsony-server", version)]
#[command(about = "Monopsony caching proxy server", long_about = None)]
#[command(propagate_version = true)]
struct Args {
    /// Path to TOML configuration file
    #[arg(short, long, default_value = "/etc/monopsony/server.toml")]
    config: String,

    /// Address for the HTTP caching proxy
    #[arg(long, default_value = "0.0.0.0:3128")]
    http_addr: String,

    /// Address for the gRPC control plane (mTLS required)
    #[arg(long, default_value = "0.0.0.0:50051")]
    grpc_addr: String,

    /// Address for the Prometheus metrics endpoint
    #[arg(long, default_value = "0.0.0.0:9090")]
    metrics_addr: String,

    /// Address for the admin dashboard API
    #[arg(long, default_value = "0.0.0.0:3130")]
    dashboard_addr: String,

    /// Directory for cached package files
    #[arg(long, default_value = "/var/cache/monopsony")]
    cache_dir: String,

    /// Directory for distribution mirror data
    #[arg(long, default_value = "/var/lib/monopsony/mirrors")]
    mirror_dir: String,

    /// Address for the distribution mirror HTTP server
    #[arg(long, default_value = "0.0.0.0:3129")]
    mirror_addr: String,

    /// PostgreSQL connection URL
    #[arg(
        long,
        default_value = "postgres://monopsony:CHANGE_ME@localhost/monopsony"
    )]
    database_url: String,

    /// Run without a database (limited features: no client management, no inventory)
    #[arg(long)]
    no_db: bool,

    /// Enable mDNS server discovery
    #[arg(long, default_value = "true")]
    discovery: bool,

    /// Directory for TLS CA and certificates
    #[arg(long)]
    tls_cert_dir: Option<String>,

    /// Directory for the CA backup (kept OUTSIDE the cache dir so a cache wipe
    /// cannot destroy both copies). Default: /var/lib/monopsony/ca-backup
    #[arg(long, default_value = "/var/lib/monopsony/ca-backup")]
    ca_backup_dir: String,

    /// Regenerate the CA even when paired clients exist (deliberate rotation;
    /// every client must re-pair afterwards)
    #[arg(long)]
    regenerate_ca: bool,

    /// Tracker URL(s) for mirror announce (comma-separated)
    #[arg(long, env = "MONOPSONY_TRACKER_URL")]
    tracker_url: Option<String>,

    /// Tracker announce interval in seconds (default: 1800 = 30min)
    #[arg(long, default_value = "1800")]
    tracker_interval: u64,

    /// Directory for shadow package cache and signing key
    #[arg(long, default_value = "/var/lib/monopsony/shadow")]
    shadow_dir: String,

    /// Directory for shadow git mirrors
    #[arg(long, default_value = "/var/lib/monopsony/shadow/git-mirrors")]
    shadow_git_mirror_dir: String,

    /// GitHub API token for shadow package update checking (or set MONOPSONY_GITHUB_TOKEN)
    #[arg(long, env = "MONOPSONY_GITHUB_TOKEN")]
    github_token: Option<String>,

    /// Shadow update poll interval in seconds (default: 3600 = 1h)
    #[arg(long, default_value = "3600")]
    shadow_poll_interval: u64,

    /// Authentication mode for HTTP proxy: open (default), token, mtls
    #[arg(long, default_value = "open")]
    auth_mode: String,

    /// Cache backend: filesystem (default) or s3
    #[arg(long, default_value = "filesystem")]
    cache_backend: String,

    /// S3 endpoint URL (e.g. http://127.0.0.1:8333)
    #[arg(long)]
    s3_endpoint: Option<String>,

    /// S3 region
    #[arg(long)]
    s3_region: Option<String>,

    /// S3 bucket name
    #[arg(long)]
    s3_bucket: Option<String>,

    /// S3 access key (or set MONOPSONY_S3_ACCESS_KEY)
    #[arg(long)]
    s3_access_key: Option<String>,

    /// S3 secret key (or set MONOPSONY_S3_SECRET_KEY)
    #[arg(long)]
    s3_secret_key: Option<String>,

    /// S3 SSE-C encryption key file path (32 bytes)
    #[arg(long)]
    s3_encryption_key_path: Option<PathBuf>,

    /// S3 Object Lock retention days (default: 7)
    #[arg(long)]
    s3_retention_days: Option<u32>,

    /// S3 path-style access (true for SeaweedFS/Garage)
    #[arg(long)]
    s3_force_path_style: Option<bool>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Pair client with server (starts pairing via admin API on the running daemon)
    Pair {
        /// Dashboard/admin API address of the running server
        #[arg(short, long)]
        server: String,

        /// Admin API port (default: 3130)
        #[arg(long, default_value = "3130")]
        admin_port: u16,

        /// Optional: admin username for authenticated dashboard
        #[arg(long)]
        admin_user: Option<String>,

        /// Optional: admin password for authenticated dashboard
        #[arg(long)]
        admin_pass: Option<String>,
    },
    /// API key management
    ApiKey {
        #[command(subcommand)]
        action: ApiKeyCommands,
    },
    /// Show cache statistics
    CacheStats {
        /// Cache directory to scan (default: /var/cache/monopsony)
        #[arg(long, default_value = "/var/cache/monopsony")]
        cache_dir: String,
    },
    /// Interactive setup wizard for storage, scanning, and node tier
    Setup {
        /// Validate existing config instead of running wizard
        #[arg(long)]
        check: bool,
    },
    /// Self-update monopsony server from GitHub releases via the shadow system
    SelfUpdate {
        #[command(subcommand)]
        cmd: SelfUpdateMode,
    },
}

#[derive(Subcommand, Debug)]
enum ApiKeyCommands {
    /// Create a new API key
    Create {
        /// Label for the key (e.g., "ci-pipeline")
        #[arg(short, long)]
        label: String,
    },
    /// List all API keys
    List,
    /// Revoke an API key
    Revoke {
        /// Key ID to revoke
        key_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum SelfUpdateMode {
    /// Check for available updates and show version info
    Check,
    /// Download and apply the latest update (atomic swap + health check)
    Apply,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the default crypto provider for rustls (required by rustls 0.23)
    // This prevents panics when gRPC TLS connections are established
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let args = Args::parse();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Handle CLI subcommands
    if let Some(Commands::Pair {
        server,
        admin_port,
        admin_user,
        admin_pass,
    }) = &args.command
    {
        return handle_pair_via_admin_api(
            server,
            *admin_port,
            admin_user.as_deref(),
            admin_pass.as_deref(),
        )
        .await;
    }
    if let Some(Commands::ApiKey { action }) = &args.command {
        return handle_api_key_command(action, &args.database_url).await;
    }
    if let Some(Commands::CacheStats { cache_dir }) = &args.command {
        return handle_cache_stats_command(cache_dir).await;
    }
    if let Some(Commands::Setup { check }) = &args.command {
        if *check {
            let issues = setup::run_check(&args.config)?;
            if issues.is_empty() {
                println!("Config OK: {}", args.config);
            } else {
                println!("Issues found in {}:", args.config);
                for issue in &issues {
                    println!("  - {}", issue);
                }
                std::process::exit(1);
            }
        } else {
            setup::run_setup(&args.config)?;
        }
        return Ok(());
    }
    if let Some(Commands::SelfUpdate { cmd }) = args.command {
        return self_update::run_self_update(cmd).await;
    }

    info!("Starting Monopsony Server v{}", env!("CARGO_PKG_VERSION"));
    info!("HTTP proxy: {}", args.http_addr);
    info!("gRPC: {}", args.grpc_addr);
    info!("Metrics: {}", args.metrics_addr);
    info!("Dashboard: {}", args.dashboard_addr);
    info!("Cache directory: {}", args.cache_dir);

    let http_addr: SocketAddr = args.http_addr.parse().expect("Invalid HTTP proxy address");
    let grpc_addr: SocketAddr = args.grpc_addr.parse().expect("Invalid gRPC address");
    let metrics_addr: SocketAddr = args.metrics_addr.parse().expect("Invalid metrics address");

    let cache_dir = PathBuf::from(&args.cache_dir);

    if let Err(e) = tokio::fs::create_dir_all(&cache_dir).await {
        warn!("Could not create cache directory: {}", e);
    }

    let tls_dir = args
        .tls_cert_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| cache_dir.join("tls"));

    // Connect to the DB BEFORE initializing the CA: the CA guard must know how
    // many paired clients exist before it may generate a fresh CA.
    let db: Option<Arc<Database>>;
    let mut db_unreachable = false;
    if args.no_db {
        info!("Database disabled, using file-only caching");
        db = None;
    } else {
        match Database::connect(&args.database_url).await {
            Ok(database) => {
                database.run_migrations().await?;
                info!("Database connected and migrations applied");
                db = Some(Arc::new(database));
            }
            Err(e) => {
                warn!(
                    "Failed to connect to database: {}. Continuing with file-only caching.",
                    e
                );
                db = None;
                db_unreachable = true;
            }
        }
    }

    // CA lifecycle guard: auto-restore from backup (plan B) or fail closed when
    // paired clients exist. Only generates a fresh CA when that is provably safe.
    let backup_dir = PathBuf::from(&args.ca_backup_dir);
    let tls_manager = ca_guard::init_ca(
        &tls_dir,
        &backup_dir,
        db.as_deref(),
        db_unreachable,
        args.regenerate_ca,
    )
    .await?;
    info!("CA fingerprint: {}", tls_manager.ca_fingerprint());

    let tls_manager = Arc::new(tls_manager);

    let storage: Arc<dyn cache_storage::CacheStorage> = match args.cache_backend.as_str() {
        "s3" => {
            let s3_config = monopsony_common::config::S3Config {
                endpoint: args.s3_endpoint.clone().unwrap_or_default(),
                region: args
                    .s3_region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_string()),
                bucket: args.s3_bucket.clone().unwrap_or_default(),
                access_key: args.s3_access_key.clone().unwrap_or_default(),
                secret_key: args.s3_secret_key.clone().unwrap_or_default(),
                encryption_key_path: args.s3_encryption_key_path.clone(),
                retention_days: args.s3_retention_days.unwrap_or(7),
                force_path_style: args.s3_force_path_style.unwrap_or(true),
            };
            info!("Cache backend: S3 (bucket: {})", s3_config.bucket);
            let backend = object_store_backend::ObjectStoreBackend::s3(&s3_config)?;
            Arc::new(backend)
        }
        _ => {
            info!("Cache backend: local ({})", cache_dir.display());
            let backend = object_store_backend::ObjectStoreBackend::local(
                cache_dir.clone(),
                0, // retention_days — no retention for local
            )?;
            Arc::new(backend)
        }
    };
    let db_clone = db.clone();
    let auth_mode: monopsony_common::config::AuthMode =
        args.auth_mode.parse().unwrap_or_else(|_| {
            warn!(
                "Invalid auth_mode '{}', defaulting to 'open'",
                args.auth_mode
            );
            monopsony_common::config::AuthMode::Open
        });
    info!("Auth mode: {}", auth_mode);
    let storage_for_proxy = storage.clone();
    // Scan config will be loaded from TOML config file in future sprint
    let scan_config_for_proxy = None;

    // Read telemetry config from TOML
    let telemetry_enabled = {
        match std::fs::read_to_string(&args.config) {
            Ok(content) => {
                #[derive(serde::Deserialize)]
                struct TelemetryWrapper {
                    #[serde(default)]
                    telemetry: monopsony_common::config::TelemetryConfig,
                }
                toml::from_str::<TelemetryWrapper>(&content)
                    .map(|w| w.telemetry.install_tracking)
                    .unwrap_or(true)
            }
            Err(_) => true,
        }
    };
    info!(
        "Install telemetry: {}",
        if telemetry_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );

    // Read streaming proxy limits from TOML (falls back to defaults).
    let proxy_limits = {
        match std::fs::read_to_string(&args.config) {
            Ok(content) => {
                #[derive(serde::Deserialize, Default)]
                struct CacheWrapper {
                    #[serde(default)]
                    cache: CacheLimitsOnly,
                }
                #[derive(serde::Deserialize, Default)]
                struct CacheLimitsOnly {
                    #[serde(default)]
                    proxy_limits: monopsony_common::config::ProxyLimitsConfig,
                }
                toml::from_str::<CacheWrapper>(&content)
                    .ok()
                    .map(|w| w.cache.proxy_limits)
            }
            Err(_) => None,
        }
    };

    // Read CONNECT pass-through config ([proxy] section). Falls back to
    // defaults (connect_enabled=false) when absent — same behavior as
    // configs that predate the section.
    let connect_config = match std::fs::read_to_string(&args.config) {
        Ok(content) => {
            #[derive(serde::Deserialize, Default)]
            struct ProxySectionWrapper {
                #[serde(default)]
                proxy: monopsony_common::config::ProxyServerConfig,
            }
            toml::from_str::<ProxySectionWrapper>(&content)
                .map(|w| w.proxy)
                .unwrap_or_default()
        }
        Err(_) => monopsony_common::config::ProxyServerConfig::default(),
    };
    info!(
        "CONNECT pass-through: {}",
        if connect_config.connect_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    info!(
        "Proxy limits: max_body={} MiB, upstream_stall={}s",
        proxy_limits
            .as_ref()
            .map(|l| l.max_response_bytes / (1024 * 1024))
            .unwrap_or(4096),
        proxy_limits
            .as_ref()
            .map(|l| l.upstream_stall_secs)
            .unwrap_or(120)
    );

    // Read integrity pinning config ([integrity] section) the same way
    // [proxy] is read: falls back to defaults (pinning disabled) when absent.
    let integrity_cfg: monopsony_common::config::IntegrityConfig =
        match std::fs::read_to_string(&args.config) {
            Ok(content) => {
                #[derive(serde::Deserialize, Default)]
                struct IntegritySectionWrapper {
                    #[serde(default)]
                    integrity: monopsony_common::config::IntegrityConfig,
                }
                toml::from_str::<IntegritySectionWrapper>(&content)
                    .map(|w| w.integrity)
                    .unwrap_or_default()
            }
            Err(_) => monopsony_common::config::IntegrityConfig::default(),
        };

    // Pin key custody: the dedicated pin-signing key
    // (<shadow_dir>/pin-signing.key) — NEVER the mTLS CA and NEVER the
    // shadow recipe signing key (`signing.key`). Owned by main; cloned into
    // the proxy below. When pinning is disabled we still LOAD the key if it
    // exists (verification of previously written pins must keep working) but
    // never generate a new one; a zeroed verify key = "not provisioned".
    let pin_key_path = PathBuf::from(&args.shadow_dir).join("pin-signing.key");
    let pin_key = if integrity_cfg.pinning_enabled {
        let key = monopsony_shadow::shadow_signing::load_or_generate_signing_key(Some(&pin_key_path));
        info!(
            "Integrity pinning: enabled (policy={}), key: {}",
            integrity_cfg.unverified_policy,
            pin_key_path.display()
        );
        Some(key)
    } else if pin_key_path.exists() {
        let key = monopsony_shadow::shadow_signing::load_or_generate_signing_key(Some(&pin_key_path));
        info!(
            "Integrity pinning: disabled (policy={}), existing key loaded for pin verification: {}",
            integrity_cfg.unverified_policy,
            pin_key_path.display()
        );
        Some(key)
    } else {
        info!(
            "Integrity pinning: disabled (policy={}), no pin key generated",
            integrity_cfg.unverified_policy
        );
        None
    };
    // Zeroed verify key means "pinning not provisioned" (inert everywhere).
    let mirror_verify_key: [u8; 32] = pin_key
        .as_ref()
        .map(|k| *k.verifying_key().as_bytes())
        .unwrap_or([0u8; 32]);

    let pin_key_for_proxy = pin_key.clone();
    let proxy_handle = tokio::spawn(async move {
        if let Err(e) = proxy::run_proxy(
            http_addr,
            storage_for_proxy,
            db_clone,
            auth_mode,
            scan_config_for_proxy,
            None,
            proxy_limits,
            connect_config,
            pin_key_for_proxy,
            integrity_cfg.clone(),
        )
        .await
        {
            error!("HTTP proxy error: {}", e);
        }
    });

    // Initialize mirror manager (before gRPC so we can share it)
    let mirror_dir = PathBuf::from(&args.mirror_dir);
    let shared_mirror_manager =
        if mirror_dir.exists() || tokio::fs::create_dir_all(&mirror_dir).await.is_ok() {
            let mgr = mirror::MirrorManager::new(mirror_dir.clone(), db.clone());
            if let Err(e) = mgr.load_config().await {
                warn!("Could not load mirror config: {}", e);
            }
            // Also load any configs persisted in the DB (auto-created from client
            // pairing, or manually added via dashboard on a previous run).
            if let Err(e) = mgr.load_from_db().await {
                warn!("Could not load mirror configs from DB: {}", e);
            }
            info!("Mirror directory: {}", mirror_dir.display());
            Some(Arc::new(mgr))
        } else {
            warn!("Mirror directory not available, mirror support disabled");
            None
        };

    // Initialize shadow package manager
    let shadow_dir = PathBuf::from(&args.shadow_dir);
    let shadow_git_mirror_dir = PathBuf::from(&args.shadow_git_mirror_dir);
    let shared_shadow_manager = if db.is_some() {
        if let Err(e) = tokio::fs::create_dir_all(&shadow_dir).await {
            warn!("Could not create shadow directory: {}", e);
        }
        let signing_key_path = shadow_dir.join("signing.key");
        let signing_key =
            monopsony_shadow::shadow_signing::load_or_generate_signing_key(Some(&signing_key_path));
        let cache_dir = shadow_dir.join("cache");
        let poll_interval = std::time::Duration::from_secs(args.shadow_poll_interval);

        let mgr = ShadowPackageManager::new(
            db.clone().unwrap(),
            cache_dir,
            signing_key,
            args.github_token.clone(),
            poll_interval,
            shadow_git_mirror_dir,
        );
        info!(
            "Shadow package manager initialized (key: {})",
            shadow_dir.join("signing.key").display()
        );

        // Start background polling
        let mgr_arc = Arc::new(mgr);
        mgr_arc.start_polling();
        Some(mgr_arc)
    } else {
        info!("Shadow package manager not started (requires database)");
        None
    };

    if let Some(db) = db.clone() {
        let db_for_grpc = db.clone();
        let mirror_for_grpc = shared_mirror_manager.clone();
        let shadow_for_grpc = shared_shadow_manager.clone();
        let tls_for_grpc = tls_manager.clone();

        // Read TLS SANs and access control from config file
        let (tls_sans, access_control) = match std::fs::read_to_string(&args.config) {
            Ok(content) => {
                #[derive(serde::Deserialize)]
                struct CfgWrapper {
                    #[serde(default)]
                    tls: TlsSanWrapper,
                    #[serde(default)]
                    access_control: monopsony_common::config::AccessControlConfig,
                }
                #[derive(serde::Deserialize, Default)]
                struct TlsSanWrapper {
                    #[serde(default)]
                    san_names: Vec<String>,
                }
                match toml::from_str::<CfgWrapper>(&content) {
                    Ok(w) => (w.tls.san_names, Some(w.access_control)),
                    Err(_) => (Vec::new(), None),
                }
            }
            Err(_) => (Vec::new(), None),
        };
        if !tls_sans.is_empty() {
            info!(sans = ?tls_sans, "Additional TLS SANs from config");
        }
        if let Some(ref ac) = access_control {
            if ac.enabled {
                info!(cidrs = ?ac.allowed_cidrs, "Access control enabled");
            }
        }

        let grpc_handle = tokio::spawn(async move {
            if let Err(e) = grpc::start(
                grpc_addr,
                db_for_grpc,
                tls_for_grpc,
                mirror_for_grpc,
                shadow_for_grpc,
                telemetry_enabled,
                mirror_verify_key,
                &tls_sans,
                access_control,
            )
            .await
            {
                error!("gRPC server error: {}", e);
            }
        });
        tokio::spawn(async move {
            let _ = grpc_handle.await;
        });
    } else {
        info!("gRPC server not started (requires database)");
    };

    // Start cache purge task if database is available
    let purge_handle = if let Some(db) = db.clone() {
        let purge_config = purge::PurgeConfig {
            storage: storage.clone(),
            purge_after_days: 30,
            max_size_gb: 4,
        };
        let db_for_purge = db.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = purge::start_purge_task(db_for_purge, purge_config).await {
                error!("Cache purge task error: {}", e);
            }
        }))
    } else {
        None
    };

    // Start pre-fetch scheduler if database is available
    let prefetch_handle = if let Some(db) = db.clone() {
        let prefetch_config = prefetch::PrefetchConfig {
            storage: storage.clone(),
            interval_secs: 3600,
            max_concurrent: 10,
            request_timeout_secs: 300,
            bandwidth_limit_bps: 0,
        };
        let db_for_prefetch = db.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = prefetch::start_prefetch_task(db_for_prefetch, prefetch_config).await {
                error!("Pre-fetch task error: {}", e);
            }
        }))
    } else {
        None
    };

    // Start mirror sync + HTTP router (using shared mirror manager)
    let mirror_handle = if let Some(ref mm) = shared_mirror_manager {
        let mirror_state = mirror::MirrorState::new((**mm).clone(), mirror_dir.clone());

        // Spawn sync task
        let sync_mgr = (**mm).clone();
        let sync_handle = tokio::spawn(async move {
            if let Err(e) = sync_mgr.start_sync_task().await {
                error!("Mirror sync task error: {}", e);
            }
        });

        // Spawn mirror HTTP router on dedicated port
        // Read mirror config values via async accessors (avoid blocking_read inside runtime)
        let max_rps = mirror_state.manager.max_per_ip_rps().await;
        let any_private = mirror_state.manager.has_private_mirrors().await;
        let mirror_router = mirror::create_mirror_router(mirror_state, max_rps, any_private);
        let mirror_listen_addr: SocketAddr =
            args.mirror_addr.parse().expect("Invalid mirror address");
        let mirror_listener = tokio::net::TcpListener::bind(mirror_listen_addr).await?;
        info!("Mirror HTTP listening on {}", mirror_listen_addr);

        Some(tokio::spawn(async move {
            tokio::select! {
                _ = sync_handle => {},
                result = axum::serve(mirror_listener, mirror_router.into_make_service()) => {
                    if let Err(e) = result {
                        error!("Mirror HTTP server error: {}", e);
                    }
                }
            }
        }))
    } else {
        None
    };

    // Tracker announce task
    let announce_handle = if let Some(ref mm) = shared_mirror_manager {
        if let Some(ref tracker_url_str) = args.tracker_url {
            let urls: Vec<String> = tracker_url_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !urls.is_empty() {
                let url_count = urls.len();
                let announcer = announce::TrackerAnnouncer::new(
                    mm.clone(),
                    urls,
                    args.mirror_addr.clone(),
                    args.tracker_interval,
                );
                info!(
                    "Tracker announce task starting ({} tracker(s), interval={}s)",
                    url_count, args.tracker_interval
                );
                Some(tokio::spawn(async move {
                    announcer.run().await;
                }))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let discovery_service = if args.discovery {
        let http_port = http_addr.port();
        let grpc_port = grpc_addr.port();
        let svc = discovery::DiscoveryService::new("monopsony", http_port, grpc_port)?;
        let local_addr: std::net::IpAddr = http_addr.ip();
        if let Err(e) = svc.start(local_addr).await {
            warn!("mDNS discovery failed to start: {}", e);
            None
        } else {
            info!("mDNS discovery active on _monopsony._tcp.local.");
            Some(svc)
        }
    } else {
        info!("mDNS discovery disabled");
        None
    };

    // Start metrics server
    let cache_dir_for_metrics = cache_dir.clone();
    let db_for_metrics = db.clone();
    let metrics_handle = tokio::spawn(async move {
        if let Err(e) =
            metrics::run_metrics_server(metrics_addr, db_for_metrics, cache_dir_for_metrics).await
        {
            error!("Metrics server error: {}", e);
        }
    });

    // Start admin dashboard API server
    let dashboard_addr: SocketAddr = args
        .dashboard_addr
        .parse()
        .expect("Invalid dashboard address");
    // Parse dashboard config from the TOML config file
    let dashboard_config = {
        #[derive(serde::Deserialize)]
        struct ConfigWrapper {
            #[serde(default)]
            dashboard: DashboardConfig,
        }
        match std::fs::read_to_string(&args.config) {
            Ok(content) => match toml::from_str::<ConfigWrapper>(&content) {
                Ok(wrapped) => wrapped.dashboard,
                Err(_) => DashboardConfig::default(),
            },
            Err(_) => DashboardConfig::default(),
        }
    };
    info!(
        "Dashboard enabled: {}, credentials configured: {}",
        dashboard_config.enabled,
        dashboard_config.username.is_some() && dashboard_config.password_hash.is_some()
    );

    // Read WebSocket tunnel URL from [grpc] section (optional)
    let ws_url = match std::fs::read_to_string(&args.config) {
        Ok(content) => {
            #[derive(serde::Deserialize)]
            struct GrpcWrapper {
                #[serde(default)]
                grpc: GrpcWsWrapper,
            }
            #[derive(serde::Deserialize, Default)]
            struct GrpcWsWrapper {
                #[serde(default)]
                ws_url: Option<String>,
            }
            toml::from_str::<GrpcWrapper>(&content)
                .ok()
                .and_then(|w| w.grpc.ws_url)
        }
        Err(_) => None,
    };
    if let Some(ref url) = ws_url {
        info!("WebSocket tunnel URL from config: {}", url);
    }

    let admin_state = AdminState {
        db: db.clone(),
        cache_dir: cache_dir.clone(),
        shadow_manager: shared_shadow_manager.clone(),
        mirror_manager: shared_mirror_manager.clone(),
        tls_manager: tls_manager.clone(),
        config_path: PathBuf::from(&args.config),
        dashboard_config: Arc::new(dashboard_config),
        tailscale: Arc::new(monopsony_admin::auth::TailscaleDetector::new()),
        grpc_addr: args.grpc_addr.clone(),
        ws_url,
        mirror_verify_key,
        pairing_manager: Arc::new(tokio::sync::Mutex::new(None)),
        pairing_task: Arc::new(tokio::sync::Mutex::new(None)),
    };
    let admin_router = monopsony_admin::admin_router(admin_state);
    let dashboard_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(dashboard_addr)
            .await
            .expect("Failed to bind dashboard port");
        info!("Dashboard API listening on {}", dashboard_addr);
        if let Err(e) = axum::serve(listener, admin_router.into_make_service()).await {
            error!("Dashboard server error: {}", e);
        }
    });

    info!("Server started - HTTP proxy, gRPC, metrics, and dashboard active");

    // Graceful shutdown: broadcast signal to all tasks
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    // Shutdown receivers for tasks that need cooperative cancellation.
    // Currently unused — tasks are drained via timeout after ctrl-c.
    // Future: wire into axum::serve(...).with_graceful_shutdown(rx.changed())
    let _shutdown_rx_proxy = shutdown_tx.subscribe();
    let _shutdown_rx_metrics = shutdown_tx.subscribe();
    let _shutdown_rx_mirror = shutdown_tx.subscribe();

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutting down gracefully (30s drain timeout)...");

    // Send shutdown signal to all tasks
    let _ = shutdown_tx.send(());

    // Drain with timeout
    let drain_timeout = Duration::from_secs(30);
    let drain_start = std::time::Instant::now();

    // Drain order: mirror → announce → prefetch → purge → proxy → metrics
    if let Some(h) = mirror_handle {
        let r = tokio::time::timeout(drain_timeout, h).await;
        if r.is_err() {
            warn!("Mirror drain timed out");
        }
    }
    if let Some(h) = announce_handle {
        // Announce is fire-and-forget; abort on shutdown
        h.abort();
    }
    let remaining = drain_timeout.saturating_sub(drain_start.elapsed());
    if let Some(h) = prefetch_handle {
        let r = tokio::time::timeout(remaining, h).await;
        if r.is_err() {
            warn!("Prefetch drain timed out");
        }
    }
    let remaining = drain_timeout.saturating_sub(drain_start.elapsed());
    if let Some(h) = purge_handle {
        let r = tokio::time::timeout(remaining, h).await;
        if r.is_err() {
            warn!("Purge drain timed out");
        }
    }
    let remaining = drain_timeout.saturating_sub(drain_start.elapsed());
    let _ = tokio::time::timeout(remaining, proxy_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), metrics_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), dashboard_handle).await;

    if let Some(disc) = discovery_service {
        if let Err(e) = disc.stop() {
            warn!("Error stopping discovery: {}", e);
        }
    }

    info!("Shutdown complete");
    Ok(())
}

async fn handle_pair_via_admin_api(
    server: &str,
    admin_port: u16,
    admin_user: Option<&str>,
    admin_pass: Option<&str>,
) -> anyhow::Result<()> {
    info!(
        "Initiating pairing via admin API on {}:{}",
        server, admin_port
    );

    let client = reqwest::Client::new();
    let url = format!("http://{}:{}/api/v1/pair/init", server, admin_port);

    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body("{}");

    if let (Some(user), Some(pass)) = (admin_user, admin_pass) {
        req = req.basic_auth(user, Some(pass));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to reach admin API at {}: {}", url, e))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status == reqwest::StatusCode::CONFLICT {
        // A session is already active — extract the existing code
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("Failed to parse conflict response: {}", e))?;
        let code = parsed
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN");
        println!("A pairing session is already active!");
        println!("Code: {}", code);
        println!();
        println!("Client command:");
        println!(
            "  monopsony-client pair --server {}:50052 --code {}",
            server, code
        );
        return Ok(());
    }

    if !status.is_success() {
        anyhow::bail!("Admin API returned {}: {}", status, body);
    }

    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("Failed to parse pairing response: {}", e))?;

    let code = parsed
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing code in pairing response"))?;

    let cli_command = parsed
        .get("cli_command")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    println!("========================================");
    println!("     Monopsony Pairing Active");
    println!("========================================");
    println!();
    println!("Pairing code: {}", code);
    println!("Valid for 5 minutes.");
    println!();
    println!("On the client machine, run:");
    println!("  {}", cli_command.replace("<SERVER_IP>", server));
    println!();
    println!(
        "Or pair from the dashboard at http://{}:{}/dashboard",
        server, admin_port
    );

    Ok(())
}

async fn handle_api_key_command(action: &ApiKeyCommands, database_url: &str) -> anyhow::Result<()> {
    let db = Database::connect(database_url).await?;
    db.run_migrations().await?;

    match action {
        ApiKeyCommands::Create { label } => {
            // Generate a random API key
            let key = format!("mpk_{}", uuid::Uuid::new_v4().simple());
            // Hash it for storage (using SHA-256)
            use sha2::Digest;
            let key_hash = sha2::Sha256::digest(key.as_bytes());
            let key_hash_str = format!("{:x}", key_hash);

            let key_id = db.create_api_key(label, &key_hash_str).await?;
            println!("API key created:");
            println!("  Key ID: {}", key_id);
            println!("  Label: {}", label);
            println!("\n  API Key: {}", key);
            println!("\n  WARNING: Store this key securely! It cannot be retrieved again.");
        }
        ApiKeyCommands::List => {
            let keys = db.list_api_keys().await?;
            if keys.is_empty() {
                println!("No API keys found.");
                return Ok(());
            }
            println!(
                "{:<40} {:<20} {:<20} {:<20}",
                "KEY ID", "LABEL", "CREATED", "LAST USED"
            );
            println!("{}", "-".repeat(100));
            for k in keys {
                let last_used = k
                    .last_used
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "never".to_string());
                println!(
                    "{:<40} {:<20} {:<20} {:<20}",
                    k.id.0,
                    k.label,
                    k.created_at.format("%Y-%m-%d %H:%M"),
                    last_used
                );
            }
        }
        ApiKeyCommands::Revoke { key_id } => {
            let key_id = monopsony_common::ApiKeyId(
                uuid::Uuid::parse_str(key_id)
                    .map_err(|e| anyhow::anyhow!("Invalid key ID: {}", e))?,
            );
            let revoked = db.revoke_api_key(&key_id).await?;
            if revoked {
                println!("API key revoked: {}", key_id);
            } else {
                println!("API key not found: {}", key_id);
            }
        }
    }

    Ok(())
}

async fn handle_cache_stats_command(cache_dir: &str) -> anyhow::Result<()> {
    let path = std::path::PathBuf::from(cache_dir);
    let stats = proxy::scan_cache_stats(&path).await;

    println!("Monopsony Cache Statistics");
    println!("=========================");
    println!("Cache directory: {}", stats.cache_dir);
    println!("Total entries:   {}", stats.total_entries);
    println!(
        "Total size:      {} ({:.2} MB)",
        stats.total_size_bytes,
        stats.total_size_bytes as f64 / (1024.0 * 1024.0)
    );
    println!("Meta files:      {}", stats.total_meta_files);
    println!();

    if !stats.by_backend.is_empty() {
        println!("Per-backend breakdown:");
        println!("  {:<20} {:>10} {:>15}", "Backend", "Entries", "Size");
        println!("  {}", "-".repeat(48));
        for bs in &stats.by_backend {
            println!(
                "  {:<20} {:>10} {:>14.2} MB",
                bs.backend,
                bs.entry_count,
                bs.total_size_bytes as f64 / (1024.0 * 1024.0)
            );
        }
        println!();
    }

    if !stats.top_by_size.is_empty() {
        println!("Top 10 by size:");
        println!("  {:>12}  URL", "Size");
        println!("  {}", "-".repeat(70));
        for entry in &stats.top_by_size {
            let size_str = if entry.content_length > 1024 * 1024 {
                format!("{:.2} MB", entry.content_length as f64 / (1024.0 * 1024.0))
            } else if entry.content_length > 1024 {
                format!("{:.2} KB", entry.content_length as f64 / 1024.0)
            } else {
                format!("{} B", entry.content_length)
            };
            let url_short = if entry.url.len() > 55 {
                format!("...{}", &entry.url[entry.url.len() - 52..])
            } else {
                entry.url.clone()
            };
            println!("  {:>12}  {}", size_str, url_short);
        }
        println!();
    }

    if !stats.top_by_hits.is_empty() {
        println!("Top 10 by hit count:");
        println!("  {:>8}  URL", "Hits");
        println!("  {}", "-".repeat(70));
        for entry in &stats.top_by_hits {
            let url_short = if entry.url.len() > 55 {
                format!("...{}", &entry.url[entry.url.len() - 52..])
            } else {
                entry.url.clone()
            };
            println!("  {:>8}  {}", entry.hit_count, url_short);
        }
        println!();
    }

    if let Some(ref oldest) = stats.oldest_entry {
        let oldest_dt = chrono::DateTime::<chrono::Utc>::from_timestamp(oldest.cached_at as i64, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        println!("Oldest entry:  {} (cached {})", oldest_dt, oldest.url);
    }
    if let Some(ref newest) = stats.newest_entry {
        let newest_dt = chrono::DateTime::<chrono::Utc>::from_timestamp(newest.cached_at as i64, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        println!("Newest entry:  {} (cached {})", newest_dt, newest.url);
    }

    Ok(())
}
