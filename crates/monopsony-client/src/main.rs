//! Monopsony Client - Local proxy and package manager integration

mod proxy_toggle;
mod shadow;
mod shadow_cli;
mod shadow_executor;
mod shadow_provision;
mod system_proxy;
mod ws_transport;

use anyhow::Context;
use clap::{Parser, Subcommand};
use rand::Rng;
use sha2::Digest;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

// Platform-conditional default paths for config and certificate directories.
fn default_config_path() -> String {
    if cfg!(windows) {
        std::env::var("ProgramData")
            .map(|d| format!("{}\\monopsony\\client.toml", d))
            .unwrap_or_else(|_| "C:\\ProgramData\\monopsony\\client.toml".to_string())
    } else {
        "/etc/monopsony/client.toml".to_string()
    }
}

fn default_cert_dir() -> String {
    if cfg!(windows) {
        std::env::var("APPDATA")
            .map(|d| format!("{}\\monopsony", d))
            .unwrap_or_else(|_| "C:\\Users\\%USERNAME%\\AppData\\Roaming\\monopsony".to_string())
    } else {
        "~/.config/monopsony".to_string()
    }
}

mod auto_update;
mod catalog_scan;
mod content_cache;
mod detect;
mod discovery;
mod floor_guard;
mod grpc_client;
mod hit_counter;
mod index_parse;
mod lang_pm_config;
mod ledger;
mod oracle_update;
mod pin_verify;
mod proxy;
mod self_update;
mod trust;
mod shadow_scorer;
mod shadow_service;
mod update_health;

#[derive(Parser, Debug)]
#[command(name = "monopsony-client", version)]
#[command(about = "Monopsony client daemon and CLI", long_about = None)]
#[command(propagate_version = true)]
struct Args {
    /// Path to TOML configuration file
    #[arg(short, long)]
    config: Option<String>,

    /// Directory for client certificates and pairing data
    #[arg(long)]
    cert_dir: Option<String>,

    /// Enable debug logging (shows all trace-level output from proxy, gRPC, discovery)
    #[arg(short, long)]
    debug: bool,

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

        /// Per-attempt timeout in ms (connect + headers). The retry loop's
        /// total budget is --retry-deadline-ms.
        #[arg(long, default_value = "30000")]
        timeout_ms: u64,

        /// Total retry budget in ms before returning 503 to the package
        /// manager. Sized for multi-minute upstream fetches of large
        /// packages (rust, linux-firmware) through a cold cache.
        #[arg(long, default_value = "300000")]
        retry_deadline_ms: u64,

        /// First backoff step between retries (ms); doubles per attempt,
        /// capped at 30 s, ±20% jitter.
        #[arg(long, default_value = "2000")]
        retry_backoff_initial_ms: u64,
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
    Status {
        /// Machine-quiet output: suppress the tracing log prefix so stdout
        /// carries only the key: value lines the menu bar app parses.
        #[arg(long)]
        quiet: bool,
    },
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

        /// 6-character pairing code from the server (legacy mode)
        #[arg(long)]
        code: Option<String>,

        /// Use v2 Sunshine pairing: client generates a PIN, displays it,
        /// submits a request, polls for admin approval, then collects the
        /// cert with proof of possession. Requires pairing_mode=pin server.
        #[arg(long)]
        pin: bool,

        /// Enroll with a pre-approval key (mopk-...): non-interactive fleet
        /// pairing. Requires a valid, unexhausted enrollment key.
        #[arg(long)]
        enrollment_key: Option<String>,
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
    /// Manage proxy configuration for package managers, Docker, and shadow packages
    Proxy {
        #[command(subcommand)]
        cmd: ProxyCommand,
    },
    /// Self-update monopsony (server or client) from GitHub releases via the shadow system
    SelfUpdate {
        #[command(subcommand)]
        cmd: SelfUpdateMode,
    },
    /// Manage signed downgrade exceptions in the local ledger
    Override {
        #[command(subcommand)]
        mode: OverrideMode,
    },
    /// One-shot platform setup tasks (macOS Content Caching, etc.)
    Setup {
        #[command(subcommand)]
        cmd: SetupMode,
    },
    /// Trigger a macOS softwareupdate background scan (the only live SUS
    /// verb on current macOS; whether the scan's traffic routes through
    /// Monopsony depends on system-proxy / Content Caching state)
    UpdateScan,
    /// Uninstall client-side integration: remove proxy configuration from
    /// every detected package manager (and Docker/shadow hooks), leaving the
    /// daemon + certs untouched. Run before package removal.
    Uninstall,
}

#[derive(Subcommand, Debug)]
pub enum SetupMode {
    /// Interactive setup (P6): detect installed package managers, configure
    /// each to route through the Monopsony proxy, and print init-placement
    /// guidance. Idempotent and re-runnable.
    Interactive {
        /// Server address used to derive proxy settings
        /// (default: saved server.json, else 127.0.0.1:3128)
        #[arg(long)]
        server: Option<String>,
        /// Configure ALL detected subsystems without prompting
        /// (non-interactive; useful for scripts/Ansible)
        #[arg(long)]
        all: bool,
    },
    /// macOS: activate Apple Content Caching (shared content only — no
    /// iCloud personal-blob caching). One sudo prompt; needs an admin user
    /// (rc=77 without root). Serves OS updates + App Store content to OTHER
    /// Apple devices on the LAN.
    Mac {
        /// Show Content Caching status without changing anything
        #[arg(long)]
        status: bool,
        /// Deactivate Content Caching (cached content stays on disk)
        #[arg(long)]
        disable: bool,
    },
}

#[derive(Subcommand, Debug)]
enum OverrideMode {
    /// Authorize downgrades of a package until the TTL expires
    Add {
        /// Package key ("<pm>:<name>", e.g. pacman:bash)
        #[arg(long)]
        package: String,

        /// Target (lower) version the downgrades are expected to land on
        #[arg(long)]
        to: String,

        /// Why the downgrade is authorized
        #[arg(long)]
        reason: String,

        /// Exception lifetime in seconds (default 1 week)
        #[arg(long)]
        ttl_secs: Option<i64>,
    },
    /// List active (unexpired) exceptions
    List,
}

#[derive(Subcommand, Debug)]
enum SelfUpdateMode {
    /// Check for available updates and show version info
    Check,
    /// Download and apply the latest update (atomic swap + health check)
    Apply,
}

/// Detect which binary we are by examining argv[0]
fn detect_binary_type() -> &'static str {
    if let Some(argv0) = std::env::args().next() {
        let name = std::path::Path::new(&argv0)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        match name {
            "monopsony-server" => "server",
            _ => "client",
        }
    } else {
        "unknown"
    }
}

