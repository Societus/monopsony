//! Monopsony Client - Local proxy and package manager integration

mod shadow;
mod shadow_cli;

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;
use anyhow::Context;

mod proxy;
mod grpc_client;
mod discovery;
mod trust;
mod auto_update;

#[derive(Parser, Debug)]
#[command(name = "monopsony-client", version)]
#[command(about = "Monopsony client daemon and CLI", long_about = None)]
#[command(propagate_version = true)]
struct Args {
    /// Path to TOML configuration file
    #[arg(short, long, default_value = "/etc/monopsony/client.toml")]
    config: String,

    /// Directory for client certificates and pairing data
    #[arg(long, default_value = "~/.config/monopsony")]
    cert_dir: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the local proxy daemon and connect to a server
    Daemon {
        /// Address for the local HTTP proxy
        #[arg(long, default_value = "127.0.0.1:3129")]
        proxy_addr: String,

        /// Server address (host:port). If not set, discovers via mDNS.
        #[arg(long)]
        server: Option<String>,

        /// Timeout in ms before falling back to direct upstream
        #[arg(long, default_value = "5000")]
        timeout_ms: u64,
    },
    /// Discover Monopsony servers on the network
    Discover {
        /// Discovery timeout in seconds
        #[arg(long, default_value = "5")]
        timeout_secs: u64,

        /// Tracker URL(s) for mirror discovery (comma-separated)
        #[arg(long, env = "MONOPSONY_TRACKER_URL")]
        tracker_url: Option<String>,

        /// Peer exchange URL(s) (comma-separated)
        #[arg(long)]
        peer_url: Option<String>,

        /// Distro filter for tracker queries
        #[arg(long, default_value = "archlinux")]
        distro: String,
    },
    /// Show client status (certificates, detected package managers)
    Status,
    /// Configure local package managers to use the Monopsony proxy
    Configure {
        /// Server address used to derive proxy settings
        #[arg(long)]
        server: String,
    },
    /// Pair with a server using a 6-character code (establishes mTLS trust)
    Pair {
        /// Server address (host:port)
        #[arg(long)]
        server: String,

        /// 6-character pairing code from the server
        #[arg(long)]
        code: String,
    },
    /// Configure a local package manager to use a distribution mirror
    Mirror {
        /// Server address (host:port)
        #[arg(long)]
        server: String,

        /// Distro to configure (archlinux, debian, ubuntu, alpine, fedora, nixos)
        #[arg(long)]
        distro: String,

        /// HTTP address of the mirror on the server (default: http://<server>/mirror/<distro>/)
        #[arg(long)]
        mirror_url: Option<String>,
    },
    /// Show cache statistics (queries the server)
    CacheStats {
        /// Server gRPC address
        #[arg(long)]
        server: String,
    },
    /// Shadow package management (register, add, update, rollback, etc.)
    Package {
        #[command(subcommand)]
        cmd: shadow_cli::PackageCommand,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the default crypto provider for rustls (required by rustls 0.23)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let args = Args::parse();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Expand tilde in cert_dir
    let cert_dir = shellexpand::tilde(&args.cert_dir).to_string();
    let cert_dir = PathBuf::from(cert_dir);

    match args.command {
        Some(Commands::Daemon { proxy_addr, server, timeout_ms }) => {
            run_daemon(proxy_addr, server, timeout_ms, cert_dir, &args.config).await?;
        }
        Some(Commands::Discover { timeout_secs, tracker_url, peer_url, distro }) => {
            run_discover(timeout_secs, tracker_url, peer_url, &distro).await?;
        }
        Some(Commands::Status) => {
            run_status(&cert_dir).await?;
        }
        Some(Commands::Configure { server }) => {
            run_configure(&server).await?;
        }
        Some(Commands::Pair { server, code }) => {
            run_pair(&server, &code, &cert_dir).await?;
        }
        Some(Commands::Mirror { server, distro, mirror_url }) => {
            run_mirror(&server, &distro, mirror_url.as_deref()).await?;
        }
        Some(Commands::CacheStats { server }) => {
            run_cache_stats(&server).await?;
        }
        Some(Commands::Package { cmd }) => {
            run_shadow_package(cmd)?;
        }
        None => {
            info!("No command specified. Use --help for usage.");
        }
    }

    Ok(())
}

async fn run_daemon(
    proxy_addr: String,
    server: Option<String>,
    timeout_ms: u64,
    cert_dir: PathBuf,
    config_path: &str,
) -> anyhow::Result<()> {
    info!("Starting Monopsony Client v{}", env!("CARGO_PKG_VERSION"));

    // Load client config (optional — daemon works without one using CLI args)
    let client_config = match std::fs::read_to_string(config_path) {
        Ok(toml_str) => {
            match toml::from_str::<monopsony_common::config::ClientConfig>(&toml_str) {
                Ok(cfg) => {
                    info!("Loaded config from {}", config_path);
                    Some(cfg)
                }
                Err(e) => {
                    warn!("Failed to parse config at {}: {}", config_path, e);
                    None
                }
            }
        }
        Err(_) => {
            info!("No config file at {} (using CLI args)", config_path);
            None
        }
    };

    // Validate auto-update config if present and enabled
    let auto_update_config = client_config.as_ref().map(|c| c.auto_update.clone());
    if let Some(ref au_cfg) = auto_update_config {
        if au_cfg.enabled {
            auto_update::validate_schedule(&au_cfg.schedule)
                .context("auto-update schedule is invalid")?;
            auto_update::validate_timezone(&au_cfg.timezone)
                .context("auto-update timezone is invalid")?;
            info!(
                "Auto-update enabled: schedule='{}' tz='{}' max_severity='{}'",
                au_cfg.schedule, au_cfg.timezone, au_cfg.max_severity
            );
        }
    }

    // Require paired certificates
    let certs = grpc_client::ClientCerts::load(&cert_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "No client certificates found in {}. Run 'monopsony-client pair --server ADDR --code CODE' first.",
            cert_dir.display()
        )
    })?;
    info!("Using stored client certificates for mTLS");

    let server_addr = match server {
        Some(addr) => {
            info!("Using configured server: {}", addr);
            addr
        }
        None => {
            info!("Discovering server via mDNS...");
            let disc = discovery::ClientDiscovery::new("monopsony");
            let found = disc.discover_one(Duration::from_secs(10)).await
                .map_err(|e| anyhow::anyhow!(
                    "Server discovery failed (no --server provided). Is the server running and on the same network? Error: {}", e
                ))?;
            let addr = format!("{}:{}", found.address, found.grpc_port);
            info!("Discovered server at {}", addr);
            addr
        }
    };

    let listen_addr: SocketAddr = proxy_addr.parse()
        .expect("invalid proxy listen address");

    // Build TLS config for the initial connection
    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(&certs.ca_cert_pem))
        .identity(tonic::transport::Identity::from_pem(
            &certs.client_cert_pem,
            &certs.client_key_pem,
        ))
        .domain_name("monopsony-server")
        .assume_http2(true);

    // Connect using standard connect() with TLS
    let mut grpc_client = grpc_client::MonopsonyGrpcClient::connect(&server_addr, Some(tls)).await?;

    // Get client ID from stored file or register
    let client_id = {
        let id_file = cert_dir.join("client_id");
        match std::fs::read_to_string(&id_file) {
            Ok(id) => id.trim().to_string(),
            Err(_) => {
                // Register with server to get client ID
                let hostname = hostname::get()?
                    .to_string_lossy()
                    .to_string();
                let backends = monopsony_common::package_manager::get_available_backends();
                let managers: Vec<String> = backends.iter().map(|b| b.kind().as_str().to_string()).collect();

                let (id, _) = grpc_client.register(
                    &hostname,
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    &managers,
                ).await?;
                std::fs::write(&id_file, &id)?;
                info!("Registered with server, client ID: {}", id);
                id
            }
        }
    };

    // Spawn background task for heartbeat and inventory sync
    let client_id_for_task = client_id.clone();
    let cert_dir_for_task = cert_dir.clone();
    let server_addr_for_task = server_addr.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let mut last_inventory = std::time::Instant::now();
        let mut backoff: std::time::Duration = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);
        const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
        const INVENTORY_INTERVAL: Duration = Duration::from_secs(300);

        loop {
            // Attempt connection with exponential backoff
            let certs = match grpc_client::ClientCerts::load(&cert_dir_for_task) {
                Some(c) => c,
                None => {
                    warn!("No certificates available for heartbeat");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };

            let tls = tonic::transport::ClientTlsConfig::new()
                .ca_certificate(tonic::transport::Certificate::from_pem(&certs.ca_cert_pem))
                .identity(tonic::transport::Identity::from_pem(
                    &certs.client_cert_pem,
                    &certs.client_key_pem,
                ))
                .domain_name("monopsony-server");

            let mut client = match grpc_client::MonopsonyGrpcClient::connect(&server_addr_for_task, Some(tls)).await {
                Ok(c) => {
                    info!("Connected to server");
                    backoff = Duration::from_secs(1); // reset on success
                    c
                }
                Err(e) => {
                    warn!("Connection failed (retry in {:?}): {}", backoff, e);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };

            // Connected — run heartbeat loop
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);

            loop {
                interval.tick().await;

                // Send heartbeat
                match client.heartbeat(&client_id_for_task).await {
                    Ok(_) => {
                        info!("Heartbeat sent");
                    }
                    Err(e) => {
                        warn!("Heartbeat failed, reconnecting: {}", e);
                        break; // break inner loop → retry connection
                    }
                }

                // Send inventory periodically
                if last_inventory.elapsed() > INVENTORY_INTERVAL {
                    let backends = monopsony_common::package_manager::get_available_backends();
                    let mut all_packages = Vec::new();
                    for backend in &backends {
                        match backend.list_installed().await {
                            Ok(pkgs) => all_packages.extend(pkgs),
                            Err(e) => warn!("Failed to list packages from {:?}: {}", backend.kind(), e),
                        }
                    }

                    match client.report_inventory(&client_id_for_task, &all_packages, true).await {
                        Ok(_) => info!("Inventory reported ({} packages)", all_packages.len()),
                        Err(e) => {
                            warn!("Inventory report failed, reconnecting: {}", e);
                            break;
                        }
                    }
                    last_inventory = std::time::Instant::now();
                }
            }

            // Outer loop: backoff before retrying connection
            warn!("Disconnected (retry in {:?})", backoff);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    });

    // Spawn auto-update scheduler if configured and enabled
    let auto_update_handle = if let Some(ref au_cfg) = auto_update_config {
        if au_cfg.enabled {
            let au_cfg = au_cfg.clone();
            info!("Spawning auto-update scheduler");
            Some(tokio::spawn(async move {
                loop {
                    match auto_update::next_delay(&au_cfg) {
                        Some(delay) => {
                            info!("Next auto-update check in {:?}", delay);
                            tokio::time::sleep(delay).await;
                            info!("Auto-update window reached — checking for updates");
                            // TODO: integrate with shadow package check/apply cycle
                            // (will be wired when shadow daemon loop is implemented)
                        }
                        None => {
                            warn!("Auto-update schedule produced no future times — disabling scheduler");
                            break;
                        }
                    }
                }
            }))
        } else {
            None
        }
    } else {
        None
    };

    let proxy_config = proxy::ProxyConfig {
        server_addr: server_addr.clone(),
        fallback_timeout_ms: timeout_ms,
    };

    info!("Local proxy listening on {}", listen_addr);
    info!("Heartbeat and inventory sync running in background");
    proxy::run_proxy(listen_addr, proxy_config).await?;

    heartbeat_handle.abort();
    if let Some(h) = auto_update_handle {
        h.abort();
    }
    Ok(())
}

