//! Monopsony Server - Caching proxy and coordination server

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

mod proxy;
mod grpc;
mod db;
mod discovery;
mod tls;
mod metrics;
mod purge;
mod prefetch;
mod security;
mod mirror;
mod announce;
mod nat;
mod auth;
mod manifest;
mod public_rate_limit;
mod shadow_signing;
mod shadow_recipe;
mod shadow_conflict;
mod shadow_cache;
mod shadow_source;
mod shadow_git_mirror;
mod shadow;

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
    #[arg(long, default_value = "postgres://monopsony:***@localhost/monopsony")]
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

    /// Tracker URL(s) for mirror announce (comma-separated)
    #[arg(long, env = "MONOPSONY_TRACKER_URL")]
    tracker_url: Option<String>,

    /// Tracker announce interval in seconds (default: 1800 = 30min)
    #[arg(long, default_value = "1800")]
    tracker_interval: u64,

    /// Authentication mode for HTTP proxy: open (default), token, mtls
    #[arg(long, default_value = "open")]
    auth_mode: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Pair client with server (establishes mTLS trust)
    Pair {
        /// Server gRPC address to pair with
        #[arg(short, long)]
        server: String,
        
        /// Paired client certificate directory (optional)
        #[arg(long)]
        cert_dir: Option<String>,
        
        /// Override paired client ID (optional)
        #[arg(long)]
        client_id: Option<String>,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Handle CLI subcommands
    if let Some(Commands::Pair { server, cert_dir, client_id }) = &args.command {
        return handle_pair_command(server, cert_dir.as_deref(), client_id.as_deref(), &args.grpc_addr).await;
    }
    if let Some(Commands::ApiKey { action }) = &args.command {
        return handle_api_key_command(action, &args.database_url).await;
    }
    if let Some(Commands::CacheStats { cache_dir }) = &args.command {
        return handle_cache_stats_command(cache_dir).await;
    }

    info!("Starting Monopsony Server v{}", env!("CARGO_PKG_VERSION"));
    info!("HTTP proxy: {}", args.http_addr);
    info!("gRPC: {}", args.grpc_addr);
    info!("Metrics: {}", args.metrics_addr);
    info!("Cache directory: {}", args.cache_dir);

    let http_addr: SocketAddr = args.http_addr.parse()
        .expect("Invalid HTTP proxy address");
    let grpc_addr: SocketAddr = args.grpc_addr.parse()
        .expect("Invalid gRPC address");
    let metrics_addr: SocketAddr = args.metrics_addr.parse()
        .expect("Invalid metrics address");

    let cache_dir = PathBuf::from(&args.cache_dir);

    if let Err(e) = tokio::fs::create_dir_all(&cache_dir).await {
        warn!("Could not create cache directory: {}", e);
    }

    let tls_dir = args.tls_cert_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| cache_dir.join("tls"));

    let tls_manager = tls::TlsManager::load_or_create(
        &tls_dir.join("ca.crt"),
        &tls_dir.join("ca.key"),
    )?;
    info!("CA fingerprint: {}", tls_manager.ca_fingerprint());

    let db = if args.no_db {
        info!("Database disabled, using file-only caching");
        None
    } else {
        match db::Database::connect(&args.database_url).await {
            Ok(database) => {
                database.run_migrations().await?;
                info!("Database connected and migrations applied");
                Some(Arc::new(database))
            }
            Err(e) => {
                warn!("Failed to connect to database: {}. Continuing with file-only caching.", e);
                None
            }
        }
    };

    let cache_dir_for_proxy = cache_dir.clone();
    let db_clone = db.clone();
    let auth_mode: monopsony_common::config::AuthMode = args.auth_mode.parse()
        .unwrap_or_else(|_| {
            warn!("Invalid auth_mode '{}', defaulting to 'open'", args.auth_mode);
            monopsony_common::config::AuthMode::Open
        });
    info!("Auth mode: {}", auth_mode);
    let proxy_handle = tokio::spawn(async move {
        if let Err(e) = proxy::run_proxy(http_addr, cache_dir_for_proxy, db_clone, auth_mode).await {
            error!("HTTP proxy error: {}", e);
        }
    });

    // Initialize mirror manager (before gRPC so we can share it)
    let mirror_dir = PathBuf::from(&args.mirror_dir);
    let shared_mirror_manager = if mirror_dir.exists() || tokio::fs::create_dir_all(&mirror_dir).await.is_ok() {
        let mgr = mirror::MirrorManager::new(mirror_dir.clone(), db.clone());
        if let Err(e) = mgr.load_config().await {
            warn!("Could not load mirror config: {}", e);
        }
        info!("Mirror directory: {}", mirror_dir.display());
        Some(Arc::new(mgr))
    } else {
        warn!("Mirror directory not available, mirror support disabled");
        None
    };

    if let Some(db) = db.clone() {
        let tls_manager = Arc::new(tls_manager);
        let db_for_grpc = db.clone();
        let mirror_for_grpc = shared_mirror_manager.clone();
        let grpc_handle = tokio::spawn(async move {
            if let Err(e) = grpc::start(grpc_addr, db_for_grpc, tls_manager, mirror_for_grpc).await {
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
            cache_dir: cache_dir.clone(),
            purge_after_days: 30,
            max_size_gb: 0,
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
            cache_dir: cache_dir.clone(),
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
        let mirror_state = mirror::MirrorState::new(
            (**mm).clone(),
            mirror_dir.clone(),
        );

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
        let mirror_listen_addr: SocketAddr = args.mirror_addr.parse()
            .expect("Invalid mirror address");
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
                info!("Tracker announce task starting ({} tracker(s), interval={}s)", 
                    url_count, args.tracker_interval);
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
        if let Err(e) = metrics::run_metrics_server(metrics_addr, db_for_metrics, cache_dir_for_metrics).await {
            error!("Metrics server error: {}", e);
        }
    });

    info!("Server started - HTTP proxy, gRPC, and metrics active");

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
        if r.is_err() { warn!("Mirror drain timed out"); }
    }
    if let Some(h) = announce_handle {
        // Announce is fire-and-forget; abort on shutdown
        h.abort();
    }
    let remaining = drain_timeout.saturating_sub(drain_start.elapsed());
    if let Some(h) = prefetch_handle {
        let r = tokio::time::timeout(remaining, h).await;
        if r.is_err() { warn!("Prefetch drain timed out"); }
    }
    let remaining = drain_timeout.saturating_sub(drain_start.elapsed());
    if let Some(h) = purge_handle {
        let r = tokio::time::timeout(remaining, h).await;
        if r.is_err() { warn!("Purge drain timed out"); }
    }
    let remaining = drain_timeout.saturating_sub(drain_start.elapsed());
    let _ = tokio::time::timeout(remaining, proxy_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), metrics_handle).await;

    if let Some(disc) = discovery_service {
        if let Err(e) = disc.stop() {
            warn!("Error stopping discovery: {}", e);
        }
    }

    info!("Shutdown complete");
    Ok(())
}