async fn run_self_update(cmd: SelfUpdateMode) -> anyhow::Result<()> {
    let binary_type = detect_binary_type();

    match cmd {
        SelfUpdateMode::Check => {
            info!("Checking for {} updates...", binary_type);

            // Get current version from env var set by cargo at build time
            let current_version = env!("CARGO_PKG_VERSION").to_string();
            println!("Current {}: {}", binary_type, current_version);

            let result = self_update::check_for_update(&current_version).await;

            match result {
                Ok(result) => {
                    if let Some(latest) = &result.latest_version {
                        println!("Update available: {} -> {}", result.current_version, latest);
                        if let Some(url) = &result.changelog_url {
                            println!("Changelog: {}", url);
                        }
                        std::process::exit(1); // Exit code 1 indicates update available
                    } else {
                        println!(
                            "You are running the latest version of monopsony {}.",
                            binary_type
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Failed to check for updates: {}", e);
                    std::process::exit(2);
                }
            }
        }

        SelfUpdateMode::Apply => {
            info!("Applying {} update...", binary_type);

            let current_version = env!("CARGO_PKG_VERSION").to_string();

            let result = self_update::check_for_update(&current_version).await;

            match result {
                Ok(result) => {
                    if result.latest_version.is_none() {
                        println!("Already at latest version. Nothing to do.");
                        return Ok(());
                    }

                    // Check if we have an asset URL for this platform
                    let asset_url = match result.asset_url {
                        Some(url) => url,
                        None => {
                            eprintln!("No compatible release asset found for this platform.");
                            eprintln!(
                                "Available at: {}",
                                result.changelog_url.as_deref().unwrap_or("GitHub releases")
                            );
                            std::process::exit(3);
                        }
                    };

                    // Determine current binary path
                    let current_exe =
                        std::env::current_exe().context("Failed to get current executable path")?;

                    let target_dir = current_exe
                        .parent()
                        .context("Failed to get parent directory of binary")?
                        .to_path_buf();

                    println!("Downloading update from: {}", asset_url);
                    println!("Installing to: {}", current_exe.display());

                    // Try parsing the default recipe for the managed pipeline
                    let recipe = toml::from_str::<monopsony_common::shadow_types::ShadowRecipe>(
                        monopsony_common::shadow_types::DEFAULT_MONOPSONY_RECIPE,
                    )
                    .ok();

                    if recipe.is_some() {
                        // Managed update with full safety pipeline
                        let mut ctx = self_update::UpdateContext::new(
                            recipe,
                            current_exe,
                            target_dir,
                            current_version,
                            result
                                .latest_version
                                .as_deref()
                                .unwrap_or("unknown")
                                .to_string(),
                            asset_url,
                        );

                        match self_update::apply_update_managed(&mut ctx).await {
                            Ok(self_update::UpdateOutcome::Success) => {
                                println!("Update applied successfully! Restart monopsony to use the new version.");
                            }
                            Ok(self_update::UpdateOutcome::RolledBack(reason)) => {
                                eprintln!("Update rolled back: {}", reason);
                                eprintln!("Your system has been restored to the previous version.");
                                std::process::exit(5);
                            }
                            Err(e) => {
                                eprintln!("Failed to apply update: {}", e);
                                std::process::exit(4);
                            }
                        }
                    } else {
                        // Fallback: raw download + swap (no health check)
                        match self_update::apply_update(
                            &current_exe,
                            &target_dir,
                            &asset_url,
                            result.latest_version.as_deref().unwrap_or("unknown"),
                        )
                        .await
                        {
                            Ok(()) => {
                                println!("Update applied successfully! Restart monopsony to use the new version.");
                            }
                            Err(e) => {
                                eprintln!("Failed to apply update: {}", e);
                                std::process::exit(4);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to check for updates: {}", e);
                    std::process::exit(2);
                }
            }
        }
    }

    Ok(())
}

#[derive(Subcommand, Debug)]
enum ProxyCommand {
    /// Show current proxy status for all subsystems
    Status {
        /// Show status for a specific package manager (pacman, apt, dnf, apk, winget, choco, scoop, npm, pip, cargo, brew)
        #[arg(long)]
        pm: Option<String>,
        /// Show macOS system-proxy forwarding status
        #[arg(long)]
        system: bool,
    },
    /// Enable proxying for specified subsystems
    On {
        /// Enable specific package manager (pacman, apt, dnf, apk, winget, choco, scoop, npm, pip, cargo, brew)
        #[arg(long)]
        pm: Option<String>,
        /// Enable Docker registry mirror
        #[arg(long)]
        docker: bool,
        /// Enable shadow package trust
        #[arg(long)]
        shadow: bool,
        /// Enable macOS system-proxy forwarding (forwarded, NOT cached)
        #[arg(long)]
        system: bool,
        /// Enable all detected subsystems
        #[arg(long)]
        all: bool,
        /// Server address for proxy configuration
        #[arg(long)]
        server: Option<String>,
    },
    /// Disable proxying for specified subsystems
    Off {
        /// Disable specific package manager (pacman, apt, dnf, apk, winget, choco, scoop, npm, pip, cargo, brew)
        #[arg(long)]
        pm: Option<String>,
        /// Disable Docker registry mirror
        #[arg(long)]
        docker: bool,
        /// Disable shadow package trust
        #[arg(long)]
        shadow: bool,
        /// Disable macOS system-proxy forwarding
        #[arg(long)]
        system: bool,
        /// Disable all subsystems
        #[arg(long)]
        all: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the default crypto provider for rustls (required by rustls 0.23)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let args = Args::parse();

    // `status --quiet` is a machine contract: stdout carries ONLY the key:
    // value lines the menu bar app parses. Skip subscriber installation
    // entirely — info!/warn! macros become no-ops instead of prefixing lines.
    let quiet_status = matches!(args.command, Some(Commands::Status { quiet: true }));
    if !quiet_status {
        let log_level = if args.debug {
            Level::DEBUG
        } else {
            Level::INFO
        };
        let subscriber = FmtSubscriber::builder().with_max_level(log_level).finish();
        tracing::subscriber::set_global_default(subscriber)?;
    }

    // Resolve cert_dir — platform-aware defaults. Windows uses APPDATA; Linux uses ~/.config/monopsony.
    let cert_dir_raw = args.cert_dir.clone().unwrap_or_else(default_cert_dir);

    let cert_dir = if cfg!(windows) {
        PathBuf::from(cert_dir_raw) // Windows: APPDATA paths don't need tilde expansion
    } else {
        shellexpand::tilde(&cert_dir_raw)
            .to_string()
            .parse::<PathBuf>()
            .unwrap_or_else(|_| PathBuf::from(cert_dir_raw))
    };
    // Resolve config path — platform-aware default if not provided via --config
    let config_path = args.config.clone().unwrap_or_else(default_config_path);

    match args.command {
        Some(Commands::Daemon {
            proxy_addr,
            server,
            timeout_ms,
            retry_deadline_ms,
            retry_backoff_initial_ms,
        }) => {
            run_daemon(
                proxy_addr,
                server,
                timeout_ms,
                retry_deadline_ms,
                retry_backoff_initial_ms,
                cert_dir,
                &config_path,
            )
            .await?;
        }
        Some(Commands::Discover {
            timeout_secs,
            tracker_url,
            peer_url,
            distro,
        }) => {
            run_discover(timeout_secs, tracker_url, peer_url, &distro).await?;
        }
        Some(Commands::Status { quiet }) => {
            if quiet {
                run_status_quiet(&cert_dir);
            } else {
                run_status(&cert_dir).await?;
            }
        }
        Some(Commands::Configure { server }) => {
            run_configure(&server).await?;
        }
        Some(Commands::Pair { server, code, pin, enrollment_key }) => {
            run_pair(&server, code.as_deref(), pin, enrollment_key.as_deref(), &cert_dir).await?;
        }
        Some(Commands::Mirror {
            server,
            distro,
            mirror_url,
        }) => {
            run_mirror(&server, &distro, mirror_url.as_deref()).await?;
        }
        Some(Commands::CacheStats { server }) => {
            run_cache_stats(&server).await?;
        }
        Some(Commands::Package { cmd }) => {
            run_shadow_package(cmd, &cert_dir).await?;
        }
        Some(Commands::SelfUpdate { cmd }) => {
            run_self_update(cmd).await?;
        }
        Some(Commands::Proxy { cmd }) => {
            run_proxy_command(cmd, &cert_dir).await?;
        }
        Some(Commands::Override { mode }) => {
            run_override(mode, &cert_dir).await?;
        }
        Some(Commands::Setup { cmd }) => {
            match cmd {
                SetupMode::Interactive { server, all } => {
                    run_setup_interactive(server.as_deref(), all, &cert_dir).await?;
                }
                other => run_setup(other)?,
            }
        }
        Some(Commands::UpdateScan) => {
            run_update_scan()?;
        }
        Some(Commands::Uninstall) => {
            run_uninstall(&cert_dir).await?;
        }
        None => {
            info!("No command specified. Use --help for usage.");
        }
    }

    Ok(())
}

/// Resolve server address using the fallback chain:
/// 1. cert_dir/server.json — ws_url field (from pairing, for CF tunnel deployments)
/// 2. cert_dir/server.json — grpc_addr field (from pairing, for direct TCP)
/// 3. mDNS discovery
/// 4. Error if all fail
async fn resolve_server_addr(cert_dir: &Path) -> anyhow::Result<String> {
    // Try server.json from pairing
    let server_json_path = cert_dir.join("server.json");
    if server_json_path.exists() {
        match std::fs::read_to_string(&server_json_path) {
            Ok(content) => {
                if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                    // Prefer ws_url (WebSocket tunnel) if present — this is set
                    // when the server is behind Cloudflare Tunnels or a reverse proxy.
                    if let Some(ws_url) = info.get("ws_url").and_then(|v| v.as_str()) {
                        if !ws_url.is_empty() {
                            info!("Using WebSocket tunnel from pairing: {}", ws_url);
                            return Ok(ws_url.to_string());
                        }
                    }
                    // Fall back to direct TCP gRPC address
                    if let Some(grpc_addr) = info.get("grpc_addr").and_then(|v| v.as_str()) {
                        if !grpc_addr.is_empty() {
                            info!("Using server from pairing: {}", grpc_addr);
                            return Ok(grpc_addr.to_string());
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Failed to read server.json: {}", e);
            }
        }
    }

    // Fall back to mDNS
    info!("Discovering server via mDNS...");
    let disc = discovery::ClientDiscovery::new("monopsony");
    let found =
        disc.discover_one(Duration::from_secs(10))
            .await
            .map_err(|e| {
                anyhow::anyhow!(
            "Server discovery failed (no --server provided, no server.json, and mDNS failed). \
             Is the server running and on the same network? Error: {}", e
        )
            })?;
    let addr = format!("{}:{}", found.address, found.grpc_port);
    info!("Discovered server at {}", addr);
    Ok(addr)
}

/// Build an mTLS-enabled connection to the server for shadow package
/// commands (Sprint 22 slice 22-A). Requires paired certificates; without
/// them the caller prints pairing guidance and exits non-zero.
async fn connect_shadow_client(
    server: Option<&str>,
    cert_dir: &Path,
) -> anyhow::Result<(grpc_client::MonopsonyGrpcClient, String)> {
    use tracing::warn;

    let certs = grpc_client::ClientCerts::load(cert_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "No client certificates found in {}.\n\
             Shadow package commands require a paired client. Run:\n  \
             monopsony-client pair --server <host> --code <code>\n\
             then retry.",
            cert_dir.display()
        )
    })?;

    let server_addr = match server {
        Some(s) => s.to_string(),
        None => resolve_server_addr(cert_dir).await?,
    };

    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(&certs.ca_cert_pem))
        .identity(tonic::transport::Identity::from_pem(
            &certs.client_cert_pem,
            &certs.client_key_pem,
        ))
        .domain_name("monopsony-server")
        .assume_http2(true);

    let mut client = if server_addr.starts_with("wss://") || server_addr.starts_with("ws://") {
        grpc_client::MonopsonyGrpcClient::connect_ws(&server_addr, Some(tls)).await?
    } else {
        grpc_client::MonopsonyGrpcClient::connect(&server_addr, Some(tls)).await?
    };

    // Read the persisted client ID (written at pair/register time). If the
    // file is missing, re-register to recover the identity.
    let client_id = match std::fs::read_to_string(cert_dir.join("client_id")) {
        Ok(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            warn!("client_id file missing; re-registering with server");
            let hostname = hostname::get()?.to_string_lossy().to_string();
            let backends = monopsony_common::package_manager::get_available_backends();
            let managers: Vec<String> = backends
                .iter()
                .map(|b| b.kind().as_str().to_string())
                .collect();
            let (id, _, _) = client
                .register(
                    &hostname,
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    &managers,
                )
                .await?;
            std::fs::write(cert_dir.join("client_id"), &id)?;
            id
        }
    };

    Ok((client, client_id))
}

/// Serialize a ShadowSource the same way serde snake_case does on the wire
/// ("github_release" / "git_release" / "direct_url").
fn shadow_source_as_str(source: &monopsony_common::shadow_types::ShadowSource) -> &'static str {
    match source {
        monopsony_common::shadow_types::ShadowSource::GithubRelease => "github_release",
        monopsony_common::shadow_types::ShadowSource::GitRelease => "git_release",
        monopsony_common::shadow_types::ShadowSource::DirectUrl => "direct_url",
    }
}

/// Ledger DB parent dir: the client state dir (~/.local/state/monopsony),
/// same convention as the tamper-event spool. NOT cert_dir — the anti-rewind
/// anchor must survive re-pairing.
fn dirs_ledger_parent(_cert_dir: &Path) -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local/state/monopsony"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/monopsony"))
}

/// `override add` / `override list` — signed downgrade exceptions in the
/// local ledger. The signing key lives next to ledger.sqlite3
/// (ledger-override.key, 0600, OsRng-generated on first use); records carry
/// the signature + verifying key so the ledger can verify them on consult
/// (see ledger.rs active_exception_on).
async fn run_override(mode: OverrideMode, cert_dir: &Path) -> anyhow::Result<()> {
    use ledger::SignedException;

    match mode {
        OverrideMode::Add {
            package,
            to,
            reason,
            ttl_secs,
        } => {
            // Validation: exactly one ':' with non-empty pm and name.
            if package.matches(':').count() != 1 {
                anyhow::bail!(
                    "--package must be \"<pm>:<name>\" with exactly one ':' (e.g. pacman:bash)"
                );
            }
            let (pm, name) = package.split_once(':').expect("one colon checked above");
            if pm.is_empty() || name.is_empty() {
                anyhow::bail!("--package pm and name must both be non-empty (e.g. pacman:bash)");
            }
            if reason.trim().is_empty() {
                anyhow::bail!("--reason must not be empty");
            }
            if to.is_empty() {
                anyhow::bail!("--to must not be empty");
            }
            let ttl = ttl_secs.unwrap_or(604_800);

            let state_dir = dirs_ledger_parent(cert_dir);
            let ledger = ledger::Ledger::open(&state_dir.join("ledger.sqlite3"))
                .map_err(|e| anyhow::anyhow!("Failed to open ledger at {:?}: {}", state_dir, e))?;

            // Load-or-generate the override signing key (hex text, 0600).
            let key_path = state_dir.join("ledger-override.key");
            let key_bytes: [u8; 32] = match std::fs::read_to_string(&key_path) {
                Ok(text) => {
                    let raw = hex::decode(text.trim()).map_err(|e| {
                        anyhow::anyhow!("Corrupt override key {:?}: {}", key_path, e)
                    })?;
                    raw.try_into().map_err(|v: Vec<u8>| {
                        anyhow::anyhow!(
                            "Corrupt override key {:?}: {} bytes (expected 32)",
                            key_path,
                            v.len()
                        )
                    })?
                }
                Err(_) => {
                    use rand::RngCore;
                    let mut k = [0u8; 32];
                    rand::rngs::OsRng.fill_bytes(&mut k);
                    std::fs::write(&key_path, hex::encode(k))
                        .with_context(|| format!("Writing {:?}", key_path))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &key_path,
                            std::fs::Permissions::from_mode(0o600),
                        );
                    }
                    k
                }
            };
            let signing_key = ed25519_dalek::SigningKey::from_bytes(&key_bytes);

            let now = chrono::Utc::now().timestamp();
            let mut rec = SignedException {
                id: 0,
                package_key: package.clone(),
                to_version: to.clone(),
                reason: reason.clone(),
                created_at: now,
                expires_at: now + ttl,
                signature: None,
                signing_key: None,
            };
            rec.sign(&signing_key);
            ledger
                .add_signed_exception(&rec)
                .map_err(|e| anyhow::anyhow!("Failed to record exception: {}", e))?;

            // Footgun UX: make it explicit what was just permitted.
            let expiry_rfc3339 = chrono::DateTime::from_timestamp(rec.expires_at, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| rec.expires_at.to_string());
            println!("Package: {}", rec.package_key);
            println!("Target version: {}", rec.to_version);
            println!("Reason: {}", rec.reason);
            println!("Expires: {}", expiry_rfc3339);
            if let Some(held) = ledger.high_water_for(&rec.package_key) {
                println!(
                    "Current high-water (ledger will hold downgrades below): {}",
                    held
                );
            }
            println!(
                "Downgrades of {} will be permitted until {}.",
                rec.package_key, expiry_rfc3339
            );
        }
        OverrideMode::List => {
            let state_dir = dirs_ledger_parent(cert_dir);
            let ledger = ledger::Ledger::open(&state_dir.join("ledger.sqlite3"))
                .map_err(|e| anyhow::anyhow!("Failed to open ledger at {:?}: {}", state_dir, e))?;
            let now = chrono::Utc::now().timestamp();
            let rows = ledger
                .list_active_exceptions(now)
                .map_err(|e| anyhow::anyhow!("Failed to list exceptions: {}", e))?;
            if rows.is_empty() {
                println!("No active exceptions.");
                return Ok(());
            }
            for r in rows {
                let expiry = chrono::DateTime::from_timestamp(r.expires_at, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| r.expires_at.to_string());
                let reason: String = r.reason.chars().take(60).collect();
                println!(
                    "{}  {}  {}  {}  {}",
                    r.id, r.package_key, r.to_version, expiry, reason
                );
            }
        }
    }
    Ok(())
}

/// Load the pinned mirror verifying key saved at pair/register time.
/// Returns None when absent, wrong-sized, or zeroed (not provisioned).
/// Load the operator (server shadow-manager) verifying key if configured.
/// Accepts a hex-encoded 32-byte Ed25519 public key at
/// `[shadow_trust].operator_verifying_key_path`. Returns None (and warns)
/// when unset or unreadable — trust policy then fails closed, by design.
fn load_operator_verifying_key(
    trust_config: &monopsony_common::config::ShadowPackageTrustConfig,
) -> Option<ed25519_dalek::VerifyingKey> {
    let path = trust_config.operator_verifying_key_path.as_deref()?;
    let text = std::fs::read_to_string(path).ok()?;
    let bytes = hex::decode(text.trim()).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&arr).ok()
}