async fn run_discover(
    timeout_secs: u64,
    tracker_url: Option<String>,
    peer_url: Option<String>,
    distro: &str,
) -> anyhow::Result<()> {
    info!("Discovering Monopsony servers (timeout: {}s)...", timeout_secs);
    let disc = discovery::ClientDiscovery::new("monopsony");

    let tracker_urls: Vec<String> = tracker_url
        .map(|s| s.split(',').map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect())
        .unwrap_or_default();

    let peer_urls: Vec<String> = peer_url
        .map(|s| s.split(',').map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect())
        .unwrap_or_default();

    if tracker_urls.is_empty() && peer_urls.is_empty() {
        // Legacy mDNS-only mode
        let servers = disc.discover(Duration::from_secs(timeout_secs)).await?;
        if servers.is_empty() {
            info!("No servers found via mDNS.");
        } else {
            for srv in &servers {
                info!(
                    "  {} (HTTP port: {}, gRPC port: {:?})",
                    srv.address, srv.http_port, srv.grpc_port
                );
            }
        }
    } else {
        // Multi-source discovery
        let result = disc.discover_v2(
            Duration::from_secs(timeout_secs),
            &tracker_urls,
            &peer_urls,
            Some(distro),
        ).await?;

        if result.total_results() == 0 {
            info!("No servers found from any source.");
        } else {
            if !result.tracker_mirrors.is_empty() {
                info!("Tracker mirrors ({}):", result.tracker_mirrors.len());
                for m in &result.tracker_mirrors {
                    info!("  {} {} (score={:.1}, transport={})",
                        m.mirror_id, m.endpoint, m.score, m.transport);
                }
            }
            if !result.peer_mirrors.is_empty() {
                info!("Peer mirrors ({}):", result.peer_mirrors.len());
                for m in &result.peer_mirrors {
                    info!("  {} {} (score={:.1}, transport={})",
                        m.mirror_id, m.endpoint, m.score, m.transport);
                }
            }
            if !result.mdns_services.is_empty() {
                info!("mDNS services ({}):", result.mdns_services.len());
                for srv in &result.mdns_services {
                    info!("  {} (HTTP port: {}, gRPC port: {:?})",
                        srv.address, srv.http_port, srv.grpc_port);
                }
            }
        }
    }
    Ok(())
}