async fn handle_pair_command(
    server_grpc_addr: &str,
    cert_dir: Option<&str>,
    client_id_override: Option<&str>,
    _local_grpc_addr: &str,
) -> anyhow::Result<()> {
    use prost::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let server_addr: SocketAddr = server_grpc_addr.parse().map_err(|e| {
        anyhow::anyhow!("Invalid server address: {}. Example: 192.168.1.100:50051", e)
    })?;

    info!("Pairing with server: {}", server_addr);

    // Init TLS manager for this session
    let cache_dir = PathBuf::from("/var/cache/monopsony");
    let tls_dir = cert_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| cache_dir.join("tls"));
    let tls_manager = tls::TlsManager::load_or_create(
        &tls_dir.join("ca.crt"),
        &tls_dir.join("ca.key"),
    )?;

    // Init DB for recording pairing
    let db = {
        let database_url = std::env::var("MONOPSONY_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://monopsony:monopsony@localhost/monopsony".to_string());
        match db::Database::connect(&database_url).await {
            Ok(d) => { d.run_migrations().await?; Some(d) }
            Err(_) => { warn!("No database available, pairing will not be recorded"); None }
        }
    };

    // Generate 6-char alphanumeric pairing code
    let charset: Vec<u8> = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".to_vec(); // no ambiguous chars
    let mut random_bytes = [0u8; 6];
    getrandom::getrandom(&mut random_bytes)?;
    let pairing_code: String = random_bytes.iter()
        .map(|&b| charset[b as usize % charset.len()] as char)
        .collect();

    println!("Server pairing code: {}", pairing_code);
    println!("Share this code with the client. Valid for 5 minutes.");
    info!("Generated pairing code: {}", pairing_code);

    // Short-lived plaintext listener on port 50052
    let pair_addr: SocketAddr = "0.0.0.0:50052".parse()?;
    let pair_listener = tokio::net::TcpListener::bind(pair_addr).await
        .map_err(|e| anyhow::anyhow!("Failed to bind pairing port {}: {}", pair_addr, e))?;

    info!("Pairing listener on {} (5-minute timeout)", pair_addr);

    let timeout = tokio::time::Instant::now() + tokio::time::Duration::from_secs(300);

    tokio::select! {
        result = pair_listener.accept() => {
            match result {
                Ok((mut socket, addr)) => {
                    info!("Client connected from {}", addr);

                    // Read length-prefixed PairRequest
                    let mut len_buf = [0u8; 4];
                    socket.read_exact(&mut len_buf).await
                        .map_err(|e| anyhow::anyhow!("Failed to read request: {}", e))?;
                    let req_len = u32::from_le_bytes(len_buf) as usize;
                    let mut req_buf = vec![0u8; req_len];
                    socket.read_exact(&mut req_buf).await
                        .map_err(|e| anyhow::anyhow!("Failed to read request body: {}", e))?;

                    let req = monopsony_common::proto::monopsony::PairRequest::decode(req_buf.as_slice())
                        .map_err(|e| anyhow::anyhow!("Failed to decode pair request: {}", e))?;

                    if req.code != pairing_code {
                        return Err(anyhow::anyhow!("Invalid pairing code"));
                    }

                    info!("Pairing code validated from {} ({})", req.hostname, req.os);

                    // Generate client certificate
                    let new_client_id = client_id_override
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("client-{}", uuid::Uuid::new_v4().simple()));

                    let (client_cert, client_key) = tls_manager.generate_client_cert(&new_client_id)?;
                    let (client_cert_pem, client_key_pem) = tls::TlsManager::cert_and_key_to_pem(&client_cert, &client_key);
                    let ca_cert_pem = tls_manager.ca_cert_pem_bytes();

                    // Build PairResponse
                    let resp = monopsony_common::proto::monopsony::PairResponse {
                        client_id: new_client_id.clone(),
                        client_cert_pem,
                        client_key_pem,
                        ca_cert_pem,
                        grpc_addr: format!("{}:{}", server_addr.ip(), server_addr.port()),
                    };
                    let resp_bytes = resp.encode_to_vec();
                    let resp_len = resp_bytes.len() as u32;
                    socket.write_all(&resp_len.to_le_bytes()).await?;
                    socket.write_all(&resp_bytes).await?;

                    info!("Client certificates sent to {}", addr);

                    // Record pairing in DB
                    if let Some(db) = db.as_ref() {
                        let cid = monopsony_common::ClientId::parse(&new_client_id)
                            .map_err(|e| anyhow::anyhow!("Invalid client ID: {}", e))?;
                        db.record_pairing(&cid, chrono::Utc::now()).await?;
                    }

                    println!("Pairing complete with client: {}", new_client_id);
                    println!("Client can now connect over mTLS on {}", server_addr);
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to accept pairing connection: {}", e));
                }
            }
        },
        _ = tokio::time::sleep_until(timeout) => {
            return Err(anyhow::anyhow!("Pairing timed out (5 minutes)"));
        }
    }

    Ok(())
}