/// Read one line from stdin. Used only for interactive prompts (register
/// adopt-from-peer choice, update re-approval); never used by the daemon.
fn read_line(prompt: &str) -> anyhow::Result<String> {
    use std::io::{self, Write};
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn load_mirror_verify_key(cert_dir: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(cert_dir.join("mirror-verify.key")).ok()?;
    if bytes.len() != 32 {
        warn!(
            "mirror-verify.key has {} bytes (expected 32) — ignoring",
            bytes.len()
        );
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    if key == [0u8; 32] {
        return None;
    }
    Some(key)
}

/// Persist the server's mirror verifying key (pair/register payload).
/// Empty or zeroed payloads are skipped — pinning stays inert.
fn store_mirror_verify_key(cert_dir: &Path, key: &[u8]) {
    if key.len() != 32 || key.iter().all(|&b| b == 0) {
        return;
    }
    match std::fs::write(cert_dir.join("mirror-verify.key"), key) {
        Ok(()) => info!("Mirror verify key saved for pin verification"),
        Err(e) => warn!("Could not persist mirror verify key: {}", e),
    }
}

/// Pure gate decision for remote shadow assignments (Sprint 22 slice 22-E2b):
/// both the client opt-in ([shadow_trust].accept_remote_assignment) and the
/// trust check must pass.
fn remote_assignment_allowed(policy_accepts: bool, trust_ok: bool) -> bool {
    policy_accepts && trust_ok
}

/// The wire carries the operator signature as hex or base64; trust.rs decodes
/// base64 only. Re-encode hex input (an Ed25519 signature is exactly 64 bytes)
/// as base64 so either encoding verifies.
fn normalize_signature_encoding(signature: &str) -> String {
    use base64::Engine;
    if let Ok(bytes) = hex::decode(signature) {
        if bytes.len() == 64 {
            return base64::engine::general_purpose::STANDARD.encode(bytes);
        }
    }
    signature.to_string()
}

/// Build the `SignedRecipe` shape trust.rs expects from the wire params and
/// verify it against the client trust policy. The operator verifying key is
/// the pinned mirror key — the only operator key the client holds locally.
fn verify_signed_recipe(
    recipe_toml: &str,
    key_id: &str,
    signature: &str,
    parsed: Option<&monopsony_common::shadow_types::ShadowRecipe>,
    trust_config: &monopsony_common::config::ShadowPackageTrustConfig,
    mirror_verify_key: Option<[u8; 32]>,
) -> anyhow::Result<()> {
    let recipe = parsed
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("recipe TOML failed strict parse"))?;
    let signed = monopsony_common::shadow_types::SignedRecipe {
        recipe_toml: recipe_toml.to_string(),
        recipe,
        operator_signature: monopsony_common::shadow_types::RecipeSignature {
            key_id: key_id.to_string(),
            signature: normalize_signature_encoding(signature),
            signed_at: chrono::Utc::now(),
        },
        author_signature: None,
    };
    let vk = mirror_verify_key
        .map(|k| ed25519_dalek::VerifyingKey::from_bytes(&k))
        .transpose()?;
    // Fall back to the configured operator verifying key when no pairing-time
    // mirror key is present — the operator key is the canonical server signer
    // for shadow recipes (mirror-verify.key predates Sprint 22 and only
    // exists on clients that stored one at pair time).
    let vk = match vk {
        Some(k) => Some(k),
        None => load_operator_verifying_key(trust_config),
    };
    trust::verify_recipe_trust(&signed, trust_config, vk.as_ref())
}

/// Consume a `shadow_assign` remote assignment (Sprint 22 slice 22-E2b).
///
/// Gate order: client opt-in → trust policy → local install detection →
/// registration. Every refusal/failure logs a warning and returns; nothing
/// is printed to stdout and the daemon loop continues untouched.
#[allow(clippy::too_many_arguments)]
async fn handle_remote_assignment(
    client: &mut grpc_client::MonopsonyGrpcClient,
    client_id: &str,
    params: &std::collections::HashMap<String, String>,
    accept_remote: bool,
    trust_config: &monopsony_common::config::ShadowPackageTrustConfig,
    mirror_verify_key: Option<[u8; 32]>,
    update_health: &update_health::UpdateHealthTracker,
) {
    // (a) Client opt-in gate — refuse before touching the payload.
    if !accept_remote {
        warn!("remote assignment refused: client opt-in disabled");
        return;
    }

    // (a2) Idempotency: if this package is already registered locally the
    // assignment was already consumed — re-pushed pendings must not create
    // duplicate registrations (caught live on the bench: 1800+ duplicates
    // from a re-delivered pending).
    let recipe_name_probe = params
        .get("recipe_name")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if !recipe_name_probe.is_empty() {
        if let Ok(packages) = client.list_shadow_packages(client_id).await {
            if packages.iter().any(|p| p.name == recipe_name_probe) {
                debug!(
                    package = %recipe_name_probe,
                    "remote assignment already registered — skipping duplicate"
                );
                return;
            }
        }
    }

    // (b) Parse the payload leniently. Empty recipe_toml → warn + ignore;
    // recipe_name comes from params with a fallback to [package].name.
    let recipe_toml = params
        .get("recipe_toml")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if recipe_toml.trim().is_empty() {
        warn!("remote assignment ignored: empty recipe_toml");
        return;
    }
    let wire_sig = params
        .get("signature")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let key_id = params
        .get("key_id")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    // Strict parse for the trust path; lenient parse for the name fallback.
    let parsed: Option<monopsony_common::shadow_types::ShadowRecipe> =
        toml::from_str(&recipe_toml).ok();
    let lenient: Option<toml::Value> = toml::from_str(&recipe_toml).ok();
    let param_name = params
        .get("recipe_name")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let recipe_name = if !param_name.is_empty() {
        param_name
    } else {
        match lenient
            .as_ref()
            .and_then(|doc| doc.get("package"))
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            .filter(|n| !n.is_empty())
        {
            Some(n) => n.to_string(),
            None => {
                warn!(
                    "remote assignment ignored: no recipe_name param and recipe TOML has no [package].name"
                );
                return;
            }
        }
    };

    // (c) Trust verification per client policy. Empty signature is decided
    // by policy: strict policies refuse, permissive ones accept with a warn.
    let trust_ok = if wire_sig.trim().is_empty() {
        match trust_config.policy.as_str() {
            "trusted_server_only" | "paranoid" => {
                warn!(
                    "remote assignment refused: unsigned recipe under strict policy '{}'",
                    trust_config.policy
                );
                false
            }
            _ => {
                warn!("unsigned recipe accepted under permissive policy");
                true
            }
        }
    } else {
        match verify_signed_recipe(
            &recipe_toml,
            &key_id,
            &wire_sig,
            parsed.as_ref(),
            trust_config,
            mirror_verify_key,
        ) {
            Ok(()) => true,
            Err(e) => {
                warn!(
                    "remote assignment refused: trust verification failed: {}",
                    e
                );
                false
            }
        }
    };

    if !remote_assignment_allowed(accept_remote, trust_ok) {
        // Specific reason already warned above.
        return;
    }

    // (d) Local confirm: the package must already exist on this host.
    let finding = catalog_scan::scan_entry(&recipe_name, &recipe_toml);
    if !finding.installed {
        warn!("assignment ignored: package not detected on this host");
        return;
    }
    let detected_version = if finding.detected_version.is_empty() {
        String::new()
    } else {
        finding.detected_version.clone()
    };

    // (e) Record the assignment — same shape as the Adopt arm, but with
    // source_type "remote_assigned". Errors are warned and swallowed: the
    // daemon must never degrade.
    let source_url = lenient
        .as_ref()
        .and_then(|doc| doc.get("package"))
        .and_then(|p| p.get("source_url"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (install_paths, service_name) = shadow_cli::extract_install_fields(&recipe_toml);
    // Sprint 27 Part B: an assignment that passed every policy gate and is
    // being registered counts as one update-pipeline attempt.
    update_health.record_attempt(chrono::Utc::now().timestamp());
    match client
        .register_shadow_package(
            client_id,
            &recipe_name,
            &detected_version,
            std::env::consts::ARCH,
            std::env::consts::OS,
            "", // libc: unknown from the heartbeat path
            "remote_assigned",
            &source_url,
            &recipe_toml,
            &install_paths,
            &[], // config_paths: not consumed from the pushed recipe
            &service_name,
            "", // binary_sha256: not probed on the heartbeat path
            false, // mask_from_pm: opt-in only
        )
        .await
    {
        Ok(info) => info!(
            package = %recipe_name,
            id = %info.id,
            version = %detected_version,
            "Remote shadow assignment recorded"
        ),
        Err(e) => {
            // Sprint 27 Part B: operational failure → windowed telemetry.
            update_health.record_failure(chrono::Utc::now().timestamp());
            warn!("remote assignment registration failed (ignored): {}", e);
        }
    }
}

/// Handle a Command yielded alongside a heartbeat (Sprint 22 slice 22-E2b).
/// Currently the only consumer is the `shadow_assign` UPDATE_CONFIG command;
/// anything else is logged at debug and ignored. Never propagates errors to
/// the heartbeat loop.
async fn handle_heartbeat_command(
    client: &mut grpc_client::MonopsonyGrpcClient,
    client_id: &str,
    command: monopsony_common::proto::monopsony::Command,
    accept_remote: bool,
    trust_config: &monopsony_common::config::ShadowPackageTrustConfig,
    mirror_verify_key: Option<[u8; 32]>,
    update_health: &update_health::UpdateHealthTracker,
) {
    use monopsony_common::proto::monopsony::command::CommandType;

    let ctype = command.r#type();
    if ctype != CommandType::UpdateConfig {
        debug!(command_type = ?ctype, "Ignoring server command");
        return;
    }
    let operation = command
        .params
        .get("operation")
        .map(|s| s.as_str())
        .unwrap_or("");
    if operation != "shadow_assign" {
        debug!(operation, "Ignoring UPDATE_CONFIG command");
        return;
    }
    handle_remote_assignment(
        client,
        client_id,
        &command.params,
        accept_remote,
        trust_config,
        mirror_verify_key,
        update_health,
    )
    .await;
}

async fn run_daemon(
    proxy_addr: String,
    server: Option<String>,
    timeout_ms: u64,
    retry_deadline_ms: u64,
    retry_backoff_initial_ms: u64,
    cert_dir: PathBuf,
    config_path: &str,
) -> anyhow::Result<()> {
    info!("Starting Monopsony Client v{}", env!("CARGO_PKG_VERSION"));

    // Log system info for diagnostics
    debug!(
        "Platform: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    debug!("Config path: {}", config_path);
    debug!("Cert dir: {}", cert_dir.display());

    // Load client config (optional — daemon works without one using CLI args)
    let client_config = match std::fs::read_to_string(config_path) {
        Ok(toml_str) => match toml::from_str::<monopsony_common::config::ClientConfig>(&toml_str) {
            Ok(cfg) => {
                info!("Loaded config from {}", config_path);
                Some(cfg)
            }
            Err(e) => {
                warn!("Failed to parse config at {}: {}", config_path, e);
                None
            }
        },
        Err(_) => {
            info!("No config file at {} (using CLI args)", config_path);
            None
        }
    };

    // Validate auto-update config if present and enabled
    let auto_update_config = client_config.as_ref().map(|c| c.auto_update.clone());
    let report_telemetry = client_config
        .as_ref()
        .map(|c| c.client.report_install_telemetry)
        .unwrap_or(true);
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
            // Try config file first
            if let Some(ref config) = client_config {
                if let Some(ref addr) = config.server.address {
                    let full_addr = format!("{}:{}", addr, config.server.grpc_port);
                    info!("Using server from config: {}", full_addr);
                    full_addr
                } else {
                    // Try server.json from pairing, then mDNS
                    resolve_server_addr(&cert_dir).await?
                }
            } else {
                // Try server.json from pairing, then mDNS
                resolve_server_addr(&cert_dir).await?
            }
        }
    };

    let listen_addr: SocketAddr = proxy_addr.parse().expect("invalid proxy listen address");

    // Pin verification state (Sprint 20 Phase 3): provisioned from the key
    // saved at pair/register time; inert until then. Events spool under
    // cert_dir so they survive restarts and travel with the client identity.
    let pin_state = std::sync::Arc::new(pin_verify::PinState::new(
        load_mirror_verify_key(&cert_dir),
        cert_dir.join("tamper-events.jsonl"),
    ));
    if pin_state.provisioned() {
        info!("Integrity pin verification enabled (mirror key provisioned)");
    }

    // Build TLS config for the initial connection
    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(&certs.ca_cert_pem))
        .identity(tonic::transport::Identity::from_pem(
            &certs.client_cert_pem,
            &certs.client_key_pem,
        ))
        .domain_name("monopsony-server")
        .assume_http2(true);

    // Connect — use WebSocket tunnel if address starts with wss:// or ws://,
    // otherwise direct TCP with mTLS.
    let mut grpc_client = if server_addr.starts_with("wss://") || server_addr.starts_with("ws://") {
        info!("Using WebSocket tunnel transport");
        grpc_client::MonopsonyGrpcClient::connect_ws(&server_addr, Some(tls)).await?
    } else {
        grpc_client::MonopsonyGrpcClient::connect(&server_addr, Some(tls)).await?
    };

    // Get client ID from stored file or register
    let client_id = {
        let id_file = cert_dir.join("client_id");
        match std::fs::read_to_string(&id_file) {
            Ok(id) => id.trim().to_string(),
            Err(_) => {
                // Register with server to get client ID
                let hostname = hostname::get()?.to_string_lossy().to_string();
                let backends = monopsony_common::package_manager::get_available_backends();
                let managers: Vec<String> = backends
                    .iter()
                    .map(|b| b.kind().as_str().to_string())
                    .collect();

                let (id, _, mirror_key) = grpc_client
                    .register(
                        &hostname,
                        std::env::consts::OS,
                        std::env::consts::ARCH,
                        &managers,
                    )
                    .await?;
                store_mirror_verify_key(&cert_dir, &mirror_key);
                // Provision the in-memory PinState too: it was built from the
                // key file BEFORE this register, so without this the proxy
                // serves unverified until the next daemon restart (fleet
                // verification 2026-09-16, Bug 1). PinState is interior-
                // mutable — the Arc held by the proxy picks this up live.
                // A zeroed delivery legitimately disables (server turned
                // pinning off); set_key maps zeroed → None.
                if let Ok(key) = <[u8; 32]>::try_from(mirror_key.as_slice()) {
                    pin_state.set_key(key);
                }
                if pin_state.provisioned() {
                    info!("Integrity pin verification enabled (mirror key delivered at register)");
                }
                std::fs::write(&id_file, &id)?;
                info!("Registered with server, client ID: {}", id);
                id
            }
        }
    };

    // Load the trust config once for remote-assignment handling (Sprint 22
    // slice 22-E2b). Defaults to the strict policy when no config exists.
    let trust_config = client_config
        .as_ref()
        .map(|c| c.shadow_trust.clone())
        .unwrap_or_default();
    let _accept_remote = trust_config.accept_remote_assignment;
    let mirror_key_for_task = load_mirror_verify_key(&cert_dir);

    // Sprint 27 Part B: windowed update-failure telemetry (counts-only).
    // Producers record here; the heartbeat task snapshots it every beat.
    let update_health = std::sync::Arc::new(update_health::UpdateHealthTracker::new(
        update_health::DEFAULT_WINDOW_SECS,
    ));

    // Spawn background task for heartbeat and inventory sync
    let client_id_for_task = client_id.clone();
    let cert_dir_for_task = cert_dir.clone();
    let server_addr_for_task = server_addr.clone();
    let pin_for_heartbeat = pin_state.clone();
    let update_health_for_task = update_health.clone();
    let mut catalog_scan_guard = catalog_scan::ScanGuard::new(Duration::from_secs(600));
    let trust_config_for_task = trust_config.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let mut last_inventory = std::time::Instant::now();
        let mut backoff: std::time::Duration = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);
        const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
        const INVENTORY_INTERVAL: Duration = Duration::from_secs(300);

        // Live TLS config for the control connection. Rotated in place after
        // a successful mid-connection renewal — the connection itself is
        // never dropped; the new cert/key take effect on the next handshake.
        let mut tls_config: Option<tonic::transport::ClientTlsConfig> = None;

        loop {
            // Attempt connection with exponential backoff
            let mut certs = match grpc_client::ClientCerts::load(&cert_dir_for_task) {
                Some(c) => c,
                None => {
                    warn!("No certificates available for heartbeat");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };

            let tls = match &tls_config {
                Some(cfg) => cfg.clone(),
                None => {
                    let cfg = tonic::transport::ClientTlsConfig::new()
                        .ca_certificate(tonic::transport::Certificate::from_pem(&certs.ca_cert_pem))
                        .identity(tonic::transport::Identity::from_pem(
                            &certs.client_cert_pem,
                            &certs.client_key_pem,
                        ))
                        .domain_name("monopsony-server")
                        .assume_http2(true);
                    tls_config = Some(cfg.clone());
                    cfg
                }
            };

            let mut client = if server_addr_for_task.starts_with("wss://")
                || server_addr_for_task.starts_with("ws://")
            {
                match grpc_client::MonopsonyGrpcClient::connect_ws(&server_addr_for_task, Some(tls.clone()))
                    .await
                {
                    Ok(c) => {
                        info!("Connected to server via WebSocket tunnel");
                        backoff = Duration::from_secs(1);
                        c
                    }
                    Err(e) => {
                        warn!("WS connection failed (retry in {:?}): {}", backoff, e);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                }
            } else {
                match grpc_client::MonopsonyGrpcClient::connect(&server_addr_for_task, Some(tls.clone()))
                    .await
                {
                    Ok(c) => {
                        info!("Connected to server");
                        backoff = Duration::from_secs(1); // reset on success
                        c
                    }
                    Err(e) => {
                        warn!("Connection failed (retry in {:?}): {e:?}", backoff);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                }
            };

            // Connected — run heartbeat loop
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            let pin_for_drain = pin_for_heartbeat.clone();

            loop {
                interval.tick().await;

                // P1 renewal: inside the 30-day window, re-issue the cert
                // over the live mTLS channel, persist the new cert/key
                // atomically, and rotate the live TLS config. The control
                // connection itself is not dropped — the new creds take
                // effect on the next handshake.
                let renewal_due = certs.expiry().map(|expires| {
                    let days = (expires - chrono::Utc::now()).num_days();
                    (days, days <= grpc_client::CERT_RENEWAL_WINDOW_DAYS as i64)
                });
                if let Some((days, true)) = renewal_due {
                    info!(days, "Certificate in the 30-day renewal window — re-issuing over mTLS");
                    match client
                        .renew_client_cert(&client_id_for_task, certs.serial_hex().as_deref())
                        .await
                    {
                        Ok((new_certs, new_expiry)) => {
                            match new_certs.save_atomic(&cert_dir_for_task) {
                                Ok(()) => {
                                    // Rotate the live TLS config so subsequent
                                    // connections use the new cert/key pair.
                                    let renewed_tls =
                                        tonic::transport::ClientTlsConfig::new()
                                            .ca_certificate(tonic::transport::Certificate::from_pem(
                                                &new_certs.ca_cert_pem,
                                            ))
                                            .identity(tonic::transport::Identity::from_pem(
                                                &new_certs.client_cert_pem,
                                                &new_certs.client_key_pem,
                                            ))
                                            .domain_name("monopsony-server");
                                    tls_config = Some(renewed_tls);
                                    certs = new_certs;
                                    info!(
                                        expiry = %new_expiry,
                                        "Client certificate renewed; TLS config rotated, control connection kept alive"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        "Server re-issued the client certificate but atomic persist failed"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                "Certificate renewal failed — retrying on next heartbeat"
                            );
                        }
                    }
                }

                // Send heartbeat (Sprint 27 Part B: carries the windowed
                // update-failure snapshot — counts-only, no package identity)
                let health_snapshot =
                    update_health_for_task.snapshot(chrono::Utc::now().timestamp());
                match client
                    .heartbeat(&client_id_for_task, &health_snapshot)
                    .await
                {
                    Ok(_) => {
                        info!("Heartbeat sent");
                        // Poll server-pushed commands (Sprint 22 slice 22-E2b):
                        // the server yields a Command (e.g. shadow_assign) on
                        // the ControlStream when an assignment is pending.
                        // Failures are logged and swallowed so the heartbeat
                        // loop never degrades.
                        match client
                            .poll_pending_commands(&client_id_for_task, &health_snapshot)
                            .await
                        {
                            Ok(Some(command)) => {
                                handle_heartbeat_command(
                                    &mut client,
                                    &client_id_for_task,
                                    command,
                                    trust_config_for_task.accept_remote_assignment,
                                    &trust_config_for_task,
                                    mirror_key_for_task,
                                    &update_health_for_task,
                                )
                                .await;
                            }
                            Ok(None) => {}
                            Err(e) => warn!("command poll failed (ignored): {}", e),
                        }
                        // Drain spooled tamper events (Sprint 20 Phase 3):
                        // forward to the server as TamperAlerts; re-append on
                        // failure so alarms are never lost.
                        let events = pin_for_drain.drain_events();
                        if !events.is_empty() {
                            let alerts: Vec<_> =
                                events.iter().map(pin_verify::encode_tamper_alert).collect();
                            match client.report_tamper_alerts(&alerts).await {
                                Ok(()) => {
                                    info!("Reported {} tamper alert(s)", alerts.len())
                                }
                                Err(e) => {
                                    warn!("Tamper alert report failed, re-spooling: {}", e);
                                    for ev in &events {
                                        pin_for_drain.record_event(ev);
                                    }
                                }
                            }
                        }
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
                            Err(e) => {
                                warn!("Failed to list packages from {:?}: {}", backend.kind(), e)
                            }
                        }
                    }

                    match client
                        .report_inventory(
                            &client_id_for_task,
                            &all_packages,
                            true,
                            report_telemetry,
                        )
                        .await
                    {
                        Ok(_) => info!("Inventory reported ({} packages)", all_packages.len()),
                        Err(e) => {
                            warn!("Inventory report failed, reconnecting: {}", e);
                            break;
                        }
                    }
                    last_inventory = std::time::Instant::now();
                }

                // Shadow recipe catalog scan (Sprint 22 slice 22-C2), on
                // its own cadence. Runs inline but any failure is logged
                // and swallowed: the daemon must never degrade.
                if catalog_scan_guard.ready() {
                    let result: anyhow::Result<()> = async {
                        let known =
                            catalog_scan::load_last_hash(&cert_dir_for_task).unwrap_or_default();
                        let (catalog_hash, entries) = client
                            .list_shadow_catalog(&client_id_for_task, &known)
                            .await?;
                        if catalog_hash == known {
                            return Ok(());
                        }
                        let local = client.list_shadow_packages(&client_id_for_task).await?;
                        let mut findings = Vec::new();
                        for entry in &entries {
                            if local.iter().any(|p| p.name == entry.name) {
                                continue;
                            }
                            findings
                                .push(catalog_scan::scan_entry(&entry.name, &entry.recipe_toml));
                        }
                        let (accepted, message) = client
                            .report_shadow_scan(&client_id_for_task, &catalog_hash, findings)
                            .await?;
                        info!(
                            accepted = accepted,
                            message = %message,
                            catalog_hash = %catalog_hash,
                            "Shadow catalog scan reported"
                        );
                        catalog_scan::save_last_hash(&cert_dir_for_task, &catalog_hash);
                        Ok(())
                    }
                    .await;
                    if let Err(e) = result {
                        warn!("Shadow catalog scan failed (ignored): {}", e);
                    }
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

    // Derive HTTP proxy address from server address.
    // The --server flag gives us the gRPC port (50051), but the HTTP forward proxy
    // is on port 3128. We derive the proxy address from the server host.
    let proxy_server_addr = {
        let server_host = server_addr.split(':').next().unwrap_or("127.0.0.1");
        format!("{}:3128", server_host)
    };

    let proxy_config = proxy::ProxyConfig {
        server_addr: proxy_server_addr,
        timeout_ms,
        retry_deadline_ms,
        retry_backoff_initial_ms,
        // tunnel_max_lifetime_ms keeps its Default (1 h).
        ..Default::default()
    };

    info!("Local proxy listening on {}", listen_addr);
    info!("Heartbeat and inventory sync running in background");

    // Oracle floor store (Sprint 21 fleet loop): load floors.json from the
    // state dir. Missing/corrupt file → empty store, sync still runs and will
    // populate it (fail-open availability; integrity is enforced by the
    // signature check, never by the file). Store lives in the RwLock slot the
    // proxy reads — refreshes apply live, no proxy restart.
    let floor_state_path = dirs_ledger_parent(&cert_dir).join("floors.json");
    let floor_store: std::sync::Arc<std::sync::RwLock<crate::floor_guard::FloorStore>> =
        std::sync::Arc::new(std::sync::RwLock::new(
            match crate::floor_guard::load(&floor_state_path) {
                Ok(s) => s,
                Err(e) => {
                    info!(
                        "No floor store at {} ({}): starting empty — oracle sync will populate",
                        floor_state_path.display(),
                        e
                    );
                    crate::floor_guard::FloorStore::default()
                }
            },
        ));
    let oracle_url = client_config
        .as_ref()
        .and_then(|c| c.server.oracle_url.clone());
    if let Some(ref url) = oracle_url {
        let url = url.clone();
        let store = floor_store.clone();
        let pin_dir = cert_dir.clone();
        let floor_path = floor_state_path.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                ticker.tick().await;
                let pin_hex = match crate::oracle_update::load_pin(&pin_dir) {
                    Ok(p) => p,
                    Err(_) => {
                        debug!("oracle sync: no verify pin — skipping (feature off until pinned)");
                        continue;
                    }
                };
                let http = match reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("oracle sync: http client build failed: {}", e);
                        continue;
                    }
                };
                match crate::oracle_update::fetch_delta(&http, &url).await {
                    Ok(delta_json) => {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        match crate::oracle_update::verify_and_merge(&pin_hex, &delta_json, now) {
                            Ok(obs) => {
                                let mut guard = store.write().unwrap_or_else(|e| e.into_inner());
                                guard.merge_delta(&obs);
                                if let Err(e) = guard.save(&floor_path) {
                                    warn!("oracle sync: floor persist failed: {}", e);
                                }
                                info!("oracle sync: merged {} observations from delta", obs.len());
                            }
                            Err(e) => {
                                // Includes Expired — the freeze-alarm path:
                                // never silently accept a stale snapshot.
                                warn!("oracle sync: delta rejected: {:?}", e);
                            }
                        }
                    }
                    Err(e) => {
                        debug!("oracle sync: fetch failed: {:?}", e);
                    }
                }
            }
        });
    }

    // Ledger enforcement (Sprint 20 Phase 4): open the high-water ledger
    // when [ledger] enabled in config. Lives under the client state dir
    // (~/.local/state/monopsony/), NOT cert_dir — re-pairing must not wipe
    // the anti-rewind anchor. Open failure = fail-open availability (daemon
    // runs without the ledger; tamper events still work).
    let (ledger, ledger_policy, ledger_max_age) = match &client_config {
        Some(cfg) if cfg.ledger.enabled => {
            let state_dir = dirs_ledger_parent(&cert_dir);
            let path = state_dir.join("ledger.sqlite3");
            match crate::ledger::Ledger::open(&path) {
                Ok(l) => {
                    info!("Integrity ledger: enabled (policy={})", cfg.ledger.policy);
                    (
                        Some(std::sync::Arc::new(l)),
                        cfg.ledger.policy.clone(),
                        cfg.ledger.max_index_age_secs,
                    )
                }
                Err(e) => {
                    tracing::error!(
                        "Ledger open failed ({}): {} — running without ledger",
                        path.display(),
                        e
                    );
                    (None, "warn".to_string(), 0)
                }
            }
        }
        _ => (None, "warn".to_string(), 0),
    };

    // HIT/MISS/STALE accounting (Sprint 26 Phase 4 slice 5): persist under
    // the client state dir so `status --quiet` (a separate process) reads
    // the daemon's snapshot. Flush on an interval; a crash loses at most
    // one interval. Counting failure never blocks forwarding (fail-open).
    let hit_counter_path = hit_counter_state_path(&cert_dir);
    if let Some(parent) = hit_counter_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let hit_counter = hit_counter::HitCounter::load(hit_counter_path.clone());
    {
        let counter = hit_counter.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                counter.persist();
            }
        });
    }

    proxy::run_proxy(
        listen_addr,
        proxy_config,
        pin_state.clone(),
        ledger,
        ledger_policy,
        ledger_max_age,
        Some(floor_store),
        Some(hit_counter),
    )
    .await?;

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
    info!(
        "Discovering Monopsony servers (timeout: {}s)...",
        timeout_secs
    );
    let disc = discovery::ClientDiscovery::new("monopsony");

    let tracker_urls: Vec<String> = tracker_url
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let peer_urls: Vec<String> = peer_url
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect()
        })
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
        let result = disc
            .discover_v2(
                Duration::from_secs(timeout_secs),
                &tracker_urls,
                &peer_urls,
                Some(distro),
            )
            .await?;

        if result.total_results() == 0 {
            info!("No servers found from any source.");
        } else {
            if !result.tracker_mirrors.is_empty() {
                info!("Tracker mirrors ({}):", result.tracker_mirrors.len());
                for m in &result.tracker_mirrors {
                    info!(
                        "  {} {} (score={:.1}, transport={})",
                        m.mirror_id, m.endpoint, m.score, m.transport
                    );
                }
            }
            if !result.peer_mirrors.is_empty() {
                info!("Peer mirrors ({}):", result.peer_mirrors.len());
                for m in &result.peer_mirrors {
                    info!(
                        "  {} {} (score={:.1}, transport={})",
                        m.mirror_id, m.endpoint, m.score, m.transport
                    );
                }
            }
            if !result.mdns_services.is_empty() {
                info!("mDNS services ({}):", result.mdns_services.len());
                for srv in &result.mdns_services {
                    info!(
                        "  {} (HTTP port: {}, gRPC port: {:?})",
                        srv.address, srv.http_port, srv.grpc_port
                    );
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

        // Certificate expiry + renewal window (P1 cert lifecycle): the daemon
        // auto-re-issues inside the 30-day window without human re-pairing.
        match trust::client_cert_expiry(&certs.client_cert_pem) {
            Some(expires) => {
                let days = (expires - chrono::Utc::now()).num_days();
                info!("  Certificate expires: {} ({} days)", expires.to_rfc3339(), days);
                if days <= grpc_client::CERT_RENEWAL_WINDOW_DAYS as i64 {
                    info!(
                        "  Renewal window: ACTIVE — automatic mTLS re-issue inside 30 days"
                    );
                }
            }
            None => info!("  Certificate expires: unparseable"),
        }
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

/// Machine-readable status (Sprint 26 Phase 4, B4 slices 3/5). stdout carries
/// ONLY `key: value` lines — the tracing subscriber is not installed for this
/// verb, so no log prefixes ever interleave. Parse contract (case-insensitive
/// keys, matched by the menu bar app):
///   Server: <host[:port] | not paired | unavailable>
///   Connected clients: <N>
///   Cache HIT ratio: <NN.N%>
/// Exit code 0 always — absence of data is expressed by omitting the line.
fn run_status_quiet(cert_dir: &Path) {
    // Server identity: same fallback chain as the daemon (ws_url, then
    // grpc_addr, from cert_dir/server.json written at pairing time).
    let server_json_path = cert_dir.join("server.json");
    if let Ok(content) = std::fs::read_to_string(&server_json_path) {
        if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
            let addr = info
                .get("ws_url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| {
                    info.get("grpc_addr")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                });
            if let Some(addr) = addr {
                println!("Server: {addr}");
            }
        }
    }

    // Certificate expiry + renewal window (P1 cert lifecycle), read from the
    // paired cert on disk — a separate process from the daemon, so disk is
    // the source of truth (rotated atomically by the renewal path).
    if let Some(certs) = grpc_client::ClientCerts::load(cert_dir) {
        match trust::client_cert_expiry(&certs.client_cert_pem) {
            Some(expires) => {
                let days = (expires - chrono::Utc::now()).num_days();
                println!("Cert Expiry: {}", expires.to_rfc3339());
                let renewal = if days <= grpc_client::CERT_RENEWAL_WINDOW_DAYS as i64 {
                    "active"
                } else {
                    "not-due"
                };
                println!("Cert Renewal: {renewal} ({days} days remaining)");
            }
            None => {
                println!("Cert Expiry: unknown");
                println!("Cert Renewal: unknown");
            }
        }
    }

    // Cache HIT ratio from the daemon's persisted X-Cache counters.
    // daemon (not this process) owns the live counters; this reads its
    // last flushed snapshot, so a never-run daemon simply omits the line.
    let counter_path = hit_counter_state_path(cert_dir);
    if let Ok(content) = std::fs::read_to_string(&counter_path) {
        if let Ok(counters) = serde_json::from_str::<hit_counter::PersistedSnapshot>(&content) {
            let total = counters.hits + counters.misses + counters.stale;
            if total > 0 {
                let served = counters.hits + counters.stale;
                let ratio = (served as f64 / total as f64) * 100.0;
                println!("Cache HIT ratio: {:.1}%", ratio);
            }
        }
    }
}

/// Persist path for the daemon's X-Cache counters: the client state dir,
/// matching the tamper-event spool / ledger convention.
fn hit_counter_state_path(_cert_dir: &Path) -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local/state/monopsony/hit-counts.json"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/monopsony/hit-counts.json"))
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

    println!(
        "Configuration complete: {} actions succeeded, {} failed",
        configured, failed
    );
    if failed > 0 {
        anyhow::bail!("{} configuration actions failed", failed);
    }
    Ok(())
}