async fn run_status(cert_dir: &Path) -> anyhow::Result<()> {
    info!("Client status:");

    // Check for certificates
    if let Some(certs) = grpc_client::ClientCerts::load(cert_dir) {
        info!("  Certificates: Found at {}", cert_dir.display());
        info!("  CA cert: {} bytes", certs.ca_cert_pem.len());
        info!("  Client cert: {} bytes", certs.client_cert_pem.len());
    } else {
        info!("  Certificates: Not found");
        info!("  Run monopsony-client register to obtain certificates");
    }

    // Check package managers
    let backends = monopsony_common::package_manager::get_available_backends();
    info!("  Package managers detected: {}", backends.len());
    for backend in &backends {
        info!("    - {:?}", backend.kind());
    }

    Ok(())
}

async fn run_configure(server: &str) -> anyhow::Result<()> {
    info!("Configuring package managers for server: {}", server);
    let backends = monopsony_common::package_manager::get_available_backends();
    if backends.is_empty() {
        warn!("No supported package managers detected on this system.");
        return Ok(());
    }

    // Derive proxy address from server address
    let proxy_addr = "http://127.0.0.1:3129".to_string();
    let mut configured = 0;
    let mut failed = 0;

    for backend in &backends {
        let kind = backend.kind();
        let action = backend.proxy_config(&proxy_addr);
        info!("[{:?}] configuring...", kind);

        for result in action.execute() {
            match result {
                Ok(desc) => {
                    info!("[{:?}] {}", kind, desc);
                    configured += 1;
                }
                Err(e) => {
                    warn!("[{:?}] FAILED: {}", kind, e);
                    failed += 1;
                }
            }
        }
    }

    println!("Configuration complete: {} actions succeeded, {} failed", configured, failed);
    if failed > 0 {
        anyhow::bail!("{} configuration actions failed", failed);
    }
    Ok(())
}