async fn handle_api_key_command(action: &ApiKeyCommands, database_url: &str) -> anyhow::Result<()> {
    let db = db::Database::connect(database_url).await?;
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
            println!("{:<40} {:<20} {:<20} {:<20}",
                "KEY ID", "LABEL", "CREATED", "LAST USED");
            println!("{}", "-".repeat(100));
            for k in keys {
                let last_used = k.last_used
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "never".to_string());
                println!("{:<40} {:<20} {:<20} {:<20}",
                    k.id.0,
                    k.label,
                    k.created_at.format("%Y-%m-%d %H:%M"),
                    last_used);
            }
        }
        ApiKeyCommands::Revoke { key_id } => {
            let key_id = monopsony_common::ApiKeyId(
                uuid::Uuid::parse_str(key_id)
                    .map_err(|e| anyhow::anyhow!("Invalid key ID: {}", e))?
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
    println!("Total size:      {} ({:.2} MB)", stats.total_size_bytes, stats.total_size_bytes as f64 / (1024.0 * 1024.0));
    println!("Meta files:      {}", stats.total_meta_files);
    println!();

    if !stats.by_backend.is_empty() {
        println!("Per-backend breakdown:");
        println!("  {:<20} {:>10} {:>15}", "Backend", "Entries", "Size");
        println!("  {}", "-".repeat(48));
        for bs in &stats.by_backend {
            println!("  {:<20} {:>10} {:>14.2} MB",
                bs.backend,
                bs.entry_count,
                bs.total_size_bytes as f64 / (1024.0 * 1024.0));
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