async fn run_pair(
    server: &str,
    code: Option<&str>,
    pin: bool,
    enrollment_key: Option<&str>,
    cert_dir: &Path,
) -> anyhow::Result<()> {
    // Check if already paired
    if grpc_client::ClientCerts::load(cert_dir).is_some() {
        warn!(
            "Client already has certificates. Delete {} to re-pair.",
            cert_dir.display()
        );
        return Ok(());
    }

    let hostname = hostname::get()
        .map_err(|e| anyhow::anyhow!("Failed to get system hostname: {}", e))?
        .to_string_lossy()
        .to_string();

    let backends = monopsony_common::package_manager::get_available_backends();
    let managers: Vec<String> = backends
        .iter()
        .map(|b| b.kind().as_str().to_string())
        .collect();

    info!("Pairing with server: {}", server);

    // Legacy code-based pairing (pairing_mode = legacy-code)
    if let Some(code) = code {
        info!("Using legacy code mode");

    // Route to WebSocket pairing when the server address is a ws:// or wss:// URL.
    // This is used when the server is behind Cloudflare Tunnels or a reverse proxy.
    let (client_id, certs, grpc_addr, ws_url, mirror_verify_key) = if server.starts_with("wss://")
        || server.starts_with("ws://")
    {
        info!("Using WebSocket tunnel for pairing");
        grpc_client::MonopsonyGrpcClient::pair_ws(
            server,
            code,
            &hostname,
            std::env::consts::OS,
            std::env::consts::ARCH,
            &managers,
        ).await.map_err(|e| anyhow::anyhow!(
            "Pairing with server {} failed: {}. Is the server running and the pairing code correct?",
            server, e
        ))?
    } else {
        grpc_client::MonopsonyGrpcClient::pair(
            server,
            code,
            &hostname,
            std::env::consts::OS,
            std::env::consts::ARCH,
            &managers,
        ).await.map_err(|e| anyhow::anyhow!(
            "Pairing with server {} failed: {}. Is the server running and the pairing code correct?",
            server, e
        ))?
    };

    certs.save_atomic(cert_dir)?;
    std::fs::write(cert_dir.join("client_id"), &client_id)?;
    store_mirror_verify_key(cert_dir, &mirror_verify_key);

    // Surface the new cert's expiry so the operator sees the 90-day clock
    // and the 30-day renewal window at pair time.
    if let Some(expires) = trust::client_cert_expiry(&certs.client_cert_pem) {
        info!(
            expiry = %expires,
            "Paired client certificate valid for 90 days; daemon auto-renews inside the 30-day window"
        );
        println!("  Certificate expires: {} (auto-renews inside 30 days)", expires.to_rfc3339());
    }

    // Persist server connection info for future daemon connections.
    // Include ws_url when the server provided one — clients will prefer it
    // over grpc_addr for daemon connections (WS tunnel through CF/proxy).
    let server_info = serde_json::json!({
        "grpc_addr": grpc_addr,
        "ws_url": ws_url,
        "paired_addr": server,
        "paired_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        cert_dir.join("server.json"),
        serde_json::to_string_pretty(&server_info)?,
    )?;

    info!("Pairing complete!");
    info!("  Client ID: {}", client_id);
    info!("  Certificates saved to: {}", cert_dir.display());
    info!(
        "  Server address saved to: {}/server.json",
        cert_dir.display()
    );
    println!("Pairing complete! Client ID: {}", client_id);
        return Ok(());
    }

    // v2 Sunshine pairing (pairing_mode = pin)
    if pin {
        return run_pair_v2(server, &hostname, &managers, cert_dir).await;
    }

    // Enrollment-key pairing (P5): pre-approved, non-interactive.
    if let Some(enroll_key) = enrollment_key {
        return run_pair_enroll(server, enroll_key, &hostname, &managers, cert_dir).await;
    }

    anyhow::bail!(
        "Pairing mode not specified. Use --code <6-char-code> for legacy mode, \
         --pin for v2 Sunshine pairing (admin approval + client PIN), or \
         --enrollment-key <mopk-...> for pre-approved fleet enrollment."
    );
}

/// Enrollment-key pairing (P5): non-interactive pre-approved pairing.
/// Generates an ephemeral keypair, signs `monopsony-enroll-v1` as the
/// possession proof, posts to /api/v1/pair/enroll, saves certs + TOFU pin.
async fn run_pair_enroll(
    server: &str,
    enrollment_key: &str,
    hostname: &str,
    managers: &[String],
    cert_dir: &Path,
) -> anyhow::Result<()> {
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    let base_url = if server.starts_with("http://") || server.starts_with("https://") {
        server.to_string()
    } else {
        format!("http://{}", server)
    };

    info!(
        "Starting enrollment-key pairing with server: {}",
        server
    );

    // 1. Generate client ephemeral Ed25519 keypair
    let signing_key = SigningKey::generate(&mut OsRng);
    let ephemeral_pubkey_bytes = signing_key.verifying_key().to_bytes();

    // 2. Possession proof: Ed25519 signature over the fixed domain string
    let signature = signing_key.sign(b"monopsony-enroll-v1");
    let signature_bytes = signature.to_bytes();

    // 3. Enroll (single round-trip — no PIN, no polling)
    let (client_id, certs) = grpc_client::MonopsonyGrpcClient::enroll_pair_v2(
        &base_url,
        enrollment_key,
        &ephemeral_pubkey_bytes,
        &signature_bytes,
        hostname,
        std::env::consts::OS,
        std::env::consts::ARCH,
        managers,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Enrollment failed: {}", e))?;

    // 4. TOFU CA fingerprint pinning
    let ca_fingerprint =
        crate::trust::ca_fingerprint(&certs.ca_cert_pem).context("failed to compute CA fingerprint")?;
    crate::trust::check_ca_fingerprint(cert_dir, &ca_fingerprint)
        .map_err(|e| anyhow::anyhow!("CA fingerprint check failed: {}", e))?;
    crate::trust::store_ca_fingerprint(cert_dir, &ca_fingerprint)?;

    // 5. Save certificates
    save_pair_result(cert_dir, &client_id, &certs, "", None, &[])?;

    info!(
        client_id = %client_id,
        ca_fingerprint = %ca_fingerprint,
        "Enrollment-key pairing complete — CA fingerprint TOFU-pinned"
    );
    println!("Enrollment complete! Client ID: {}", client_id);
    println!("CA fingerprint (TOFU-pinned): {}", ca_fingerprint);
    Ok(())
}

fn save_pair_result(
    cert_dir: &Path,
    client_id: &str,
    certs: &grpc_client::ClientCerts,
    grpc_addr: &str,
    ws_url: Option<String>,
    mirror_verify_key: &[u8],
) -> anyhow::Result<()> {
    certs.save_atomic(cert_dir)?;
    std::fs::write(cert_dir.join("client_id"), client_id)?;

    if !mirror_verify_key.is_empty() {
        crate::store_mirror_verify_key(cert_dir, mirror_verify_key);
    }

    // Surface the new cert's expiry so the operator sees the 90-day clock
    // and the 30-day renewal window at pair time.
    if let Some(expires) = crate::trust::client_cert_expiry(&certs.client_cert_pem) {
        info!(
            expiry = %expires,
            "Paired client certificate valid for 90 days; daemon auto-renews inside the 30-day window"
        );
        println!(
            "  Certificate expires: {} (auto-renews inside 30 days)",
            expires.to_rfc3339()
        );
    }

    let server_info = serde_json::json!({
        "grpc_addr": grpc_addr,
        "ws_url": ws_url,
        "paired_addr": "",
        "paired_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        cert_dir.join("server.json"),
        serde_json::to_string_pretty(&server_info)?,
    )?;

    Ok(())
}

/// v2 Sunshine pairing flow: generate PIN + ephemeral keypair, display PIN,
/// submit request over HTTP, poll for admin approval, collect cert with
/// proof of possession. TOFU-pins the server CA fingerprint on first collect.
async fn run_pair_v2(
    server: &str,
    hostname: &str,
    managers: &[String],
    cert_dir: &Path,
) -> anyhow::Result<()> {
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    let base_url = if server.starts_with("http://") || server.starts_with("https://") {
        server.to_string()
    } else {
        format!("http://{}", server)
    };

    info!(
        "Starting v2 Sunshine pairing with server: {} (pairing_mode=pin)",
        server
    );

    // 1. Generate client ephemeral Ed25519 keypair
    let signing_key = SigningKey::generate(&mut OsRng);
    let ephemeral_pubkey_bytes = signing_key.verifying_key().to_bytes();
    let ephemeral_pubkey_hex = hex::encode(ephemeral_pubkey_bytes);

    // 2. Generate 6-char PIN (charset ABCDEFGHJKLMNPQRSTUVWXYZ23456789)
    let charset = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut pin_bytes = [0u8; 6];
    OsRng.fill(&mut pin_bytes);
    let pin: String = pin_bytes
        .iter()
        .map(|&b| charset[b as usize % charset.len()] as char)
        .collect();

    // 3. Compute salted PIN hash: SHA-256(pin_salt + pin)
    // pin_salt is a hex-encoded string; hash the hex string bytes to match
    // the server's verification in PendingPairStore::attempt_pin.
    let mut pin_salt_bytes = [0u8; 16];
    OsRng.fill(&mut pin_salt_bytes);
    let pin_salt = hex::encode(pin_salt_bytes);
    let mut hasher = sha2::Sha256::new();
    hasher.update(pin_salt.as_bytes());
    hasher.update(pin.as_bytes());
    let pin_hash = hex::encode(hasher.finalize());

    // Display PIN to the user (out-of-band channel for admin approval)
    println!();
    println!("============================================================");
    println!("  MONOPOSSONY PAIRING — v2 Sunshine mode");
    println!("  Server: {}", server);
    println!("  Your pairing PIN: {}", pin);
    println!("  Show this PIN to the server administrator for approval.");
    println!("============================================================");
    println!();

    // 4. Submit pairing request
    info!("Submitting v2 pairing request");
    let (request_id, _expires_at) = grpc_client::MonopsonyGrpcClient::submit_pair_request_v2(
        &base_url,
        &ephemeral_pubkey_hex,
        &pin_hash,
        &pin_salt,
        hostname,
        std::env::consts::OS,
        std::env::consts::ARCH,
        managers,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to submit pairing request: {}", e))?;

    info!(
        request_id = %request_id,
        "Pairing request submitted, waiting for admin approval"
    );
    println!(
        "Pairing request submitted (request_id={}). Waiting for admin approval...",
        request_id
    );

    // 5. Poll for approval (up to 15 minutes, matching the server's pending TTL)
    let poll_deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    loop {
        if std::time::Instant::now() > poll_deadline {
            anyhow::bail!("Pairing request expired before admin approval");
        }

        let status = grpc_client::MonopsonyGrpcClient::poll_pair_request_status(&base_url, &request_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to poll pairing status: {}", e))?;

        match status.as_str() {
            "approved" => break,
            "pending" => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            "expired" | "invalidated" => {
                anyhow::bail!("Pairing request {} (status={})", request_id, status);
            }
            _ => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        }
    }

    println!("Admin approved the request. Collecting certificate...");

    // 6. Collect: sign request_id with ephemeral private key
    let signature = signing_key.sign(request_id.as_bytes());
    let signature_bytes = signature.to_bytes();

    let (client_id, certs, mirror_verify_key) = grpc_client::MonopsonyGrpcClient::collect_pair_cert_v2(
        &base_url,
        &request_id,
        &ephemeral_pubkey_bytes,
        &signature_bytes,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to collect certificate: {}", e))?;

    // Persist the server's Ed25519 recipe-signing key so shadow package
    // updates verify against a provisioned trust anchor (v2 parity with the
    // legacy register path — without this, updates abort trust-UNVERIFIED).
    if let Some(k) = &mirror_verify_key {
        store_mirror_verify_key(cert_dir, k);
    }

    // 7. TOFU CA fingerprint pinning
    let ca_fingerprint =
        crate::trust::ca_fingerprint(&certs.ca_cert_pem).context("failed to compute CA fingerprint")?;
    crate::trust::check_ca_fingerprint(cert_dir, &ca_fingerprint)
        .map_err(|e| anyhow::anyhow!("CA fingerprint check failed: {}", e))?;
    crate::trust::store_ca_fingerprint(cert_dir, &ca_fingerprint)?;

    // 8. Save certificates
    save_pair_result(cert_dir, &client_id, &certs, "", None, &[])?;

    info!(
        client_id = %client_id,
        ca_fingerprint = %ca_fingerprint,
        "Pairing v2 complete — CA fingerprint TOFU-pinned"
    );

    // Persist server connection info (mirrors legacy pairing) so the daemon
    // can find the server without --server or mDNS. Derive the gRPC address
    // from the paired host — same host, gRPC port 50051; the pairing port
    // (:3130) is the dashboard API, not gRPC.
    let host = server
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .unwrap_or(server);
    let server_info = serde_json::json!({
        "grpc_addr": format!("{}:50051", host),
        "paired_addr": server,
        "paired_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        cert_dir.join("server.json"),
        serde_json::to_string_pretty(&server_info)?,
    )?;

    println!("Pairing complete! Client ID: {}", client_id);
    println!("CA fingerprint (TOFU-pinned): {}", ca_fingerprint);
    Ok(())
}

/// Interactive setup (P6): detect installed package managers, apply the
/// ProxyConfigActions for each (reusing proxy_toggle), and print init
/// placement guidance. Idempotent — re-running re-applies the same config.
async fn run_setup_interactive(
    server: Option<&str>,
    assume_all: bool,
    cert_dir: &Path,
) -> anyhow::Result<()> {
    let server_addr = if let Some(s) = server {
        s.to_string()
    } else {
        resolve_server_addr(cert_dir)
            .await
            .unwrap_or_else(|_| "127.0.0.1:3128".to_string())
    };

    // PM discovery: only offer backends that are actually installed.
    let backends: Vec<String> = monopsony_common::package_manager::get_available_backends()
        .iter()
        .filter(|b| b.is_available())
        .map(|b| b.kind().as_str().to_string())
        .collect();

    println!();
    println!("Monopsony client setup");
    println!("  Server:   {}", server_addr);
    println!("  Detected package managers: {}", if backends.is_empty() { "(none)".into() } else { backends.join(", ") });
    println!();

    if !assume_all {
        print!(
            "Configure all detected package managers (plus Docker/shadow hooks) \
             for proxying through Monopsony? [y/N]: "
        );
        use std::io::Write as _;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Setup cancelled — no changes made.");
            return Ok(());
        }
    }

    let results = crate::proxy_toggle::proxy_on(
        crate::proxy_toggle::ToggleTarget::All,
        &server_addr,
    )
    .await;

    let mut succeeded = 0;
    let mut failed = 0;
    for r in &results {
        if r.success {
            println!("✓ {}: {}", r.subsystem, r.message);
            succeeded += 1;
        } else {
            println!("✗ {}: {}", r.subsystem, r.message);
            failed += 1;
        }
    }
    println!("\n{} configured, {} failed", succeeded, failed);

    // Init placement guidance — the installer (dist/install.sh) owns unit
    // files; here we only point the operator at the next step.
    println!();
    println!("Next steps:");
    println!("  1. Enable the daemon: systemctl enable --now monopsony-client");
    println!("     (unit file placed by dist/install.sh; see docs/container-deploy.md");
    println!("      for container deployments)");
    println!("  2. Test one package: e.g. `pacman -Syu` (check the server dashboard");
    println!("     for cache hits)");
    println!("  Re-run this command any time — setup is idempotent.");

    if failed > 0 {
        anyhow::bail!("{} setup actions failed", failed);
    }
    Ok(())
}

/// Uninstall (P6): remove proxy configuration from every detected package
/// manager and subsystem. Daemon + certs are left in place — the operator
/// stops/disables the service and deletes the cert dir separately.
async fn run_uninstall(cert_dir: &Path) -> anyhow::Result<()> {
    println!("Removing Monopsony proxy configuration from all detected subsystems...");

    let results =
        crate::proxy_toggle::proxy_off(crate::proxy_toggle::ToggleTarget::All).await;

    let mut succeeded = 0;
    let mut failed = 0;
    for r in &results {
        if r.success {
            println!("✓ {}: {}", r.subsystem, r.message);
            succeeded += 1;
        } else {
            println!("✗ {}: {}", r.subsystem, r.message);
            failed += 1;
        }
    }
    println!("\n{} removed, {} failed", succeeded, failed);

    println!();
    println!("Proxy configuration removed. To fully uninstall:");
    println!("  1. systemctl disable --now monopsony-client");
    println!("  2. Remove the package (pacman -R monopsony-client / apt remove, ...)");
    println!("  3. Optionally delete {}", cert_dir.display());
    println!("  4. On the server: revoke this client's certificate from the dashboard");

    if failed > 0 {
        anyhow::bail!("{} uninstall actions failed", failed);
    }
    Ok(())
}

#[cfg(unix)]
async fn run_mirror(server: &str, distro: &str, mirror_url: Option<&str>) -> anyhow::Result<()> {
    {
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
                println!(
                    "NixOS: set NIX_CONFIG=\"substituters = {} https://cache.nixos.org\"",
                    mirror_base
                );
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
                        let content = format!(
                            "# Monopsony mirror for Arch Linux\nServer = {}$repo/os/$arch\n",
                            mirror_base
                        );
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
                                Ok(o) if o.status.success() => {
                                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                                }
                                _ => "noble".to_string(),
                            }
                        } else {
                            // Debian
                            let output = std::process::Command::new("lsb_release")
                                .args(["-c", "-s"])
                                .output();
                            match output {
                                Ok(o) if o.status.success() => {
                                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                                }
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
                } // end match kind (#[cfg(unix)])

                println!("Mirror configuration complete for {}.", distro);
                println!("Note: You may need to run the package manager update command (e.g., apt update, pacman -Sy).");
            }
            None => {
                info!(
                    "No package manager backend found for {} (kind: {:?})",
                    distro, kind
                );
                println!(
                    "No compatible package manager detected for {} on this system.",
                    distro
                );
                println!(
                    "Manual configuration: point your package manager at {}",
                    mirror_base
                );
            }
        } // end match backend
    } // end #[cfg(unix)] block

    Ok(())
}

#[cfg(windows)]
async fn run_mirror(server: &str, distro: &str, mirror_url: Option<&str>) -> anyhow::Result<()> {
    let mirror_base = mirror_url
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("http://{}/mirror/{}/", server, distro));

    info!("Mirror subcommand not supported on Windows — set HTTP_PROXY manually.");
    println!("Windows: configure your package manager to use the Monopsony proxy manually:");
    println!();
    println!("  winget/choco/scoop: Set system proxy via netsh winhttp");
    println!("    netsh winhttp set proxy {}", mirror_base);
    println!();
    println!("  Or use environment variable for current session:");
    println!("    set HTTP_PROXY={}", mirror_base);
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
    println!(
        "Or connect to the server at {} and query the metrics endpoint:",
        server
    );
    println!("  curl http://{}/metrics | grep monopsony_cache", server);

    Ok(())
}

async fn run_shadow_package(
    cmd: shadow_cli::PackageCommand,
    cert_dir: &Path,
) -> anyhow::Result<()> {
    use shadow_cli::PackageCommand::*;

    // Helper: resolve server addr + connect (Sprint 22 slice 22-A).
    async fn connect(
        cert_dir: &Path,
    ) -> anyhow::Result<(grpc_client::MonopsonyGrpcClient, String)> {
        let server_addr = resolve_server_addr(cert_dir).await?;
        connect_shadow_client(Some(&server_addr), cert_dir).await
    }

    // Helper: find the package id for a name-or-id selector.
    async fn resolve_package_id(
        client: &mut grpc_client::MonopsonyGrpcClient,
        client_id: &str,
        name_or_id: &str,
    ) -> anyhow::Result<String> {
        let packages = client.list_shadow_packages(client_id).await?;
        if let Some(p) = packages.iter().find(|p| p.id == name_or_id) {
            return Ok(p.id.clone());
        }
        let by_name: Vec<_> = packages.iter().filter(|p| p.name == name_or_id).collect();
        match by_name.len() {
            1 => Ok(by_name[0].id.clone()),
            0 => anyhow::bail!(
                "No shadow package found matching '{}' (id or name)",
                name_or_id
            ),
            _ => anyhow::bail!(
                "Multiple shadow packages named '{}': {} — use the package id instead",
                name_or_id,
                by_name
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    // Helper: fetch the signed recipe for a package and verify it against the
    // client trust policy. Returns the recipe TOML on success. Without a
    // verifying key configured we still check policy shape via
    // trust::verify_recipe_trust (which bails when the policy needs a key).
    async fn load_verified_recipe(
        client: &mut grpc_client::MonopsonyGrpcClient,
        client_id: &str,
        name: &str,
    ) -> anyhow::Result<String> {
        let (recipe_toml, key_id, signature_b64, signed_at) =
            client.get_shadow_recipe(client_id, name).await?;
        let recipe: monopsony_common::shadow_types::ShadowRecipe =
            toml::from_str(&recipe_toml).context("server recipe failed to parse as TOML")?;
        let signed = monopsony_common::shadow_types::SignedRecipe {
            recipe_toml: recipe_toml.clone(),
            recipe,
            operator_signature: monopsony_common::shadow_types::RecipeSignature {
                key_id,
                signature: signature_b64,
                signed_at: chrono::DateTime::<chrono::Utc>::from_timestamp(signed_at, 0)
                    .unwrap_or_default(),
            },
            author_signature: None,
        };

        // Load trust config (same source the Update arm documents).
        // Only the [shadow_trust] section is needed here; the full
        // ClientConfig requires [client]/[server]/[tls]/[proxy]/[metrics]/[logging]
        // which a minimal client.toml may not have. Parse just shadow_trust
        // so a trust-only config file works.
        let config_path = std::env::var("MONOPSONY_CLIENT_CONFIG")
            .unwrap_or_else(|_| "/etc/monopsony/client.toml".to_string());
        let trust_config = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|content| toml::from_str::<toml::Value>(&content).ok())
            .and_then(|value| value.get("shadow_trust").cloned())
            .map(|table| {
                let policy = table
                    .get("policy")
                    .and_then(|v| v.as_str())
                    .unwrap_or("trusted_server_only")
                    .to_string();
                let operator_verifying_key_path = table
                    .get("operator_verifying_key_path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                monopsony_common::config::ShadowPackageTrustConfig {
                    policy,
                    trusted_author_keys: vec![],
                    trusted_operator_keys: vec![],
                    operator_verifying_key_path,
                    accept_remote_assignment: false,
                }
            })
            .unwrap_or_default();

        // Without a locally configured operator verifying key we pass None;
        // verify_recipe_trust enforces the policy (e.g. trusted_authors works
        // without the raw key, trusted_server_only fails closed without one).
        let operator_vk = load_operator_verifying_key(&trust_config);
        trust::verify_recipe_trust(&signed, &trust_config, operator_vk.as_ref()).context(
            format!(
                "trust verification failed for recipe of '{}' (key_id: {})",
                name, signed.operator_signature.key_id
            ),
        )?;
        Ok(recipe_toml)
    }

    match cmd {
        Register {
            path,
            source,
            name,
            recipe,
            method,
            branch,
            requires,
        } => {
            let pkg_name = name.clone().unwrap_or_else(|| {
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

            let source_opt: Option<&str> = Some(&source);

            // Recipe TOML + version detection:
            //   --recipe path: operator-authored recipes bypass detection
            //   entirely (unchanged); the recipe's own [version]
            //   detect_command detects the installed version.
            //   No --recipe: run `detect::detect(path)`, render the draft
            //   TOML via `render_draft_toml`, and take `detected_version`
            //   from `report.version`. Blank-version registers die here; the
            //   "registering blank" branch survives only as the --recipe
            //   fallback when its probe genuinely fails.
            let (mut recipe_toml, detected_version, report_opt) =
                if let Some(recipe_path) = &recipe {
                    let recipe_toml = std::fs::read_to_string(recipe_path)
                        .with_context(|| format!("reading recipe file {}", recipe_path))?;
                    let detected_version =
                        shadow_executor::detect_current_version(&recipe_toml).await;
                    (recipe_toml, detected_version, None)
                } else {
                    let report = detect::detect(Path::new(&path), name.as_deref()).await?;
                    let draft = detect::render_draft_toml(&report, &pkg_name, source_opt);
                    if source_opt.is_none() {
                        // render_draft_toml emits source_url = "REPLACE_ME" plus
                        // the T0.4 header; refuse to register without a source.
                        anyhow::bail!(
                            "registration requires --source while source_url is REPLACE_ME for '{}'",
                            pkg_name
                        );
                    }
                    let detected_version = match report.version {
                        Some(ref v) => Some(v.clone()),
                        None => {
                            anyhow::bail!(
                                "refusing to register '{}' with blank detected version (use --recipe)",
                                pkg_name
                            )
                        }
                    };
                    (draft, detected_version, Some(report))
                };

            // Sprint 31 I2: the --recipe path has no detect report, so its row
            // metadata comes from the recipe TOML itself — [install] paths and
            // service via the lenient extractors, sha256 from hashing the file
            // at the install path (empty on unreadable/non-file). The detect
            // path keeps its report-derived values below.
            let (install_paths, recipe_service, recipe_config_paths, recipe_sha256) =
                if report_opt.is_none() {
                    let (recipe_install_paths, recipe_service) =
                        shadow_cli::extract_install_fields(&recipe_toml);
                    let install_paths = if recipe_install_paths.is_empty() {
                        vec![path.clone()]
                    } else {
                        recipe_install_paths
                    };
                    let recipe_sha256 = std::fs::read(&path)
                        .map(|bytes| detect::sha256_hex(&bytes))
                        .unwrap_or_default();
                    (
                        install_paths,
                        recipe_service,
                        shadow_cli::extract_config_paths(&recipe_toml),
                        recipe_sha256,
                    )
                } else {
                    (vec![path.clone()], String::new(), Vec::new(), String::new())
                };
            match &detected_version {
                Some(v) => println!("  Detected version: {}", v),
                None => println!(
                    "  Version: not detected (no runnable [version] detect_command; registering blank)"
                ),
            }
            println!("Connecting to server to register...");
            let (mut client, client_id) = connect(cert_dir).await?;

            // ── T4.3 / T4.5: similarity scoring + adopt-from-peer offer ──
            // (no-recipe path only; --recipe path registers as authored)
            if let Some(report) = &report_opt {
                let local_target_dir = report
                    .resolved_path
                    .as_deref()
                    .and_then(|p| Path::new(p).parent())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "/usr/local/bin".to_string());

                let resp = client
                    .query_shadow_registry(
                        report.sha256.as_deref().unwrap_or(""),
                        &pkg_name,
                        report
                            .service
                            .as_ref()
                            .map(|s| s.unit_name.as_str())
                            .unwrap_or(""),
                        &local_target_dir,
                        &source,
                    )
                    .await?;

                let scoring = shadow_scorer::score_report(
                    report,
                    &resp.matches,
                    &pkg_name,
                    &source,
                    &local_target_dir,
                );

                match scoring.action {
                    shadow_scorer::ScoringAction::DraftAnnotation => {
                        let peer = scoring
                            .best_match
                            .as_ref()
                            .map(|m| m.recipe_name.as_str())
                            .unwrap_or("?");
                        let comment = format!(
                            "# SIMILARITY: score {} — near match to peer recipe '{}' \
                             (register anyway to proceed)\n",
                            scoring.score, peer
                        );
                        recipe_toml = format!("{}{}", comment, recipe_toml);
                    }
                    shadow_scorer::ScoringAction::InteractiveChoice => {
                        let peer = scoring
                            .best_match
                            .as_ref()
                            .map(|m| m.recipe_name.as_str())
                            .unwrap_or("?");
                        println!(
                            "SIMILARITY MATCH (score {}) with peer recipe '{}'",
                            scoring.score, peer
                        );
                        println!("  [r]egister this package as new");
                        println!("  [a]dopt-from-peer (deploy the peer's recipe locally)");
                        println!("  [q]uit / abort registration");
                        let choice = read_line("Choice [r/a/q]: ")?;
                        match choice.as_str() {
                            "a" | "adopt" => {
                                // T4.5: adopt-from-peer (private/tailnet servers).
                                let best = scoring
                                    .best_match
                                    .context("interactive choice with no best match")?;
                                let (peer_toml, key_id, signature, signed_at) =
                                    client.get_shadow_recipe(&client_id, &best.recipe_name).await?;
                                let signed = shadow_scorer::signed_peer_recipe(
                                    peer_toml,
                                    key_id,
                                    signature,
                                    signed_at,
                                )?;
                                let trust_config = shadow_scorer::load_trust_config();
                                let operator_vk =
                                    load_operator_verifying_key(&trust_config);
                                trust::verify_recipe_trust(
                                    &signed,
                                    &trust_config,
                                    operator_vk.as_ref(),
                                )
                                .context(format!(
                                    "trust verification failed for peer recipe '{}'",
                                    best.recipe_name
                                ))?;
                                let local_config_paths: Vec<String> = report
                                    .config_findings
                                    .iter()
                                    .filter(|f| f.exists)
                                    .map(|f| f.path.clone())
                                    .collect();
                                let mutated = shadow_scorer::mutate_recipe_for_local(
                                    &signed.recipe_toml,
                                    &local_target_dir,
                                    &local_config_paths,
                                    report
                                        .service
                                        .as_ref()
                                        .map(|s| s.unit_name.as_str()),
                                )?;
                                let info = client
                                    .register_shadow_package(
                                        &client_id,
                                        &pkg_name,
                                        detected_version
                                            .as_deref()
                                            .unwrap_or(""),
                                        std::env::consts::ARCH,
                                        std::env::consts::OS,
                                        "", // libc: unknown from CLI
                                        shadow_source_as_str(&source_type),
                                        &source,
                                        &mutated,
                                        &install_paths,
                                        &local_config_paths,
                                        report
                                            .service
                                            .as_ref()
                                            .map(|s| s.unit_name.as_str())
                                            .unwrap_or(""),
                                        report.sha256.as_deref().unwrap_or(""),
                                        false, // mask_from_pm: opt-in only
                                    )
                                    .await?;
                                println!("✓ Adopted from peer and registered:");
                                println!("  ID:      {}", info.id);
                                println!("  Name:    {}", info.name);
                                println!("  Version: {}", info.version);
                                return Ok(());
                            }
                            "q" | "quit" => {
                                anyhow::bail!("registration aborted by operator");
                            }
                            _ => {
                                // fall through to normal register below
                            }
                        }
                    }
                    shadow_scorer::ScoringAction::Silent => {
                        // register silently below
                    }
                }
            }

            // Row metadata for the server (T2.1/T2.2): forward what the
            // detect pipeline found — existing config findings, matched
            // service unit, and the binary sha256. The --recipe path has
            // no report, so it falls back to the recipe-derived values
            // computed above (Sprint 31 I2).
            let reg_config_paths: Vec<String> = report_opt
                .as_ref()
                .map(|r| {
                    r.config_findings
                        .iter()
                        .filter(|f| f.exists)
                        .map(|f| f.path.clone())
                        .collect()
                })
                .unwrap_or(recipe_config_paths);
            let reg_service_name = report_opt
                .as_ref()
                .and_then(|r| r.service.as_ref())
                .map(|s| s.unit_name.clone())
                .unwrap_or(recipe_service);
            let reg_sha256 = report_opt
                .as_ref()
                .and_then(|r| r.sha256.clone())
                .unwrap_or(recipe_sha256);

            let info = client
                .register_shadow_package(
                    &client_id,
                    &pkg_name,
                    detected_version.as_deref().unwrap_or(""),
                    std::env::consts::ARCH,
                    std::env::consts::OS,
                    "", // libc: unknown from CLI
                    shadow_source_as_str(&source_type),
                    &source,
                    &recipe_toml,
                    &install_paths,
                    &reg_config_paths,
                    &reg_service_name,
                    &reg_sha256,
                    false, // mask_from_pm: opt-in only
                )
                .await?;
            println!("Registered shadow package:");
            println!("  ID:      {}", info.id);
            println!("  Name:    {}", info.name);
            println!("  Version: {}", info.version);
        }
        Add {
            name,
            tracker,
            non_interactive,
        } => {
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
            let (mut client, client_id) = connect(cert_dir).await?;
            let package_id = resolve_package_id(&mut client, &client_id, &name).await?;
            let (deleted, message) = client
                .unregister_shadow_package(&client_id, &package_id, true)
                .await?;
            if deleted {
                println!("✓ Unregistered: {}", message);
            } else {
                anyhow::bail!("Unregister failed: {}", message);
            }
        }
        List => {
            let (mut client, client_id) = connect(cert_dir).await?;
            let packages = client.list_shadow_packages(&client_id).await?;
            if packages.is_empty() {
                println!("No shadow packages registered.");
            } else {
                println!("{:<28} {:<14} {:<24} ID", "NAME", "VERSION", "SERVICE");
                for p in &packages {
                    println!(
                        "{:<28} {:<14} {:<24} {}",
                        p.name, p.version, p.service_name, p.id
                    );
                }
                println!();
                println!("{} package(s)", packages.len());
            }
        }
        Inspect { name } => {
            let (mut client, client_id) = connect(cert_dir).await?;
            let (recipe_toml, key_id, _signature, signed_at) =
                client.get_shadow_recipe(&client_id, &name).await?;
            println!("Recipe for '{}':", name);
            println!("{}", recipe_toml);
            println!();
            println!("Signed by key: {}", key_id);
            match chrono::DateTime::<chrono::Utc>::from_timestamp(signed_at, 0) {
                Some(t) => println!("Signed at:     {}", t.to_rfc3339()),
                None => println!("Signed at:     {} (unix)", signed_at),
            }
        }
        Check { name } => {
            let (mut client, client_id) = connect(cert_dir).await?;
            // Resolve the optional name filter to an id (empty = check all).
            let ids: Vec<String> = match &name {
                Some(n) => vec![resolve_package_id(&mut client, &client_id, n).await?],
                None => vec![],
            };
            let updates = client.check_shadow_updates(&client_id, &ids).await?;
            if let Some(n) = &name {
                println!("Checking updates for: {}", n);
            } else {
                println!("Checking updates for all shadow packages...");
            }
            println!();
            if updates.is_empty() {
                println!("All packages up to date.");
            } else {
                for u in &updates {
                    println!(
                        "{}: {} → {}",
                        u.package_name, u.current_version, u.latest_version
                    );
                    if !u.asset_url.is_empty() {
                        println!("  Asset: {}", u.asset_url);
                    }
                }
                println!();
                println!("{} update(s) available", updates.len());
            }
        }
        Update {
            name,
            dry_run,
            no_restart: _,
        } => {
            if dry_run {
                println!("Dry-run: checking updates for '{}'...", name);
            } else {
                println!("Updating shadow package: {}", name);
            }

            let (mut client, client_id) = connect(cert_dir).await?;
            let package_id = resolve_package_id(&mut client, &client_id, &name).await?;

            // Latest known update info (current→latest + asset URL).
            let updates = client
                .check_shadow_updates(&client_id, std::slice::from_ref(&package_id))
                .await?;
            let update = updates
                .iter()
                .find(|u| u.package_id == package_id)
                .or_else(|| updates.first());
            let update = match update {
                Some(u) if u.latest_version != u.current_version => u.clone(),
                _ => {
                    println!("No update available for '{}'.", name);
                    return Ok(());
                }
            };
            println!("  Available: {} → {}", update.current_version, update.latest_version);
            if !update.asset_url.is_empty() {
                println!("  Asset:     {}", update.asset_url);
            }

            if dry_run {
                println!("Dry-run complete (no changes made).");
                return Ok(());
            }

            // Verify the recipe's signature key against the trust policy
            // before executing anything.
            let recipe_toml = match load_verified_recipe(&mut client, &client_id, &name).await {
                Ok(t) => {
                    println!("Trust check: PASSED");
                    t
                }
                Err(e) => {
                    eprintln!("WARNING: recipe trust could not be verified: {}", e);
                    eprintln!("Recipe NOT executed. Recipe content:");
                    if let Ok((toml, key_id, _, _)) =
                        client.get_shadow_recipe(&client_id, &name).await
                    {
                        println!("{}", toml);
                        println!("(signed by key: {})", key_id);
                    }
                    println!("Update aborted (trust-UNVERIFIED).");
                    return Ok(());
                }
            };
            let recipe: monopsony_common::shadow_types::ShadowRecipe =
                toml::from_str(&recipe_toml)
                    .context("trust-verified recipe failed to parse")?;

            // Resolve the asset URL. The Check RPC returns an empty
            // asset_url for direct_url/scrape recipes (DirectUrlSource
            // exposes no assets for scrape discovery), so fall back to the
            // recipe's [direct_url].asset_url_template / github_release
            // asset selector rules. Script recipes tolerate an empty URL.
            let http = reqwest::Client::builder()
                .user_agent("monopsony-client/shadow-executor")
                .build()
                .context("building HTTP client")?;
            // Build-method recipes provision from source via [build].steps:
            // there is no release asset to resolve (git_release recipes
            // would error in resolve_asset_url), so pass an empty URL —
            // the executor skips the download for Build + empty URL the
            // same way it does for Script.
            let asset_url = if recipe.install.method
                == monopsony_common::shadow_types::InstallMethod::Build
            {
                String::new()
            } else {
                match shadow_executor::resolve_asset_url(
                    &recipe,
                    &update.asset_url,
                    &update.latest_version,
                    &http,
                )
                .await
                {
                    Ok(url) => url,
                    Err(e)
                        if recipe.install.method
                            == monopsony_common::shadow_types::InstallMethod::Script =>
                    {
                        println!("  Asset:     none resolved ({e}); script will fetch itself");
                        String::new()
                    }
                    Err(e) => {
                        anyhow::bail!("could not resolve asset URL for '{}': {e}", name);
                    }
                }
            };
            if !asset_url.is_empty() {
                println!("  Asset:     {}", asset_url);
            }

            // T4.7: repo-change gate — if the update's asset URL resolves to a
            // different base repo (host + repo path) than the registered
            // recipe's source_url, require operator re-approval before applying.
            if !asset_url.is_empty()
                && shadow_scorer::repo_base(&asset_url)
                    != shadow_scorer::repo_base(&recipe.package.source_url)
            {
                println!(
                    "⚠ RE-APPROVAL REQUIRED: asset base repo changed from {} to {}",
                    shadow_scorer::repo_base(&recipe.package.source_url),
                    shadow_scorer::repo_base(&asset_url)
                );
                let choice = read_line("Apply update anyway? [y/N]: ")?;
                if choice.as_str() != "y" {
                    println!("Update aborted (no re-approval).");
                    return Ok(());
                }
            }

            // Execute the verified recipe: download → pre_update → backup →
            // install (extract/script) → post_update → health check, with
            // automatic restore-from-backup on failure.
            //
            // Sprint 32 Phase 3: go through the diffing wrapper so recipe
            // requirement changes land in the same update log. GAP (recorded,
            // per hub decision 4): the client has NO local store of the
            // previously registered/adopted recipe (Sprint 30 persistence is
            // server-side only; recipes are fetched via gRPC), so previous is
            // always None today — zero diff warnings until a loadable
            // accessor lands. Do not invent new persistence here.
            let outcome = shadow_executor::execute_update_with_diff(
                &recipe,
                None,
                &update.latest_version,
                &asset_url,
                &http,
                Some(update.current_version.as_str()),
                true,
            )
            .await;
            for line in &outcome.log {
                println!("  {}", line);
            }
            if !outcome.success {
                if outcome.rolled_back {
                    eprintln!(
                        "✗ Update FAILED and was ROLLED BACK — previous version restored: {}",
                        outcome.error.as_deref().unwrap_or("unknown error")
                    );
                } else if outcome.held {
                    // failure_policy=hold kept the failed state on purpose — a
                    // backup exists but must not be restored without an operator.
                    eprintln!(
                        "✗ Update FAILED (failure_policy=hold — kept failed state, awaiting operator): {}",
                        outcome.error.as_deref().unwrap_or("unknown error")
                    );
                } else {
                    eprintln!(
                        "✗ Update FAILED (nothing to roll back yet): {}",
                        outcome.error.as_deref().unwrap_or("unknown error")
                    );
                }
                anyhow::bail!(
                    "update of '{}' to {} failed: {}",
                    name,
                    update.latest_version,
                    outcome.error.as_deref().unwrap_or("unknown error")
                );
            }

            // Success: record the REAL version + asset URL server-side so
            // history and package rows show actual versions.
            let (success, message, new_version, backup_path) = client
                .apply_shadow_update(
                    &client_id,
                    &package_id,
                    &outcome.new_version,
                    &asset_url,
                    true,
                )
                .await?;
            if success {
                println!("✓ Update applied: {}", message);
                println!("  New version: {}", new_version);
                if let Some(local_backup) = &outcome.backup_path {
                    println!("  Backup:      {}", local_backup.display());
                } else if !backup_path.is_empty() {
                    println!("  Backup:      {}", backup_path);
                }
            } else {
                eprintln!(
                    "WARNING: update executed locally but server bookkeeping failed: {}",
                    message
                );
                anyhow::bail!("server failed to record update: {}", message);
            }
        }
        Rollback { name } => {
            println!("Rolling back shadow package: {}", name);
            let (mut client, client_id) = connect(cert_dir).await?;
            let package_id = resolve_package_id(&mut client, &client_id, &name).await?;
            let (success, message, new_version, backup_path) = client
                .rollback_shadow_update(&client_id, &package_id)
                .await?;
            if success {
                println!("✓ Rollback applied: {}", message);
                println!("  Restored version: {}", new_version);
                if !backup_path.is_empty() {
                    println!("  Backup:           {}", backup_path);
                }
            } else {
                anyhow::bail!("Rollback failed: {}", message);
            }
        }
        Detect {
            path,
            source,
            name,
            out,
        } => {
            let pkg_name = name.clone().unwrap_or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            });
            let report = detect::detect(Path::new(&path), name.as_deref()).await?;
            let draft = detect::render_draft_toml(&report, &pkg_name, source.as_deref());
            print!("{}", draft);
            if let Some(out_path) = out {
                std::fs::write(&out_path, &draft)
                    .with_context(|| format!("writing draft to {}", out_path))?;
                println!("# Draft written to {}", out_path);
            }
            println!(
                "# Detection summary: sha256={:?} arch={:?} service={:?} needs_manual_verify={}",
                report.sha256.as_deref().unwrap_or("n/a"),
                report
                    .elf
                    .as_ref()
                    .map(|e| e.arch.as_str())
                    .unwrap_or("non-elf"),
                report
                    .service
                    .as_ref()
                    .map(|s| s.unit_name.as_str())
                    .unwrap_or("none"),
                report.needs_manual_verify
            );
        }
        Recipe { command: sub } => match sub {
            shadow_cli::RecipeCommand::Edit { name } => {
                println!("Edit recipe for: {}", name);
                println!("Recipe files are stored in /etc/monopsony/recipes/<name>.toml");
            }
            shadow_cli::RecipeCommand::Validate { path } => match std::fs::read_to_string(&path) {
                Ok(content) => {
                    match toml::from_str::<monopsony_common::shadow_types::ShadowRecipe>(&content) {
                        Ok(recipe) => println!(
                            "Recipe {} is valid (package: {}).",
                            path, recipe.package.name
                        ),
                        Err(e) => eprintln!("Recipe validation failed: {}", e),
                    }
                }
                Err(e) => eprintln!("Cannot read {}: {}", path, e),
            },
        },
        Adopt { name } => {
            println!("Adopting catalog package: {}", name);

            // 1. Fetch the catalog recipe and verify it against the trust
            //    policy. Unlike Update, a trust failure is a HARD abort: we
            //    never adopt (register + manage) an unverified recipe.
            let (mut client, client_id) = connect(cert_dir).await?;
            let recipe_toml = match load_verified_recipe(&mut client, &client_id, &name).await {
                Ok(t) => {
                    println!("Trust check: PASSED");
                    t
                }
                Err(e) => {
                    anyhow::bail!(
                        "Adopt aborted: recipe trust could not be verified ({}). \
                         Recipe content NOT adopted.",
                        e
                    );
                }
            };

            // Parse the recipe's [install] fields leniently (source url from
            // [package]; no strict full-recipe parse — catalog recipes may
            // carry extra keys).
            let source_url: String = toml::from_str::<toml::Value>(&recipe_toml)
                .ok()
                .and_then(|doc| {
                    doc.get("package")
                        .and_then(|p| p.get("source_url"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_default();
            let (install_paths, service_name) = shadow_cli::extract_install_fields(&recipe_toml);

            // 2. Confirm the local install actually exists before adopting:
            //    install-path existence + detect_command version probe.
            let finding = catalog_scan::scan_entry(&name, &recipe_toml);
            if !finding.installed {
                anyhow::bail!(
                    "'{}' not detected on this host; adopt refused (no local install to manage)",
                    name
                );
            }
            let detected_version = if finding.detected_version.is_empty() {
                String::new()
            } else {
                finding.detected_version.clone()
            };
            println!(
                "  Detected: {} (method: {})",
                if detected_version.is_empty() {
                    "<unknown>".to_string()
                } else {
                    detected_version.clone()
                },
                finding.method
            );

            // 3. Register the shadow package locally = record the
            //    assignment. Server-side register publishes/updates the
            //    catalog entry and creates the per-client row.
            let info = client
                .register_shadow_package(
                    &client_id,
                    &name,
                    &detected_version,
                    std::env::consts::ARCH,
                    std::env::consts::OS,
                    "", // libc: unknown from CLI
                    "adopted",
                    &source_url,
                    &recipe_toml,
                    &install_paths,
                    &[], // config_paths: not adopted from recipe install config_paths
                    &service_name,
                    "", // binary_sha256: not probed on the adopt path
                    false, // mask_from_pm: opt-in only
                )
                .await?;

            println!("✓ Adopted shadow package:");
            println!("  Name:    {}", name);
            println!(
                "  Version: {}",
                if detected_version.is_empty() {
                    "<unknown>".to_string()
                } else {
                    detected_version
                }
            );
            println!("  Method:  {}", finding.method);
            println!("  ID:      {}", info.id);
        }
    }
    Ok(())
}

async fn run_proxy_command(cmd: ProxyCommand, cert_dir: &Path) -> anyhow::Result<()> {
    // Sprint 23 slice 5: `--pm npm|pip|cargo` routes to the Phase B language
    // backends (npm/pip/cargo) instead of the distro toggle path. Dispatch
    // happens before target resolution so both `proxy on --pm npm` and
    // `proxy status --pm npm` are covered.
    if let Some(lang_result) = run_lang_pm_command(&cmd, cert_dir).await? {
        return lang_result;
    }
    // Sprint 26 Phase 3 Tier 1: `--system` routes to the macOS system-proxy
    // forwarding module (networksetup; forwarded, NOT cached).
    if let Some(system_result) = run_system_proxy_command(&cmd)? {
        return system_result;
    }
    match cmd {
        ProxyCommand::Status { .. } => {
            println!("Monopsony Proxy Status");
            println!("═══════════════════════════════════════════════");

            // Show server connection info
            let server_json_path = cert_dir.join("server.json");
            if server_json_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&server_json_path) {
                    if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                        let grpc_addr = info
                            .get("grpc_addr")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let paired_at = info
                            .get("paired_at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        println!("Server:           {}", grpc_addr);
                        println!("Paired at:        {}", paired_at);
                    }
                }
            } else {
                println!("Server:           not paired (no server.json)");
            }
            println!();

            // Check subsystems
            let results = proxy_toggle::proxy_status(cert_dir).await;
            if results.is_empty() {
                println!("No subsystems detected.");
            } else {
                for r in &results {
                    let status = if r.success { "ON  ✓" } else { "OFF ✗" };
                    println!("  {:<16} {}  {}", r.subsystem, status, r.message);
                }
            }
        }
        ProxyCommand::On {
            pm,
            docker,
            shadow,
            all,
            server,
            ..
        } => {
            let target = if all {
                proxy_toggle::ToggleTarget::All
            } else if let Some(name) = pm {
                proxy_toggle::ToggleTarget::Pm(name)
            } else if docker {
                proxy_toggle::ToggleTarget::Docker
            } else if shadow {
                proxy_toggle::ToggleTarget::Shadow
            } else {
                anyhow::bail!("Specify at least one of --all, --pm <name>, --docker, --shadow");
            };

            // Resolve server address
            let server_addr = if let Some(s) = server {
                s
            } else {
                resolve_server_addr(cert_dir)
                    .await
                    .unwrap_or_else(|_| "127.0.0.1:3128".to_string())
            };

            let results = proxy_toggle::proxy_on(target, &server_addr).await;
            let mut succeeded = 0;
            let mut failed = 0;
            for r in &results {
                if r.success {
                    println!("✓ {}: {}", r.subsystem, r.message);
                    succeeded += 1;
                } else {
                    println!("✗ {}: {}", r.subsystem, r.message);
                    failed += 1;
                }
            }
            println!("\n{} succeeded, {} failed", succeeded, failed);
            if failed > 0 {
                anyhow::bail!("{} actions failed", failed);
            }
        }
        ProxyCommand::Off {
            pm,
            docker,
            shadow,
            all,
            ..
        } => {
            let target = if all {
                proxy_toggle::ToggleTarget::All
            } else if let Some(name) = pm {
                proxy_toggle::ToggleTarget::Pm(name)
            } else if docker {
                proxy_toggle::ToggleTarget::Docker
            } else if shadow {
                proxy_toggle::ToggleTarget::Shadow
            } else {
                anyhow::bail!("Specify at least one of --all, --pm <name>, --docker, --shadow");
            };

            let results = proxy_toggle::proxy_off(target).await;
            let mut succeeded = 0;
            let mut failed = 0;
            for r in &results {
                if r.success {
                    println!("✓ {}: {}", r.subsystem, r.message);
                    succeeded += 1;
                } else {
                    println!("✗ {}: {}", r.subsystem, r.message);
                    failed += 1;
                }
            }
            println!("\n{} succeeded, {} failed", succeeded, failed);
            if failed > 0 {
                anyhow::bail!("{} actions failed", failed);
            }
        }
    }
    Ok(())
}

/// Sprint 23 slice 5: language package-manager (`npm|pip|cargo`) dispatch for
/// `proxy on|off|status --pm <lang>`.
///
/// Returns `Ok(None)` when the command does not target a language PM (caller
/// falls through to the distro toggle path), or `Ok(Some(result))` after the
/// language-PM path has fully handled the command.
///
/// `status` is a read-only config-file check (regenerates the backend's
/// config action, checks whether the file exists and contains a proxy line —
/// writes nothing). `on`/`off` execute the backend's `ProxyConfigAction` via
/// the shared executor in `monopsony-common`.
///
/// Unknown `--pm` values fall through to the distro path's existing
/// "Unknown package manager" error so the error message stays in one place.
async fn run_lang_pm_command(
    cmd: &ProxyCommand,
    cert_dir: &Path,
) -> anyhow::Result<Option<anyhow::Result<()>>> {
    let (pm_name, action_name, server_addr): (Option<&String>, &str, Option<String>) = match cmd {
        ProxyCommand::Status { .. } => (None, "status", None),
        ProxyCommand::On { pm, server, .. } => (pm.as_ref(), "on", server.clone()),
        ProxyCommand::Off { pm, .. } => (pm.as_ref(), "off", None),
    };

    let Some(name) = pm_name else {
        // No --pm flag at all: bare `status` prints all subsystems, leave it.
        if matches!(cmd, ProxyCommand::Status { .. }) {
            return Ok(None);
        }
        // on/off without --pm are handled by their own flags — fall through.
        return Ok(None);
    };

    let kind = match lang_pm_config::parse_lang_pm(name) {
        Some(k) => k,
        None => {
            // Not a language PM — fall through to the distro toggle path
            // (which reports unknown distro PM names itself).
            return Ok(None);
        }
    };

    match action_name {
        "status" => {
            let (on, msg) = lang_pm_config::lang_pm_status(kind);
            let status = if on { "ON  ✓" } else { "OFF ✗" };
            println!("{:<16} {}  {}", kind.as_str(), status, msg);
            Ok(Some(Ok(())))
        }
        "on" => {
            let server_addr = match server_addr {
                Some(s) => s,
                None => resolve_server_addr(cert_dir)
                    .await
                    .unwrap_or_else(|_| "127.0.0.1:3128".to_string()),
            };
            match lang_pm_config::lang_pm_apply(kind, &server_addr, true) {
                Ok(msg) => {
                    println!("✓ {}: {}", kind.as_str(), msg);
                    Ok(Some(Ok(())))
                }
                Err(e) => {
                    println!("✗ {}: {}", kind.as_str(), e);
                    Ok(Some(Err(anyhow::anyhow!("1 action failed"))))
                }
            }
        }
        "off" => match lang_pm_config::lang_pm_apply(kind, "", false) {
            Ok(msg) => {
                println!("✓ {}: {}", kind.as_str(), msg);
                Ok(Some(Ok(())))
            }
            Err(e) => {
                println!("✗ {}: {}", kind.as_str(), e);
                Ok(Some(Err(anyhow::anyhow!("1 action failed"))))
            }
        },
        _ => unreachable!("only status/on/off dispatched here"),
    }
}

/// Sprint 26 Phase 3 Tier 1: macOS system-proxy forwarding via networksetup.
///
/// Returns `Ok(Some(result))` when the `--system` flag claims the command
/// (status/on/off), `Ok(None)` to fall through to the other toggle paths.
/// Non-macOS hosts get an explicit unsupported message rather than a
/// silent no-op. The toggle runs `networksetup` via the shared
/// `ProxyConfigAction` executor and needs an admin-privileged caller
/// (networksetup prompts for credentials itself when the service is
/// admin-secured — no sudo needed for standard services).
fn run_system_proxy_command(cmd: &ProxyCommand) -> anyhow::Result<Option<anyhow::Result<()>>> {
    let (system_flag, action_name) = match cmd {
        ProxyCommand::Status { system, .. } => (*system, "status"),
        ProxyCommand::On { system, .. } => (*system, "on"),
        ProxyCommand::Off { system, .. } => (*system, "off"),
    };
    if !system_flag {
        return Ok(None);
    }
    if !system_proxy::is_supported() {
        println!("✗ system-proxy: only supported on macOS (networksetup not found)");
        return Ok(Some(Err(anyhow::anyhow!("unsupported platform"))));
    }
    match action_name {
        "status" => {
            println!("System proxy (macOS) — forwarded via monopsony client proxy, NOT cached:");
            match system_proxy::status() {
                Ok(rows) if rows.is_empty() => {
                    println!("  no configurable network services found");
                }
                Ok(rows) => {
                    for (svc, on, detail) in rows {
                        let status = if on { "ON  ✓" } else { "OFF ✗" };
                        println!("  {:<20} {}  {}", svc, status, detail);
                    }
                }
                Err(e) => {
                    println!("✗ system-proxy status: {}", e);
                    return Ok(Some(Err(anyhow::anyhow!("status failed"))));
                }
            }
            Ok(Some(Ok(())))
        }
        "on" => {
            if !system_proxy::status()
                .map(|r| r.iter().any(|(_, on, _)| *on))
                .unwrap_or(false)
            {
                // Not yet on: make sure the LOCAL client proxy is reachable
                // before pointing the whole system at it.
                match std::net::TcpStream::connect(system_proxy::listen_addr()) {
                    Ok(_) => {}
                    Err(e) => {
                        println!(
                            "✗ system-proxy: local client proxy not reachable at {} ({}). \
                             Start it with `monopsony-client daemon` or `proxy on --pm <pm>` first.",
                            system_proxy::listen_addr(),
                            e
                        );
                        return Ok(Some(Err(anyhow::anyhow!("local proxy not running"))));
                    }
                }
            }
            let rows = system_proxy::apply(true);
            report_system_proxy_rows(rows)
        }
        "off" => report_system_proxy_rows(system_proxy::apply(false)),
        _ => unreachable!("only status/on/off dispatched here"),
    }
}

fn report_system_proxy_rows(
    rows: Vec<(String, bool, String)>,
) -> anyhow::Result<Option<anyhow::Result<()>>> {
    let mut any_fail = false;
    for (svc, ok, msg) in &rows {
        if !ok {
            any_fail = true;
        }
        let mark = if *ok { "✓" } else { "✗" };
        println!("{} system-proxy [{}]: {}", mark, svc, msg);
    }
    if rows.is_empty() {
        println!("system-proxy: no network services to configure");
    }
    if any_fail {
        Ok(Some(Err(anyhow::anyhow!("one or more services failed"))))
    } else {
        Ok(Some(Ok(())))
    }
}

/// Sprint 26 Phase 3.5: one-shot platform setup. Currently the macOS Content
/// Caching lane. Sync like the system-proxy lane — blocking process spawns
/// only; never touches the async network stack.
fn run_setup(cmd: SetupMode) -> anyhow::Result<()> {
    // `Interactive` is handled (async) in main() before this call.
    let SetupMode::Mac {
        status: status_flag,
        disable,
    } = cmd
    else {
        anyhow::bail!("setup interactive must be handled by run_setup_interactive");
    };
    if !content_cache::is_supported() {
        println!("✗ setup mac: only supported on macOS (/usr/bin/AssetCacheManagerUtil not found)");
        anyhow::bail!("unsupported platform");
    }
    if status_flag {
        return run_setup_status();
    }
    if disable {
        println!("Deactivating Apple Content Caching (cached content stays on disk)...");
        println!("You may be prompted for your password (one sudo prompt):");
        let results = content_cache::disable_action().execute();
        if let Some(e) = results.iter().find_map(|r| r.as_ref().err()) {
            println!("✗ setup mac: {}", e);
            anyhow::bail!("deactivation failed");
        }
        println!("✓ Content Caching deactivated");
        return run_setup_status();
    }
    // Enable path: probe capability unprivileged first (rc=77 diagnosis
    // happens here — an unprivileged run fails BEFORE the sudo prompt).
    match content_cache::can_activate() {
        Ok(true) => {}
        Ok(false) => {
            println!("✗ setup mac: this Mac cannot host a Content Cache (canActivate reported no)");
            anyhow::bail!("cannot activate");
        }
        Err(e) => {
            println!("✗ setup mac: capability probe failed: {}", e);
            anyhow::bail!("probe failed");
        }
    }
    println!(
        "Activating Apple Content Caching — shared content only (no iCloud personal caching)."
    );
    println!("You may be prompted for your password (one sudo prompt):");
    let results = content_cache::enable_action().execute();
    if let Some(e) = results.iter().find_map(|r| r.as_ref().err()) {
        println!("✗ setup mac: {}", e);
        anyhow::bail!("activation failed");
    }
    println!("✓ Content Caching activated — verifying status:");
    run_setup_status()
}

/// Sprint 26 Phase 4 (B4 slice 2): trigger a macOS softwareupdate background
/// scan. `--clear-catalog` no longer exists on current macOS (verified on the
/// 26.3.1 bench — absent from `softwareupdate --help`), so this is the only
/// live scan verb. Meaningfulness (does scan traffic route through Monopsony?)
/// depends on system-proxy / Content Caching state and is verified on the
/// bench, not here. The scan daemonizes: exit code reflects the trigger, not
/// the scan result.
fn run_update_scan() -> anyhow::Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("update-scan: only supported on macOS");
    }
    let out = std::process::Command::new("/usr/sbin/softwareupdate")
        .arg("--background")
        .output()
        .map_err(|e| anyhow::anyhow!("update-scan: failed to spawn softwareupdate: {}", e))?;
    if out.status.success() {
        println!("✓ softwareupdate background scan triggered");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "update-scan: softwareupdate exited {} ({})",
            out.status.code().unwrap_or(-1),
            stderr.trim()
        )
    }
}

/// Unprivileged status render for the setup lane. ON = activated AND scoped
/// to shared content; the detail line always tells the truth either way.
fn run_setup_status() -> anyhow::Result<()> {
    match content_cache::status() {
        Ok(st) => {
            println!("Apple Content Caching (this Mac):");
            let (on, detail) = content_cache::enabled_and_private(&st);
            let verdict = if on { "ON  ✓" } else { "OFF ✗" };
            println!("  {:<12} {}  {}", "cache", verdict, detail);
            Ok(())
        }
        Err(e) => {
            println!("✗ setup mac status: {}", e);
            anyhow::bail!("status failed");
        }
    }
}

#[cfg(test)]
mod remote_assignment_tests {
    use super::remote_assignment_allowed;

    #[test]
    fn test_remote_assignment_gate_requires_both() {
        // Opt-in and trust must both pass.
        assert!(remote_assignment_allowed(true, true));
    }

    #[test]
    fn test_remote_assignment_gate_defaults_closed() {
        // Default posture: no opt-in → refused regardless of trust.
        assert!(!remote_assignment_allowed(false, true));
        // Opted in but trust failed → refused.
        assert!(!remote_assignment_allowed(true, false));
        assert!(!remote_assignment_allowed(false, false));
    }
}