async fn run_pair(
    server: &str,
    code: &str,
    cert_dir: &Path,
) -> anyhow::Result<()> {
    // Check if already paired
    if grpc_client::ClientCerts::load(cert_dir).is_some() {
        warn!("Client already has certificates. Delete {} to re-pair.", cert_dir.display());
        return Ok(());
    }

    let hostname = hostname::get()
        .map_err(|e| anyhow::anyhow!("Failed to get system hostname: {}", e))?
        .to_string_lossy()
        .to_string();

    let backends = monopsony_common::package_manager::get_available_backends();
    let managers: Vec<String> = backends.iter().map(|b| b.kind().as_str().to_string()).collect();

    info!("Pairing with server: {}", server);
    let (client_id, certs) = grpc_client::MonopsonyGrpcClient::pair(
        server,
        code,
        &hostname,
        std::env::consts::OS,
        std::env::consts::ARCH,
        &managers,
    ).await.map_err(|e| anyhow::anyhow!(
        "Pairing with server {} failed: {}. Is the server running and the pairing code correct?",
        server, e
    ))?;

    certs.save(cert_dir)?;
    std::fs::write(cert_dir.join("client_id"), &client_id)?;

    info!("Pairing complete!");
    info!("  Client ID: {}", client_id);
    info!("  Certificates saved to: {}", cert_dir.display());
    println!("Pairing complete! Client ID: {}", client_id);

    Ok(())
}

async fn run_mirror(
    server: &str,
    distro: &str,
    mirror_url: Option<&str>,
) -> anyhow::Result<()> {
    use monopsony_common::package_manager::{get_available_backends, PackageManagerKind};

    let mirror_base = mirror_url
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("http://{}/mirror/{}/", server, distro));

    let kind = match distro.to_lowercase().as_str() {
        "archlinux" | "arch" => PackageManagerKind::Pacman,
        "debian" => PackageManagerKind::Apt,
        "ubuntu" => PackageManagerKind::Apt,
        "alpine" => PackageManagerKind::Apt, // Alpine uses apk, but closest match
        "fedora" => PackageManagerKind::Dnf,
        "nixos" => {
            info!("NixOS mirror configuration requires manual channel override.");
            println!("NixOS: set NIX_CONFIG=\"substituters = {} https://cache.nixos.org\"", mirror_base);
            return Ok(());
        }
        _ => {
            anyhow::bail!("Unsupported distro: {}. Supported: archlinux, debian, ubuntu, alpine, fedora, nixos", distro);
        }
    };

    let backends = get_available_backends();
    let backend = backends.iter().find(|b| b.kind() == kind);

    match backend {
        Some(_b) => {
            info!("Configuring {} mirror: {}", distro, mirror_base);
            println!("Configuring {} to use mirror: {}", distro, mirror_base);

            match kind {
                PackageManagerKind::Pacman => {
                    // Write mirrorlist
                    let content = format!("# Monopsony mirror for Arch Linux\nServer = {}$repo/os/$arch\n", mirror_base);
                    let path = "/etc/pacman.d/mirrorlist";
                    println!("  Writing {}", path);
                    std::fs::write(path, content)?;
                    info!("Wrote {}", path);
                }
                PackageManagerKind::Apt => {
                    let codename = if distro.to_lowercase() == "ubuntu" {
                        // Try to detect Ubuntu codename
                        let output = std::process::Command::new("lsb_release")
                            .args(["-c", "-s"])
                            .output();
                        match output {
                            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                            _ => "noble".to_string(),
                        }
                    } else {
                        // Debian
                        let output = std::process::Command::new("lsb_release")
                            .args(["-c", "-s"])
                            .output();
                        match output {
                            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                            _ => "bookworm".to_string(),
                        }
                    };
                    let suite = &codename;
                    let content = format!(
                        "deb {} {} main restricted universe multiverse\n\
                         deb {} {}-updates main restricted universe multiverse\n\
                         deb {} {}-security main restricted universe multiverse\n",
                        mirror_base, suite, mirror_base, suite, mirror_base, suite,
                    );
                    let path = "/etc/apt/sources.list.d/monopsony-mirror.list";
                    println!("  Writing {}", path);
                    std::fs::write(path, content)?;
                    info!("Wrote {}", path);
                }
                PackageManagerKind::Dnf => {
                    let content = format!(
                        "[monopsony-mirror]\nname=Monopsony Mirror - Fedora\nbaseurl={}\nenabled=1\ngpgcheck=1\nmetadata_expire=6h\n",
                        mirror_base,
                    );
                    let path = "/etc/yum.repos.d/monopsony-mirror.repo";
                    println!("  Writing {}", path);
                    std::fs::write(path, content)?;
                    info!("Wrote {}", path);
                }
                _ => {
                    anyhow::bail!("Mirror configuration not yet implemented for {:?}", kind);
                }
            }

            println!("Mirror configuration complete for {}.", distro);
            println!("Note: You may need to run the package manager update command (e.g., apt update, pacman -Sy).");
        }
        None => {
            info!("No package manager backend found for {} (kind: {:?})", distro, kind);
            println!("No compatible package manager detected for {} on this system.", distro);
            println!("Manual configuration: point your package manager at {}", mirror_base);
        }
    }

    Ok(())
}

async fn run_cache_stats(server: &str) -> anyhow::Result<()> {
    info!("Connecting to server {} for cache stats...", server);
    
    // For now, print a helpful message pointing to the server-side command.
    // Full implementation will query the server via gRPC once the CacheStats
    // RPC is added to the proto definition.
    println!("Cache statistics requires server-side access.");
    println!("Run on the server:");
    println!("  monopsony-server cache-stats [--cache-dir /var/cache/monopsony]");
    println!();
    println!("Or connect to the server at {} and query the metrics endpoint:", server);
    println!("  curl http://{}/metrics | grep monopsony_cache", server);
    
    Ok(())
}

fn run_shadow_package(cmd: shadow_cli::PackageCommand) -> anyhow::Result<()> {
    use shadow_cli::PackageCommand::*;

    match cmd {
        Register { path, source, name, recipe, method, branch, requires } => {
            let pkg_name = name.unwrap_or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            });
            let source_type = shadow_cli::detect_source_type(&source);
            println!("Registering shadow package: {}", pkg_name);
            println!("  Source: {} ({:?})", source, source_type);
            println!("  Install: {}", method);
            println!("  Branch: {}", branch);
            if let Some(deps) = &requires {
                println!("  Build deps: {}", deps);
            }
            if let Some(recipe_path) = &recipe {
                println!("  Recipe file: {}", recipe_path);
            }
            println!();
            println!("Note: Full registration requires a running server connection.");
            println!("Use 'monopsony-client daemon' to connect, then register via gRPC.");
        }
        Add { name, tracker, non_interactive } => {
            let tracker_url = tracker.unwrap_or_else(|| "http://localhost:8080".to_string());
            println!("Searching tracker at {} for '{}'...", tracker_url, name);
            if non_interactive {
                println!("  Mode: non-interactive (auto-select highest-scored)");
            } else {
                println!("  Mode: interactive (prompt for variant selection)");
            }
            println!();
            println!("Note: Full add requires a running tracker. Start one with:");
            println!("  monopsony-tracker");
        }
        Unregister { name } => {
            println!("Unregistering shadow package: {}", name);
            println!("Note: Requires server connection.");
        }
        List => {
            println!("Listing registered shadow packages...");
            println!("Note: Requires server connection.");
        }
        Inspect { name } => {
            println!("Inspecting shadow package: {}", name);
            println!("Note: Requires server connection.");
        }
        Check { name } => {
            if let Some(pkg) = name {
                println!("Checking updates for: {}", pkg);
            } else {
                println!("Checking updates for all shadow packages...");
            }
            println!("Note: Requires server connection.");
        }
        Update { name, dry_run, no_restart: _ } => {
            if dry_run {
                println!("Dry-run: checking updates for '{}'...", name);
            } else {
                println!("Updating shadow package: {}", name);
            }

            // Load trust configuration from client config
            let config_path = std::env::var("MONOPSONY_CLIENT_CONFIG")
                .unwrap_or_else(|_| "/etc/monopsony/client.toml".to_string());
            match std::fs::read_to_string(&config_path) {
                Ok(content) => {
                    match toml::from_str::<monopsony_common::config::ClientConfig>(&content) {
                        Ok(config) => {
                            let trust_config = &config.shadow_trust;
                            println!("Trust policy: {}", trust_config.policy);
                            if !trust_config.trusted_author_keys.is_empty() {
                                println!(
                                    "Trusted author keys: {}",
                                    trust_config.trusted_author_keys.join(", ")
                                );
                            }
                            if !trust_config.trusted_operator_keys.is_empty() {
                                println!(
                                    "Trusted operator keys: {}",
                                    trust_config.trusted_operator_keys.join(", ")
                                );
                            }
                            // When a signed recipe arrives from the server, trust
                            // verification happens before execute_build/execute_script.
                            // See: trust::verify_recipe_trust()
                            println!("Trust enforcement: enabled (policy: {})", trust_config.policy);
                        }
                        Err(e) => eprintln!("Warning: could not parse client config: {}", e),
                    }
                }
                Err(_) => {
                    println!("Trust enforcement: default (no client config found)");
                }
            }

            println!("Note: Requires server connection for full update.");
        }
        Rollback { name } => {
            println!("Rolling back shadow package: {}", name);
            println!("Note: Requires server connection for full rollback.");
            println!("Available rollback versions can be listed once connected.");
        }
        Recipe { command: sub } => {
            match sub {
                shadow_cli::RecipeCommand::Edit { name } => {
                    println!("Edit recipe for: {}", name);
                    println!("Recipe files are stored in /etc/monopsony/recipes/<name>.toml");
                }
                shadow_cli::RecipeCommand::Validate { path } => {
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            match toml::from_str::<monopsony_common::shadow_types::ShadowRecipe>(&content) {
                                Ok(recipe) => println!("Recipe {} is valid (package: {}).", path, recipe.package.name),
                                Err(e) => eprintln!("Recipe validation failed: {}", e),
                            }
                        }
                        Err(e) => eprintln!("Cannot read {}: {}", path, e),
                    }
                }
            }
        }
    }
    Ok(())
}
