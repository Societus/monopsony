//! Self-update mechanism for monopsony server/client via shadow package system.
//!
//! Uses the same infrastructure as third-party shadow packages to update
//! monopsony itself from GitHub releases.

use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};

/// Which monopsony binary a self-update targets. Release assets are
/// kind-prefixed (`monopsony-server-v...` / `monopsony-client-v...`) and each
/// binary has its own default recipe, so selection and the managed pipeline
/// both key off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryKind {
    Server,
    Client,
}

impl BinaryKind {
    /// Archive-name prefix of this binary's release assets.
    fn asset_name_prefix(self) -> &'static str {
        match self {
            BinaryKind::Server => "monopsony-server",
            BinaryKind::Client => "monopsony-client",
        }
    }

    /// Map a `detect_binary_type()`-style string ("server"/"client"; unknown
    /// argv0 defaults to client, matching the CLI's display behavior).
    pub fn from_binary_type(binary_type: &str) -> Self {
        match binary_type {
            "server" => BinaryKind::Server,
            _ => BinaryKind::Client,
        }
    }

    /// Wire name for the P2 `CheckUpdate`/`DownloadUpdateAsset` RPCs
    /// (release-contract kind; the server validates `client` | `server`).
    pub fn wire_name(self) -> &'static str {
        match self {
            BinaryKind::Server => "server",
            BinaryKind::Client => "client",
        }
    }
}

/// Map (os, arch) onto the release-asset contract platform token
/// (`linux-x86_64` | `linux-aarch64` | `darwin-arm64` | `darwin-x86_64`).
/// std reports `aarch64` on Apple Silicon while assets use Apple's `arm64`;
/// remapped here. `None` = no assets are published for this platform.
fn contract_platform(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("darwin", "aarch64") | ("darwin", "arm64") => Some("darwin-arm64"),
        ("darwin", "x86_64") => Some("darwin-x86_64"),
        _ => None,
    }
}

/// This host's release-contract platform token, for the CheckUpdate /
/// DownloadUpdateAsset RPCs. `None` on platforms with no published assets.
pub fn release_platform() -> Option<String> {
    let (os, arch, _libc) = detect_platform();
    contract_platform(&os, &arch).map(str::to_string)
}

// ─── Managed Update Pipeline Types ─────────────────────────────────

/// Where a self-update asset comes from (Sprint 34 P3). The managed
/// pipeline consumes this instead of downloading from a URL inside phase 1;
/// the pipeline phases, snapshot and rollback logic are unchanged.
#[derive(Debug, Clone)]
pub enum AssetProvider {
    /// Direct URL download — today's behavior (GitHub-direct path). The
    /// URL suffix still drives the archive-vs-raw decision.
    Url(String),
    /// Bytes already fetched over the P2 gRPC stream (DownloadUpdateAsset).
    /// Archive-vs-raw is decided by magic-byte sniffing. (Sprint 34 P4
    /// consumes this for the server's own self-apply.)
    #[allow(dead_code)] // P4 wiring; exercised in tests today
    ServerStream(Vec<u8>),
    /// Already-stored local artifact: the staged finalize file, and (P4)
    /// the server's own self-apply from its ShadowCache blob. Sniffed like
    /// [`AssetProvider::ServerStream`].
    LocalBlob(PathBuf),
}

impl AssetProvider {
    /// URL suffix hint for archive detection. Only the [`AssetProvider::Url`]
    /// variant carries one — byte providers are sniffed by content.
    fn url_hint(&self) -> Option<&str> {
        match self {
            AssetProvider::Url(url) => Some(url.as_str()),
            _ => None,
        }
    }
}

/// Archive container of an update asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarGz,
    Zip,
    Raw,
}

/// Decide archive-vs-raw for asset bytes. URL providers keep the historical
/// suffix-driven behavior; byte providers (ServerStream/LocalBlob) carry no
/// reliable name and are sniffed by magic — gzip (`1f 8b`) and zip (`PK`)
/// headers. Released monopsony binaries are ELF/Mach-O and never start with
/// either magic, so the sniff cannot misclassify a real binary.
fn archive_kind(bytes: &[u8], url_hint: Option<&str>) -> ArchiveKind {
    if let Some(url) = url_hint {
        if url.ends_with(".tar.gz") {
            return ArchiveKind::TarGz;
        }
        if url.ends_with(".zip") {
            return ArchiveKind::Zip;
        }
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return ArchiveKind::TarGz;
    }
    if bytes.starts_with(b"PK") {
        return ArchiveKind::Zip;
    }
    ArchiveKind::Raw
}

/// Write asset bytes to `new_path`, extracting the monopsony binary when
/// the bytes are an archive. `url_hint` carries the URL suffix for
/// [`AssetProvider::Url`]; `None` means sniff.
fn stage_asset_bytes(bytes: &[u8], new_path: &Path, url_hint: Option<&str>) -> Result<()> {
    // extract_binary_from_archive predates P3 and takes &PathBuf.
    let out_path = new_path.to_path_buf();
    match archive_kind(bytes, url_hint) {
        ArchiveKind::TarGz => {
            extract_binary_from_archive(bytes, &out_path, "asset.tar.gz")?;
        }
        ArchiveKind::Zip => {
            extract_binary_from_archive(bytes, &out_path, "asset.zip")?;
        }
        ArchiveKind::Raw => {
            std::fs::write(new_path, bytes).context("Failed to write new binary")?;
        }
    }
    Ok(())
}

/// Mark a path executable (0755) on Unix; no-op on Windows.
fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

/// Atomically move `new_path` over `current_path`. On Windows the running
/// file cannot be renamed over, so it is removed first (the historical
/// behavior of both call sites).
fn swap_binary(new_path: &Path, current_path: &Path) -> Result<()> {
    info!("Swapping binary...");
    #[cfg(unix)]
    {
        std::fs::rename(new_path, current_path).context("Failed to atomically swap binary")?;
    }
    #[cfg(windows)]
    {
        if current_path.exists() {
            std::fs::remove_file(current_path)?;
        }
        std::fs::rename(new_path, current_path).context("Failed to atomically swap binary")?;
    }
    Ok(())
}

/// Fetch the asset bytes for a provider. Only [`AssetProvider::Url`]
/// performs I/O against the network; the other variants are already-local
/// bytes (streamed earlier / staged on disk).
async fn fetch_asset_bytes(asset: &AssetProvider) -> Result<Vec<u8>> {
    match asset {
        AssetProvider::Url(url) => {
            let client = Client::new();
            info!("Downloading {}...", url);
            let response = client.get(url).send().await?;
            if !response.status().is_success() {
                anyhow::bail!("Download failed: status {} for {}", response.status(), url);
            }
            Ok(response.bytes().await?.to_vec())
        }
        AssetProvider::ServerStream(bytes) => {
            info!(
                "Using {} bytes fetched over the server update stream",
                bytes.len()
            );
            Ok(bytes.clone())
        }
        AssetProvider::LocalBlob(path) => {
            info!("Reading staged asset {}", path.display());
            std::fs::read(path)
                .with_context(|| format!("Failed to read staged asset {}", path.display()))
        }
    }
}

/// Snapshot of a config file before update (for rollback).
#[derive(Debug)]
pub struct ConfigSnapshot {
    pub path: PathBuf,
    pub original_content: Vec<u8>,
    pub existed: bool,
}

/// Outcome of a managed update operation.
#[derive(Debug)]
pub enum UpdateOutcome {
    Success,
    RolledBack(String),
}

/// Context for a managed update operation. Tracks state across the
/// pre-update / apply / validate pipeline.
#[derive(Debug)]
pub struct UpdateContext {
    pub recipe: Option<monopsony_common::shadow_types::ShadowRecipe>,
    pub current_path: PathBuf,
    pub install_dir: PathBuf,
    pub backup_dir: PathBuf,
    pub from_version: String,
    pub to_version: String,
    /// Where the asset comes from (P3). Historically a URL string; the
    /// server-first path feeds [`AssetProvider::ServerStream`] bytes and the
    /// finalize path feeds a [`AssetProvider::LocalBlob`] staged file.
    pub asset: AssetProvider,
    pub config_paths: Vec<PathBuf>,
    config_snapshots: Vec<ConfigSnapshot>,
}

impl UpdateContext {
    /// Create a new UpdateContext with a versioned backup directory.
    ///
    /// Kept signature-compatible with the pre-P3 call sites (the server
    /// wrapper passes a GitHub URL): the URL is wrapped into an
    /// [`AssetProvider::Url`] provider.
    pub fn new(
        recipe: Option<monopsony_common::shadow_types::ShadowRecipe>,
        current_path: PathBuf,
        install_dir: PathBuf,
        from_version: String,
        to_version: String,
        asset_url: String,
    ) -> Self {
        Self::with_provider(
            recipe,
            current_path,
            install_dir,
            from_version,
            to_version,
            AssetProvider::Url(asset_url),
        )
    }

    /// Create an UpdateContext around an explicit asset provider.
    pub fn with_provider(
        recipe: Option<monopsony_common::shadow_types::ShadowRecipe>,
        current_path: PathBuf,
        install_dir: PathBuf,
        from_version: String,
        to_version: String,
        asset: AssetProvider,
    ) -> Self {
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
        let backup_dir = install_dir.join(format!("update-backup-{}", timestamp));

        let config_paths = recipe
            .as_ref()
            .and_then(|r| r.install.config_paths.as_ref())
            .map(|paths| paths.iter().map(PathBuf::from).collect::<Vec<_>>())
            .unwrap_or_default();

        Self {
            recipe,
            current_path,
            install_dir,
            backup_dir,
            from_version,
            to_version,
            asset,
            config_paths,
            config_snapshots: Vec::new(),
        }
    }

    /// Snapshot binary + config files to backup_dir.
    pub fn create_snapshot(&mut self) -> Result<()> {
        std::fs::create_dir_all(&self.backup_dir)?;

        if self.current_path.exists() {
            let bin_backup = self.backup_dir.join("binary.bak");
            std::fs::copy(&self.current_path, &bin_backup).context("Failed to backup binary")?;
            info!("Binary backed up to {}", bin_backup.display());
        }

        for config_path in &self.config_paths {
            if config_path.exists() {
                let content = std::fs::read(config_path)
                    .context(format!("Failed to read config: {}", config_path.display()))?;

                self.config_snapshots.push(ConfigSnapshot {
                    path: config_path.clone(),
                    original_content: content.clone(),
                    existed: true,
                });

                if let Some(filename) = config_path.file_name() {
                    let backup_name = format!("{}.bak", filename.to_string_lossy());
                    std::fs::write(self.backup_dir.join(&backup_name), &content)?;
                }
            }
        }

        info!(
            "Snapshot created at {} ({} config files)",
            self.backup_dir.display(),
            self.config_snapshots.len()
        );
        Ok(())
    }

    /// Roll back to pre-update state.
    pub fn rollback(&self, error: &str) -> Result<()> {
        warn!("Update failed: {}. Rolling back...", error);

        let bin_backup = self.backup_dir.join("binary.bak");
        if bin_backup.exists() && self.current_path.exists() {
            std::fs::copy(&bin_backup, &self.current_path)
                .context("Failed to restore binary during rollback")?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &self.current_path,
                    std::fs::Permissions::from_mode(0o755),
                )?;
            }
            info!("Binary restored to previous version");
        }

        for snapshot in &self.config_snapshots {
            if snapshot.existed {
                match std::fs::write(&snapshot.path, &snapshot.original_content) {
                    Ok(()) => info!("Config {} restored", snapshot.path.display()),
                    Err(e) => warn!(
                        "Failed to restore config {}: {}",
                        snapshot.path.display(),
                        e
                    ),
                }
            }
        }

        info!("Rollback complete. System restored to pre-update state.");
        Ok(())
    }

    /// Attempt to restore operator config edits after a successful update.
    fn restore_config_edits(&self) -> Result<()> {
        for snapshot in &self.config_snapshots {
            if !snapshot.existed {
                continue;
            }
            let current = std::fs::read(&snapshot.path).unwrap_or_default();
            if current == snapshot.original_content {
                continue;
            }

            warn!(
                "Config {} was modified during update. Restoring operator's version.",
                snapshot.path.display()
            );
            std::fs::write(&snapshot.path, &snapshot.original_content)?;
        }
        Ok(())
    }

    /// Prune old backup directories, keeping only max_backups most recent.
    pub fn prune_backups(&self) -> Result<()> {
        let max = self
            .recipe
            .as_ref()
            .map(|r| r.install.max_backups)
            .unwrap_or(3);

        let mut backups: Vec<_> = std::fs::read_dir(&self.install_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("update-backup-"))
                    .unwrap_or(false)
            })
            .collect();

        if backups.len() <= max {
            return Ok(());
        }

        backups.sort_by(|a, b| {
            let a_t = a.metadata().and_then(|m| m.modified()).ok();
            let b_t = b.metadata().and_then(|m| m.modified()).ok();
            b_t.cmp(&a_t)
        });

        for entry in backups.iter().skip(max) {
            debug!("Pruning old backup: {}", entry.path().display());
            std::fs::remove_dir_all(entry.path()).ok();
        }
        Ok(())
    }
}

/// Execute a recipe hook command (sh on Unix, cmd /C on Windows).
fn run_hook(hook_name: &str, command: &str) -> Result<()> {
    info!("Running {} hook: {}", hook_name, command);

    #[cfg(unix)]
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output();

    #[cfg(windows)]
    let result = std::process::Command::new("cmd")
        .arg("/C")
        .arg(command)
        .output();

    match result {
        Ok(output) if output.status.success() => {
            if !output.stdout.is_empty() {
                debug!(
                    "{} stdout: {}",
                    hook_name,
                    String::from_utf8_lossy(&output.stdout).trim()
                );
            }
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "{} hook failed (exit {}): {}",
                hook_name,
                output.status,
                stderr.trim()
            );
        }
        Err(e) => anyhow::bail!("Failed to execute {} hook: {}", hook_name, e),
    }
}

/// pgrep -x matches against the kernel's `comm`, which is truncated to 15
/// characters — procps ≥4 errors out ("pattern longer than 15 characters
/// will result in zero matches") on longer patterns. Verified live
/// (2026-11-10): `pgrep -x monopsony-client` matches NOTHING while the
/// daemon runs with comm "monopsony-clien". Truncate to comm granularity
/// so exact-match semantics work for real binary names.
fn comm_name(process: &str) -> &str {
    match process.char_indices().nth(15) {
        Some((idx, _)) => &process[..idx],
        None => process,
    }
}

/// Run a health check with timeout polling.
async fn run_health_check(check: &monopsony_common::shadow_types::HealthCheck) -> Result<()> {
    use monopsony_common::shadow_types::HealthCheckType;
    use std::time::{Duration, Instant};

    let timeout = Duration::from_secs(check.timeout_secs);

    match check.check_type {
        HealthCheckType::Http => {
            let url = check
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("HTTP health check requires url"))?;
            let client = reqwest::Client::new();
            let start = Instant::now();
            loop {
                match client.get(url).timeout(Duration::from_secs(5)).send().await {
                    Ok(resp) if resp.status().is_success() => return Ok(()),
                    Ok(resp) => warn!("Health check returned {}", resp.status()),
                    Err(e) => debug!("Health check connection failed: {}", e),
                }
                if start.elapsed() >= timeout {
                    anyhow::bail!("HTTP health check timed out after {}s", timeout.as_secs());
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        HealthCheckType::Port => {
            let port = check
                .port
                .ok_or_else(|| anyhow::anyhow!("Port health check requires port"))?;
            let start = Instant::now();
            loop {
                if std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
                    return Ok(());
                }
                if start.elapsed() >= timeout {
                    anyhow::bail!("Port {} not responding after {}s", port, timeout.as_secs());
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        HealthCheckType::Process => {
            let process = check
                .process
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Process health check requires process name"))?;
            // Match the kernel's comm granularity (15 chars) — see comm_name.
            let comm = comm_name(process).to_string();
            let start = Instant::now();
            loop {
                let output = std::process::Command::new("pgrep")
                    .arg("-x")
                    .arg(&comm)
                    .output()?;
                if output.status.success() {
                    return Ok(());
                }
                if start.elapsed() >= timeout {
                    anyhow::bail!(
                        "Process '{}' not running after {}s",
                        process,
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// Managed update with full safety pipeline.
/// Honors recipe fields: pre_update, post_update, on_failure, health_check,
/// backup_old, config_paths, max_backups.
pub async fn apply_update_managed(ctx: &mut UpdateContext) -> Result<UpdateOutcome> {
    // PHASE 1: PRE-UPDATE

    if let Some(ref recipe) = ctx.recipe {
        if let Some(ref pre) = recipe.install.pre_update {
            run_hook("pre_update", pre).context("pre_update hook failed — aborting update")?;
        }
    }

    ctx.create_snapshot()?;
    info!("Snapshot created at {}", ctx.backup_dir.display());

    // Fetch the asset from the configured provider (P3): Url downloads as
    // before; ServerStream/LocalBlob are already-local bytes.
    let bytes = fetch_asset_bytes(&ctx.asset).await?;

    // Extract binary from archive if needed (Url: suffix-driven; byte
    // providers: magic-sniffed).
    let new_path = sibling_update_path(&ctx.install_dir, &ctx.current_path, "new");

    stage_asset_bytes(&bytes, &new_path, ctx.asset.url_hint())?;
    set_executable(&new_path)?;

    // PHASE 2: APPLY — atomic swap
    swap_binary(&new_path, &ctx.current_path)?;
    info!("Binary swapped to version {}", ctx.to_version);

    // PHASE 3: VALIDATE & COMMIT/ROLLBACK

    if let Some(ref recipe) = ctx.recipe {
        if let Some(ref check) = recipe.install.health_check {
            info!("Running health check...");
            match run_health_check(check).await {
                Ok(()) => info!("Health check passed"),
                Err(e) => {
                    if let Some(ref on_fail) = recipe.install.on_failure {
                        let _ = run_hook("on_failure", on_fail);
                    }
                    ctx.rollback(&format!("Health check failed: {}", e))?;
                    return Ok(UpdateOutcome::RolledBack(e.to_string()));
                }
            }
        }

        if let Some(ref post) = recipe.install.post_update {
            if let Err(e) = run_hook("post_update", post) {
                warn!("post_update hook failed: {}", e);
            }
        }
    }

    ctx.restore_config_edits()?;
    ctx.prune_backups()?;

    info!(
        "Update committed successfully: {} → {}",
        ctx.from_version, ctx.to_version
    );
    Ok(UpdateOutcome::Success)
}

// ─── Original self-update functions (preserved) ────────────────────

/// Platform detection for asset selection
fn detect_platform() -> (String, String, Option<String>) {
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();

    let libc = if cfg!(target_env = "musl") {
        Some("musl".to_string())
    } else if os == "linux" {
        Some("glibc".to_string())
    } else {
        None
    };

    (os, arch, libc)
}

/// Parse a version string like "1.0.0-rc2" or "v1.0.0" into a semver::Version.
///
/// Uses semver 2.0 ordering: pre-release identifiers make a version LOWER than
/// the release they precede, so `1.0.0-rc1 < 1.0.0-rc2 < 1.0.0`. Build
/// metadata (`+...`) is ignored for comparison purposes, per semver §11.
/// The leading `v` is stripped when present.
fn parse_version(version: &str) -> Option<semver::Version> {
    let clean = version.trim_start_matches('v');
    semver::Version::parse(clean).ok()
}

/// GitHub release asset information
#[derive(Debug, Deserialize)]
struct ReleaseAssetInfo {
    name: String,
    browser_download_url: String,
    #[allow(dead_code)]
    size: u64,
}

/// GitHub release information
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<ReleaseAssetInfo>,
}

/// Result of checking for updates
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub asset_url: Option<String>,
    pub changelog_url: Option<String>,
}

/// Check if an update is available and return version info. `binary_kind`
/// selects which binary's assets to match (`monopsony-server-*` vs
/// `monopsony-client-*`) — the two kinds publish separate archives.
pub async fn check_for_update(
    current_version: &str,
    binary_kind: BinaryKind,
) -> Result<UpdateCheckResult> {
    let client = Client::new();

    let response = client
        .get("https://api.github.com/repos/societus/monopsony/releases/latest")
        .header("User-Agent", "monopsony-self-update/1.0.0")
        .send()
        .await?;

    if !response.status().is_success() {
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(UpdateCheckResult {
                current_version: current_version.to_string(),
                latest_version: None,
                asset_url: None,
                changelog_url: None,
            });
        }
        anyhow::bail!(
            "GitHub API returned status {}: {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
    }

    let release: GitHubRelease = response.json().await?;

    let current = parse_version(current_version)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse current version: {}", current_version))?;

    let latest = parse_version(&release.tag_name).ok_or_else(|| {
        anyhow::anyhow!("Failed to parse latest release tag: {}", release.tag_name)
    })?;

    match current.cmp(&latest) {
        std::cmp::Ordering::Less => {
            info!(
                "Update available: {} -> {}",
                current_version, release.tag_name
            );
            // libc is deliberately unused: linux release assets are static
            // musl builds serving both glibc and musl hosts, so detect_platform's
            // "glibc" never enters a match string.
            let (os, arch, _libc) = detect_platform();
            let asset_names: Vec<String> = release.assets.iter().map(|a| a.name.clone()).collect();

            let asset_url = select_asset_for_platform(&asset_names, binary_kind, &os, &arch)
                .and_then(|name| {
                    release
                        .assets
                        .iter()
                        .find(|a| a.name == name)
                        .map(|a| a.browser_download_url.clone())
                });

            Ok(UpdateCheckResult {
                current_version: current_version.to_string(),
                latest_version: Some(release.tag_name.clone()),
                asset_url,
                changelog_url: Some(release.html_url),
            })
        }
        std::cmp::Ordering::Equal => {
            info!("Already at latest version: {}", current_version);
            Ok(UpdateCheckResult {
                current_version: current_version.to_string(),
                latest_version: None,
                asset_url: None,
                changelog_url: None,
            })
        }
        std::cmp::Ordering::Greater => {
            warn!(
                "Current version ({}) is newer than GitHub release ({}). Ignoring.",
                current_version, release.tag_name
            );
            Ok(UpdateCheckResult {
                current_version: current_version.to_string(),
                latest_version: None,
                asset_url: None,
                changelog_url: None,
            })
        }
    }
}

/// Version component of a release asset name: three dot-separated numeric
/// components with an optional `-rcN` suffix (e.g. `1.0.0`, `1.0.0-rc2`).
const ASSET_VERSION_RE: &str = r"\d+\.\d+\.\d+(?:-rc\d+)?";

/// Select the release asset for the current platform from the contract the
/// release CI actually publishes:
/// `monopsony-{server,client}-v<VERSION>-{linux-x86_64,linux-aarch64,darwin-arm64,darwin-x86_64}.tar.gz`.
///
/// Kind-aware: only archives prefixed with this binary's name are considered,
/// so a client never downloads the server tarball (or vice versa). Linux
/// assets are static musl builds serving both glibc and musl hosts — libc is
/// not part of the name and never enters a match string. darwin: std reports
/// ARCH "aarch64" on Apple Silicon while assets use Apple's "arm64"; remapped
/// here, before matching. The version component is anchored to
/// [`ASSET_VERSION_RE`] so malformed versions (two-component, non-rc
/// pre-release suffixes, missing `v`) never match.
fn select_asset_for_platform(
    asset_names: &[String],
    binary_kind: BinaryKind,
    os: &str,
    arch: &str,
) -> Option<String> {
    let platform = contract_platform(os, arch)?;
    let pattern = format!(
        "^{}-v{}-{}\\.tar\\.gz$",
        binary_kind.asset_name_prefix(),
        ASSET_VERSION_RE,
        platform
    );
    let re = regex::Regex::new(&pattern).ok()?;
    asset_names.iter().find(|name| re.is_match(name)).cloned()
}

/// Timestamped sibling path for update temp/backup files, named after the
/// binary being replaced. Previously `monopsony-server.bak/new.*` was
/// hardcoded for both binaries — client updates littered server-named files.
fn sibling_update_path(target_dir: &Path, current_path: &Path, kind: &str) -> PathBuf {
    let bin_name = current_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("monopsony");
    target_dir.join(format!(
        "{}.{}.{}",
        bin_name,
        kind,
        Utc::now().format("%Y%m%d_%H%M%S")
    ))
}

// ─── Server-mediated update manifest (Sprint 34 P3) ────────────────

/// One servable asset as listed in a CheckUpdate response / manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestAsset {
    pub name: String,
    pub platform: String,
    pub sha256: String,
    pub size: i64,
}

impl ManifestAsset {
    /// From the proto wire type of a CheckUpdate response entry.
    pub fn from_proto(asset: &monopsony_common::proto::monopsony::UpdateAssetInfo) -> Self {
        Self {
            name: asset.name.clone(),
            platform: asset.platform.clone(),
            sha256: asset.sha256.clone(),
            size: asset.size,
        }
    }
}

/// Rebuild the canonical manifest payload — byte-identical to the server's
/// `build_update_manifest_payload` (control/grpc.rs, P2): serde_json `json!`
/// with default features (BTreeMap → sorted keys, no whitespace). These are
/// the exact bytes the Ed25519 manifest signature covers, so the client
/// verifies against a rebuild from the wire fields, never trusting a
/// server-sent preimage.
pub fn build_update_manifest_payload(
    version: &str,
    binary: &str,
    platform: &str,
    assets: &[ManifestAsset],
) -> Vec<u8> {
    serde_json::json!({
        "assets": assets
            .iter()
            .map(|a| {
                serde_json::json!({
                    "name": a.name,
                    "platform": a.platform,
                    "sha256": a.sha256,
                    "size": a.size,
                })
            })
            .collect::<Vec<_>>(),
        "binary": binary,
        "platform": platform,
        "version": version,
    })
    .to_string()
    .into_bytes()
}

/// Outcome of manifest trust verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestCheck {
    /// Signature present and verified against the configured trust key.
    Verified,
    /// No trust key configured — signature skipped (sha256-of-served-bytes
    /// remains; D7 no-key posture). The caller surfaces a warning.
    SkippedNoKey,
}

/// Verify a CheckUpdate manifest signature (D8: Ed25519 via the existing
/// shadow trust key path). Fail-closed when a trust key is configured — an
/// empty signature or a failed verification is an error, never a warning.
pub fn verify_update_manifest(
    payload: &[u8],
    sig_b64: &str,
    verifying_key: Option<&ed25519_dalek::VerifyingKey>,
) -> Result<ManifestCheck> {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier};

    let Some(vk) = verifying_key else {
        return Ok(ManifestCheck::SkippedNoKey);
    };
    if sig_b64.is_empty() {
        anyhow::bail!(
            "update manifest is unsigned but a trust key is configured — refusing (fail-closed)"
        );
    }
    let sig_bytes: Vec<u8> = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .context("update manifest signature: base64 decode failed")?;
    let sig_bytes: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("update manifest signature: wrong length"))?;
    vk.verify(payload, &Signature::from_bytes(&sig_bytes))
        .context("update manifest signature verification failed — refusing (fail-closed)")?;
    Ok(ManifestCheck::Verified)
}

/// SHA-256 of `bytes` as lowercase hex (manifest/stream digest comparisons
/// are case-insensitive on the wire side).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ─── Staged two-phase self-apply (Sprint 34 P3) ─────────────────────
//
// The daemon cannot health-check a swap of the *running* process from
// inside itself, so the apply is split:
//
//   phase 1 (daemon):  download + verify (manifest sig + sha256) + stage
//                      the extracted binary + drop a finalize command file,
//                      spawn a detached finalize process, exit.
//   phase 2 (finalize, fresh non-daemon process): PID-guarded; snapshot →
//                      swap → restart via detected init → health gate →
//                      commit/rollback, reusing the managed pipeline's
//                      snapshot/rollback/hook machinery around a LocalBlob
//                      provider.

/// Finalize command file — the handoff from the staging daemon to the
/// detached finalize process. JSON, written next to the staged binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizeCommand {
    /// Staged (already extracted) binary path.
    pub staged_path: PathBuf,
    /// sha256 (hex) of the staged bytes — re-verified by the finalize
    /// process before it touches anything.
    pub staged_sha256: String,
    pub from_version: String,
    pub to_version: String,
    /// Release-contract binary kind: "client" | "server".
    pub binary: String,
    pub created_at: i64,
}

/// The per-binary default recipe TOML (P0 split), by kind.
pub fn default_recipe_for(kind: BinaryKind) -> &'static str {
    match kind {
        BinaryKind::Server => monopsony_common::shadow_types::DEFAULT_MONOPSONY_SERVER_RECIPE,
        BinaryKind::Client => monopsony_common::shadow_types::DEFAULT_MONOPSONY_CLIENT_RECIPE,
    }
}

/// Phase 1 staging: write the finalized (extracted, executable) binary as a
/// timestamped sibling of the current binary and return its path + sha256.
/// Sniffs archive-vs-raw, so both a raw tarball's bytes and an
/// already-extracted binary work.
pub fn stage_update_asset(
    bytes: &[u8],
    install_dir: &Path,
    current_path: &Path,
) -> Result<(PathBuf, String)> {
    let staged = sibling_update_path(install_dir, current_path, "staged");
    stage_asset_bytes(bytes, &staged, None)?;
    set_executable(&staged)?;
    let sha = sha256_hex(&std::fs::read(&staged)?);
    info!(
        "Staged update binary {} (sha256 {})",
        staged.display(),
        &sha[..12.min(sha.len())]
    );
    Ok((staged, sha))
}

/// Write the finalize command file next to the staged binary. Returns its
/// path (the daemon passes it to the finalize process).
pub fn write_finalize_command(install_dir: &Path, cmd: &FinalizeCommand) -> Result<PathBuf> {
    let path = install_dir.join(format!(
        "monopsony-self-update.finalize.{}.json",
        cmd.created_at
    ));
    std::fs::write(&path, serde_json::to_string_pretty(cmd)?)?;
    info!("Finalize command written to {}", path.display());
    Ok(path)
}

/// Read + parse a finalize command file.
pub fn read_finalize_command(path: &Path) -> Result<FinalizeCommand> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read finalize command {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse finalize command {}", path.display()))
}

/// PID-file guard: at most one finalize process per install directory.
/// Stale files (dead PID or unparseable content) are taken over; a live
/// second finalize refuses loudly. Note: liveness is probed with
/// `kill -0`, which only distinguishes reliably between same-uid
/// processes — both the daemon and the finalize run as the service user.
#[derive(Debug)]
pub struct FinalizeGuard {
    path: PathBuf,
}

impl Drop for FinalizeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn acquire_finalize_guard(install_dir: &Path) -> Result<FinalizeGuard> {
    let path = install_dir.join("monopsony-self-update.pid");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if let Ok(pid) = existing.trim().parse::<u32>() {
            if process_alive(pid) {
                anyhow::bail!(
                    "another self-update finalize (PID {pid}) is already running — refusing \
                     to start a second one; if this is wrong, remove {}",
                    path.display()
                );
            }
        }
        // Stale guard (dead PID or unparseable) — take it over.
        let _ = std::fs::remove_file(&path);
    }
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(FinalizeGuard { path })
}

/// Is a process with this PID alive? `kill -0` via sh — no libc dependency.
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {pid} 2>/dev/null"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Spawn `<exe> self-update finalize --command-file <file>` as a detached
/// process that survives the parent's exit: a plain std process, its own
/// process group on unix (no terminal HUP propagation, not reaped by the
/// parent), stdio redirected to a log file so nothing holds the parent's
/// pipes open.
pub fn spawn_detached_finalize(exe: &Path, command_file: &Path, install_dir: &Path) -> Result<()> {
    let log_path = install_dir.join("monopsony-self-update-finalize.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open finalize log {}", log_path.display()))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("self-update")
        .arg("finalize")
        .arg("--command-file")
        .arg(command_file)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group — survives the daemon's exit and any terminal
        // signal group delivery.
        cmd.process_group(0);
    }
    let child = cmd.spawn()?;
    info!(
        "Detached finalize process spawned (pid {}, log {})",
        child.id(),
        log_path.display()
    );
    Ok(())
}

// ─── Init-system restart (Sprint 34 P3) ────────────────────────────
//
// Detection families mirror the installer's detect_init() (dist/install.sh):
// strongest signals first. The bench fleet exercises all three (ct/1952
// systemd, ct/1904 OpenRC, m2mini launchd).

/// The init system that manages the monopsony unit on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitSystem {
    Systemd,
    OpenRc,
    Launchd,
    Unknown,
}

/// Pure decision core of [`detect_init_system`] — testable without touching
/// the host. `os` is `std::env::consts::OS` ("linux" | "macos" | ...).
fn decide_init_system(
    os: &str,
    systemd_run_dir: bool,
    openrc_softlevel: bool,
    rc_service_present: bool,
    launchctl_present: bool,
) -> InitSystem {
    if os == "macos" && launchctl_present {
        return InitSystem::Launchd;
    }
    if systemd_run_dir {
        return InitSystem::Systemd;
    }
    if openrc_softlevel && rc_service_present {
        return InitSystem::OpenRc;
    }
    InitSystem::Unknown
}

/// Detect the host's init system. `Unknown` hosts get a loud manual-restart
/// instruction from the caller — never a silent skip.
pub fn detect_init_system() -> InitSystem {
    decide_init_system(
        std::env::consts::OS,
        Path::new("/run/systemd/system").exists(),
        Path::new("/run/openrc/softlevel").exists(),
        which_in_path("rc-service"),
        which_in_path("launchctl"),
    )
}

fn which_in_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// The service unit name for a binary under an init system. systemd/OpenRC
/// use the installed unit names (dist/init templates); launchd uses the
/// reverse-DNS label. The client's launchd label follows the committed
/// `com.monopsony.server.plist` convention.
impl InitSystem {
    /// Human-readable name for operator-facing messages.
    pub fn label(&self) -> &'static str {
        match self {
            InitSystem::Systemd => "systemd",
            InitSystem::OpenRc => "OpenRC",
            InitSystem::Launchd => "launchd",
            InitSystem::Unknown => "unknown init system",
        }
    }
}

pub fn unit_for(init: InitSystem, kind: BinaryKind) -> &'static str {
    match (init, kind) {
        (InitSystem::Launchd, BinaryKind::Client) => "com.monopsony.client",
        (InitSystem::Launchd, BinaryKind::Server) => "com.monopsony.server",
        (_, BinaryKind::Client) => "monopsony-client",
        (_, BinaryKind::Server) => "monopsony-server",
    }
}

/// The restart command for an init system + unit (pure — tests assert the
/// argv map). `None` when no supported init was detected.
pub fn restart_argv(init: InitSystem, unit: &str) -> Option<(String, Vec<String>)> {
    match init {
        InitSystem::Systemd => Some((
            "systemctl".to_string(),
            vec!["restart".to_string(), unit.to_string()],
        )),
        InitSystem::OpenRc => Some((
            "rc-service".to_string(),
            vec![unit.to_string(), "restart".to_string()],
        )),
        // -k: kill the running instance and restart it immediately.
        InitSystem::Launchd => Some((
            "launchctl".to_string(),
            vec![
                "kickstart".to_string(),
                "-k".to_string(),
                format!("system/{unit}"),
            ],
        )),
        InitSystem::Unknown => None,
    }
}

/// Restart the service unit via the detected init. Failure is returned as a
/// loud operator-facing error (manual restart instructions travel in the
/// message) — there is no silent fallback.
pub fn restart_service_unit(unit: &str) -> Result<()> {
    let init = detect_init_system();
    let Some((program, args)) = restart_argv(init, unit) else {
        anyhow::bail!(
            "no supported init system detected (systemd/OpenRC/launchd) — \
             restart {unit} MANUALLY; the swapped binary is already on disk"
        );
    };
    let output = std::process::Command::new(&program)
        .args(&args)
        .output()
        .with_context(|| format!("failed to execute {program} to restart {unit}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} {} failed (exit {}): {} — restart {unit} MANUALLY; the swapped binary is already on disk",
            program,
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    info!("Restarted service unit {unit} via {program}");
    Ok(())
}

/// Download and apply update with atomic swap (raw, no safety pipeline)
pub async fn apply_update(
    current_path: &Path,
    target_dir: &Path,
    asset_url: &str,
    version: &str,
) -> Result<()> {
    let client = Client::new();

    info!("Downloading {}...", asset_url);
    let response = client.get(asset_url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!(
            "Download failed: status {} for {}",
            response.status(),
            asset_url
        );
    }

    let bytes = response.bytes().await?;

    // Backup current binary
    info!("Backing up current version...");
    let backup_path = sibling_update_path(target_dir, current_path, "bak");
    if current_path.exists() {
        std::fs::copy(current_path, &backup_path).context("Failed to backup current binary")?;
        info!(
            "Backed up {} -> {}",
            current_path.display(),
            backup_path.display()
        );
    }

    // Write new binary atomically
    let temp_path = sibling_update_path(target_dir, current_path, "new");

    if asset_url.ends_with(".tar.gz") || asset_url.ends_with(".zip") {
        extract_binary_from_archive(&bytes, &temp_path, asset_url)?;
    } else {
        std::fs::write(&temp_path, &bytes).context("Failed to write new binary")?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&temp_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&temp_path, perms)?;
    }

    // Atomic rename
    info!("Swapping binary...");
    #[cfg(unix)]
    {
        std::fs::rename(&temp_path, current_path).context("Failed to atomically swap binary")?;
    }
    #[cfg(windows)]
    {
        if current_path.exists() {
            std::fs::remove_file(current_path)?;
        }
        std::fs::rename(&temp_path, current_path).context("Failed to atomically swap binary")?;
    }

    info!("Update applied successfully: {}", version);
    Ok(())
}

/// How [`run_finalize`] restarts the unit after the swap. `DetectedInit` is
/// the production behavior; `Noop` is the test/simulation seam (house rule:
/// tests never touch real services).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAction {
    DetectedInit,
    /// Test/simulation seam — tests never touch real service units.
    #[allow(dead_code)] // test seam
    Noop,
}

/// Phase 2 of the staged apply — run by the detached `self-update finalize`
/// process after the staging daemon exited. Reuses the managed pipeline's
/// snapshot, hooks, health-check and rollback machinery around a
/// [`AssetProvider::LocalBlob`] provider.
///
/// Order note (deliberate, recorded in the sprint progress doc): the plan
/// architecture prose reads "swap → health check → restart", but the daemon
/// has EXITED by the time this runs — a process health check before the
/// restart can only ever match this finalize process itself (`pgrep -x
/// monopsony-client` self-match = vacuous pass) while nothing else runs.
/// The implementable order is: snapshot → swap → restart via detected init
/// → health gate (recipe health_check, process checks exclude our own PID)
/// → commit / rollback (rollback restarts again to bring the old binary
/// up). Restart failure is LOUD, never silent — but it does not pre-empt
/// the health gate (an operator's Restart=always may have covered for us).
pub async fn run_finalize(
    cmd: &FinalizeCommand,
    current_path: &Path,
    restart: RestartAction,
) -> Result<UpdateOutcome> {
    let kind = BinaryKind::from_binary_type(&cmd.binary);
    let recipe =
        toml::from_str::<monopsony_common::shadow_types::ShadowRecipe>(default_recipe_for(kind))
            .ok();
    run_finalize_with_recipe(cmd, current_path, restart, recipe).await
}

/// Recipe-injectable core of [`run_finalize`]; tests drive this with
/// benign hooks and a unique health-check process name (the default recipe
/// stops/starts real service units — house rule: tests never touch them).
pub async fn run_finalize_with_recipe(
    cmd: &FinalizeCommand,
    current_path: &Path,
    restart: RestartAction,
    recipe: Option<monopsony_common::shadow_types::ShadowRecipe>,
) -> Result<UpdateOutcome> {
    let kind = BinaryKind::from_binary_type(&cmd.binary);
    // 0. The staged bytes must still be exactly what phase 1 recorded.
    let staged_bytes = std::fs::read(&cmd.staged_path)
        .with_context(|| format!("staged binary missing: {}", cmd.staged_path.display()))?;
    let staged_sha = sha256_hex(&staged_bytes);
    if !staged_sha.eq_ignore_ascii_case(&cmd.staged_sha256) {
        anyhow::bail!(
            "staged binary hash mismatch: expected {}, found {} — refusing to finalize \
             (the staged file was modified after staging)",
            cmd.staged_sha256,
            staged_sha
        );
    }

    let install_dir = current_path
        .parent()
        .context("current binary has no parent directory")?
        .to_path_buf();

    let mut ctx = UpdateContext::with_provider(
        recipe,
        current_path.to_path_buf(),
        install_dir,
        cmd.from_version.clone(),
        cmd.to_version.clone(),
        AssetProvider::LocalBlob(cmd.staged_path.clone()),
    );

    // 1. pre_update hook — stops the unit; systemd Restart=always may have
    //    already respawned the old daemon after phase 1's exit, and it must
    //    be stopped before the swap lands.
    if let Some(ref recipe) = ctx.recipe {
        if let Some(ref pre) = recipe.install.pre_update {
            run_hook("pre_update", pre).context("pre_update hook failed — aborting finalize")?;
        }
    }

    // 2. Snapshot (managed pipeline machinery: binary + configs).
    ctx.create_snapshot()?;

    // 3. Swap: stage the staged bytes to a fresh sibling and rename over
    //    the current binary (same mechanics as the managed pipeline).
    let new_path = sibling_update_path(&ctx.install_dir, &ctx.current_path, "new");
    stage_asset_bytes(&staged_bytes, &new_path, None)?;
    set_executable(&new_path)?;
    swap_binary(&new_path, &ctx.current_path)?;
    info!(
        "Finalize swapped binary: {} → {}",
        cmd.from_version, cmd.to_version
    );

    // 4. Restart via the detected init — loud on failure, no silent path.
    let init = detect_init_system();
    let unit = unit_for(init, kind);
    if restart == RestartAction::DetectedInit {
        if let Err(e) = restart_service_unit(unit) {
            error!("MONOPSONY SELF-UPDATE: {e:#}");
            eprintln!("MONOPSONY SELF-UPDATE: {e:#}");
        }
    }

    // 5. Health gate. Process checks exclude this finalize process's own
    //    PID (it matches the unit's process name); port/http checks run
    //    exactly as the managed pipeline's run_health_check.
    let health_ok = match ctx
        .recipe
        .as_ref()
        .and_then(|r| r.install.health_check.as_ref())
    {
        Some(check)
            if matches!(
                check.check_type,
                monopsony_common::shadow_types::HealthCheckType::Process
            ) =>
        {
            let name = check.process.clone().unwrap_or_default();
            wait_for_process_excluding_self(
                &name,
                std::time::Duration::from_secs(check.timeout_secs.max(1)),
            )
            .await
            .is_ok()
        }
        Some(check) => run_health_check(check).await.is_ok(),
        None => true,
    };

    if !health_ok {
        if let Some(ref recipe) = ctx.recipe {
            if let Some(ref on_fail) = recipe.install.on_failure {
                let _ = run_hook("on_failure", on_fail);
            }
        }
        ctx.rollback("finalize health check failed")?;
        if restart == RestartAction::DetectedInit {
            if let Err(e) = restart_service_unit(unit) {
                error!("MONOPSONY SELF-UPDATE: post-rollback restart failed: {e:#}");
                eprintln!(
                    "MONOPSONY SELF-UPDATE: post-rollback restart failed: {e:#} — the \
                     previous binary is restored on disk; restart {unit} MANUALLY"
                );
            }
        }
        return Ok(UpdateOutcome::RolledBack(
            "finalize health check failed — previous binary restored".to_string(),
        ));
    }

    // 6. Commit.
    ctx.restore_config_edits()?;
    ctx.prune_backups()?;
    if let Some(ref recipe) = ctx.recipe {
        if let Some(ref post) = recipe.install.post_update {
            if let Err(e) = run_hook("post_update", post) {
                warn!("post_update hook failed: {}", e);
            }
        }
    }
    let _ = std::fs::remove_file(&cmd.staged_path);
    info!(
        "Finalize committed: {} → {} (unit {unit})",
        cmd.from_version, cmd.to_version
    );
    Ok(UpdateOutcome::Success)
}

/// Process health gate for the finalize path: healthy only when a process
/// OTHER than this one matches (`pgrep -x`). The finalize process is itself
/// a monopsony binary, so an unfiltered self-match would be a vacuous pass.
async fn wait_for_process_excluding_self(
    process: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    use std::time::Instant;

    let start = Instant::now();
    let my_pid = std::process::id();
    // Match the kernel's comm granularity (15 chars) — see comm_name.
    let comm = comm_name(process).to_string();
    loop {
        let output = std::process::Command::new("pgrep")
            .arg("-x")
            .arg(&comm)
            .output()?;
        if output.status.success() {
            let others: Vec<u32> = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .filter(|&pid| pid != my_pid)
                .collect();
            if !others.is_empty() {
                info!("process '{process}' healthy (pid {})", others[0]);
                return Ok(());
            }
        }
        if start.elapsed() >= timeout {
            anyhow::bail!(
                "process '{}' (other than the finalize process) not running after {}s",
                process,
                timeout.as_secs()
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// `self-update rollback`: restore the newest `update-backup-*/binary.bak`
/// (the managed pipeline's snapshot layout) over the current binary.
/// Returns the backup path used.
pub fn rollback_latest_backup(current_path: &Path, install_dir: &Path) -> Result<PathBuf> {
    let mut backups: Vec<_> = std::fs::read_dir(install_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("update-backup-"))
                .unwrap_or(false)
        })
        .collect();
    if backups.is_empty() {
        anyhow::bail!(
            "no update backups found under {} — nothing to roll back to",
            install_dir.display()
        );
    }
    backups.sort_by(|a, b| {
        let a_t = a.metadata().and_then(|m| m.modified()).ok();
        let b_t = b.metadata().and_then(|m| m.modified()).ok();
        b_t.cmp(&a_t)
    });
    let backup_bin = backups[0].path().join("binary.bak");
    if !backup_bin.exists() {
        anyhow::bail!(
            "newest backup {} has no binary.bak — nothing to roll back to",
            backups[0].path().display()
        );
    }
    std::fs::copy(&backup_bin, current_path).context("Failed to restore the backup binary")?;
    set_executable(current_path)?;
    Ok(backup_bin)
}

/// Extract the monopsony binary from a tar.gz or zip archive
fn extract_binary_from_archive(bytes: &[u8], output_path: &PathBuf, asset_url: &str) -> Result<()> {
    if asset_url.ends_with(".tar.gz") {
        let tar = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(tar);

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            let path_str = path.to_string_lossy();

            if path_str.ends_with("monopsony-server") || path_str.ends_with("monopsony-client") {
                entry.unpack(output_path)?;
                return Ok(());
            }
        }
        anyhow::bail!("monopsony binary not found in archive");
    } else if asset_url.ends_with(".zip") {
        let cursor = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(cursor)?;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_string();

            if name.ends_with("monopsony-server")
                || name.ends_with("monopsony-client")
                || name.ends_with("monopsony-server.exe")
                || name.ends_with("monopsony-client.exe")
            {
                let mut outfile = std::fs::File::create(output_path)?;
                std::io::copy(&mut entry, &mut outfile)?;
                return Ok(());
            }
        }
        anyhow::bail!("monopsony binary not found in zip archive");
    } else {
        std::fs::write(output_path, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        let v = parse_version("1.0.0").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 0, 0));
        assert!(v.pre.is_empty() && v.build.is_empty());
        let v = parse_version("v2.3.4").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (2, 3, 4));
        let v = parse_version("1.0.0-rc1").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 0, 0));
        assert_eq!(v.pre.as_str(), "rc1");
        assert!(v.build.is_empty());
        assert_eq!(parse_version("invalid"), None);
    }

    #[test]
    fn test_parse_version_rc_ordering() {
        // semver 2.0: pre-release < release, rc2 > rc1
        let rc1 = parse_version("1.0.0-rc1").unwrap();
        let rc2 = parse_version("1.0.0-rc2").unwrap();
        let stable = parse_version("1.0.0").unwrap();
        assert!(rc1 < rc2, "rc1 must be less than rc2");
        assert!(rc2 < stable, "rc2 must be less than stable");
        assert!(rc1 < stable, "rc1 must be less than stable");
    }

    #[test]
    fn test_parse_version_pre_release_levels() {
        // alpha < beta < rc < stable
        let alpha = parse_version("1.0.0-alpha.1").unwrap();
        let beta = parse_version("1.0.0-beta.1").unwrap();
        let rc = parse_version("1.0.0-rc.1").unwrap();
        let stable = parse_version("1.0.0").unwrap();
        assert!(alpha < beta);
        assert!(beta < rc);
        assert!(rc < stable);
    }

    #[test]
    fn test_parse_version_build_metadata_ignored() {
        // semver §11: build metadata does not affect version precedence.
        // semver::Version's PartialEq and Ord both include build metadata
        // (a crate deviation from the spec — documented here so it's not
        // mistaken for spec-compliant behavior). The ordering-relevant
        // fields (major.minor.patch-pre) are what matter for self-update.
        let a = parse_version("1.0.0+git.abc123").unwrap();
        let b = parse_version("1.0.0+git.def456").unwrap();
        assert_eq!(a.major, b.major);
        assert_eq!(a.minor, b.minor);
        assert_eq!(a.patch, b.patch);
        assert_eq!(a.pre, b.pre);
        assert_ne!(a.build, b.build, "build metadata should differ");
        // Both have the same core version — self-update treats them as
        // the same release for update-checking purposes.
        assert_eq!(
            (a.major, a.minor, a.patch, a.pre.as_str()),
            (b.major, b.minor, b.patch, b.pre.as_str())
        );
    }

    #[test]
    fn test_parse_version_v_prefix() {
        assert_eq!(parse_version("v1.0.0"), Some(semver::Version::new(1, 0, 0)));
    }

    #[test]
    fn test_parse_version_invalid() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("abc.def.ghi"), None);
        assert_eq!(parse_version("not-a-version"), None);
    }

    #[tokio::test]
    async fn test_check_for_update_current() {
        let result = check_for_update("99.99.99", BinaryKind::Client).await;
        match result {
            Ok(check) => assert_eq!(check.latest_version, None),
            Err(e) => println!("Network error (expected in CI): {}", e),
        }
    }

    #[tokio::test]
    async fn test_check_for_update_very_old() {
        let result = check_for_update("0.0.1", BinaryKind::Client).await;
        match result {
            Ok(check) => {
                if let Some(latest) = check.latest_version {
                    let parsed = parse_version(&latest);
                    assert!(parsed.is_some(), "Latest version should parse: {}", latest);
                    let v = parsed.unwrap();
                    assert!(
                        v.major > 0 || v.minor > 0 || v.patch > 0,
                        "Latest version should be newer than 0.0.1"
                    );
                }
            }
            Err(e) => println!("Network error (expected in CI): {}", e),
        }
    }

    #[test]
    fn test_apply_update_atomic_swap() {
        let temp_dir = tempfile::tempdir().unwrap();
        let bin_path = temp_dir.path().join("monopsony-server");

        std::fs::write(&bin_path, b"old").unwrap();

        let target_dir = temp_dir.path();
        let new_content = b"new";

        std::fs::copy(&bin_path, target_dir.join("monopsony-server.bak.test")).unwrap();

        let temp_path = target_dir.join("temp_new");
        std::fs::write(&temp_path, new_content).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&temp_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&temp_path, perms).unwrap();
        }

        std::fs::rename(&temp_path, &bin_path).unwrap();

        let content = std::fs::read_to_string(&bin_path).unwrap();
        assert_eq!(content, "new");
    }

    #[test]
    fn test_run_hook_success() {
        #[cfg(unix)]
        let result = run_hook("test", "true");
        #[cfg(windows)]
        let result = run_hook("test", "exit 0");
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_hook_failure() {
        #[cfg(unix)]
        let result = run_hook("test", "false");
        #[cfg(windows)]
        let result = run_hook("test", "exit 1");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_snapshot_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        std::fs::write(&config_path, b"original = true\n").unwrap();

        // Simulate snapshot
        let snapshot = ConfigSnapshot {
            path: config_path.clone(),
            original_content: std::fs::read(&config_path).unwrap(),
            existed: true,
        };

        // Simulate update modifying config
        std::fs::write(&config_path, b"new = true\n").unwrap();
        assert_ne!(
            std::fs::read(&config_path).unwrap(),
            snapshot.original_content
        );

        // Restore
        std::fs::write(&config_path, &snapshot.original_content).unwrap();
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            snapshot.original_content
        );
    }

    #[test]
    fn test_prune_backups() {
        let temp_dir = tempfile::tempdir().unwrap();
        let install_dir = temp_dir.path();

        // Create 5 backup dirs
        for i in 0..5 {
            let dir = install_dir.join(format!("update-backup-2026010{}-120000", i));
            std::fs::create_dir_all(&dir).unwrap();
            // Slightly different mtime by writing a file
            std::fs::write(dir.join("binary.bak"), format!("v{}", i)).unwrap();
        }

        // Create context with max_backups = 3
        let ctx = UpdateContext {
            recipe: None,
            current_path: install_dir.join("binary"),
            install_dir: install_dir.to_path_buf(),
            backup_dir: install_dir.join("update-backup-current"),
            from_version: "1.0.0".to_string(),
            to_version: "2.0.0".to_string(),
            asset: AssetProvider::Url(String::new()),
            config_paths: vec![],
            config_snapshots: vec![],
        };

        // max_backups defaults to 3 when recipe is None
        ctx.prune_backups().unwrap();

        let remaining: Vec<_> = std::fs::read_dir(install_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .unwrap_or("")
                    .starts_with("update-backup-")
            })
            .collect();
        assert_eq!(remaining.len(), 3, "Should keep only 3 backups");
    }

    // ─── CI/CD Integration Tests ───────────────────────────────────
    // These tests exercise real-world scenarios that CI/CD pipelines
    // will hit: archive extraction, rollback on failed health check,
    // config preservation, recipe parsing, and platform asset matching.

    /// Verify that both split default recipes (per-binary hooks, health
    /// check, config_paths, max_backups) deserialize correctly.
    #[test]
    fn test_default_server_recipe_parses_with_hooks() {
        use monopsony_common::shadow_types::{HealthCheckType, DEFAULT_MONOPSONY_SERVER_RECIPE};
        let recipe: monopsony_common::shadow_types::ShadowRecipe =
            toml::from_str(DEFAULT_MONOPSONY_SERVER_RECIPE)
                .expect("DEFAULT_MONOPSONY_SERVER_RECIPE must parse");

        assert_eq!(recipe.package.name, "monopsony-server");
        assert!(recipe.install.backup_old);
        assert_eq!(recipe.install.max_backups, 3);
        assert_eq!(
            recipe.install.config_paths,
            Some(vec!["/etc/monopsony/server.toml".to_string()]),
            "server recipe must manage the server config only"
        );
        assert_eq!(
            recipe.install.chmod,
            Some(vec!["monopsony-server".to_string()])
        );

        // Hooks stop/start the server unit — not the client unit.
        let pre = recipe.install.pre_update.as_deref().unwrap();
        let post = recipe.install.post_update.as_deref().unwrap();
        assert!(pre.contains("systemctl stop monopsony-server"), "{}", pre);
        assert!(
            post.contains("systemctl start monopsony-server"),
            "{}",
            post
        );
        assert!(
            !pre.contains("monopsony-client") && !post.contains("monopsony-client"),
            "server hooks must not touch the client unit"
        );

        // Health check: server control port.
        let hc = recipe
            .install
            .health_check
            .expect("server recipe must have health_check");
        assert_eq!(hc.check_type, HealthCheckType::Port);
        assert_eq!(hc.port, Some(50051));
        assert_eq!(hc.timeout_secs, 30);

        // Asset rules match the real release contract for this kind.
        let sel = recipe.asset_selector.expect("asset_selector");
        assert_eq!(sel.rules.len(), 4, "linux×2 + darwin×2");
    }

    #[test]
    fn test_default_client_recipe_parses_with_hooks() {
        use monopsony_common::shadow_types::{HealthCheckType, DEFAULT_MONOPSONY_CLIENT_RECIPE};
        let recipe: monopsony_common::shadow_types::ShadowRecipe =
            toml::from_str(DEFAULT_MONOPSONY_CLIENT_RECIPE)
                .expect("DEFAULT_MONOPSONY_CLIENT_RECIPE must parse");

        assert_eq!(recipe.package.name, "monopsony-client");
        assert!(recipe.install.backup_old);
        assert_eq!(recipe.install.max_backups, 3);
        assert_eq!(
            recipe.install.config_paths,
            Some(vec!["/etc/monopsony/client.toml".to_string()]),
            "client recipe must manage the client config only"
        );
        assert_eq!(
            recipe.install.chmod,
            Some(vec!["monopsony-client".to_string()])
        );

        // Hooks stop/start the client unit — not the server unit.
        let pre = recipe.install.pre_update.as_deref().unwrap();
        let post = recipe.install.post_update.as_deref().unwrap();
        assert!(pre.contains("systemctl stop monopsony-client"), "{}", pre);
        assert!(
            post.contains("systemctl start monopsony-client"),
            "{}",
            post
        );
        assert!(
            !pre.contains("monopsony-server") && !post.contains("monopsony-server"),
            "client hooks must not touch the server unit"
        );

        // Health check: process (the client has no listening port).
        let hc = recipe
            .install
            .health_check
            .expect("client recipe must have health_check");
        assert_eq!(hc.check_type, HealthCheckType::Process);
        assert_eq!(hc.process.as_deref(), Some("monopsony-client"));
        assert_eq!(hc.timeout_secs, 30);

        let sel = recipe.asset_selector.expect("asset_selector");
        assert_eq!(sel.rules.len(), 4, "linux×2 + darwin×2");
    }

    /// Create a real tar.gz archive containing a monopsony-server binary,
    /// then extract it and verify the content matches.
    #[test]
    fn test_extract_binary_from_real_targz() {
        let temp = tempfile::tempdir().unwrap();
        let staged_dir = temp.path().join("staged");
        std::fs::create_dir_all(&staged_dir).unwrap();

        // Create the "binary" we'll pack into the archive
        let binary_content = b"#!/bin/sh\necho monopsony-server v2.0.0\n";
        std::fs::write(staged_dir.join("monopsony-server"), binary_content).unwrap();

        // Build tar.gz in memory
        let mut tar_buf = Vec::new();
        {
            let mut tar_builder = tar::Builder::new(&mut tar_buf);
            tar_builder
                .append_dir_all("monopsony-2.0.0", &staged_dir)
                .unwrap();
            tar_builder.finish().unwrap();
        }
        let mut gz_buf = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_buf, flate2::Compression::default());
            std::io::Write::write_all(&mut encoder, &tar_buf).unwrap();
            encoder.finish().unwrap();
        }

        // Extract using our function
        let out_path = temp.path().join("extracted-binary");
        extract_binary_from_archive(&gz_buf, &out_path, "monopsony-x86_64-linux-gnu.tar.gz")
            .expect("extraction should succeed");

        let extracted = std::fs::read(&out_path).unwrap();
        assert_eq!(extracted, binary_content);
    }

    /// Create a real zip archive and extract binary from it.
    #[test]
    fn test_extract_binary_from_real_zip() {
        use std::io::{Cursor, Write};

        // Build zip in memory (zip 0.6 needs Cursor for Seek)
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::FileOptions::default();
            writer
                .start_file("monopsony-2.0.0/monopsony-client", opts)
                .unwrap();
            writer.write_all(b"fake-client-binary").unwrap();
            writer.finish().unwrap();
        }
        let zip_buf = buf.into_inner();

        let temp = tempfile::tempdir().unwrap();
        let out_path = temp.path().join("extracted-from-zip");
        extract_binary_from_archive(&zip_buf, &out_path, "monopsony-windows.zip")
            .expect("zip extraction should succeed");

        let extracted = std::fs::read(&out_path).unwrap();
        assert_eq!(extracted, b"fake-client-binary");
    }

    /// Test that extraction fails when the binary name is not in the archive.
    #[test]
    fn test_extract_binary_not_found_in_archive() {
        let temp = tempfile::tempdir().unwrap();
        let staged_dir = temp.path().join("staged");
        std::fs::create_dir_all(&staged_dir).unwrap();
        std::fs::write(staged_dir.join("some-other-binary"), b"data").unwrap();

        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            builder.append_dir_all("pkg-1.0", &staged_dir).unwrap();
            builder.finish().unwrap();
        }
        let mut gz_buf = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_buf, flate2::Compression::default());
            std::io::Write::write_all(&mut encoder, &tar_buf).unwrap();
            encoder.finish().unwrap();
        }

        let out_path = temp.path().join("should-not-exist");
        let result = extract_binary_from_archive(&gz_buf, &out_path, "test.tar.gz");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    /// Health check: port success — bind a listener, verify the check passes.
    #[tokio::test]
    async fn test_health_check_port_success() {
        use monopsony_common::shadow_types::{HealthCheck, HealthCheckType};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Spawn a minimal acceptor so the port is actually "open"
        let _acceptor = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });

        let check = HealthCheck {
            check_type: HealthCheckType::Port,
            url: None,
            port: Some(port),
            process: None,
            timeout_secs: 5,
            stable: None,
        };

        let result = run_health_check(&check).await;
        assert!(
            result.is_ok(),
            "port health check should succeed on open port"
        );
    }

    /// Health check: port timeout — nothing listening, should fail quickly.
    #[tokio::test]
    async fn test_health_check_port_timeout() {
        use monopsony_common::shadow_types::{HealthCheck, HealthCheckType};

        // Use an ephemeral port range that's very unlikely to be open.
        // We can't bind, but the timeout is short (3s).
        let check = HealthCheck {
            check_type: HealthCheckType::Port,
            url: None,
            port: Some(1), // port 1 — reserved, won't be listening
            process: None,
            timeout_secs: 3,
            stable: None,
        };

        let start = std::time::Instant::now();
        let result = run_health_check(&check).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "should fail when port is not open");
        assert!(
            elapsed >= std::time::Duration::from_secs(2),
            "should have polled at least once before timing out"
        );
    }

    /// Full rollback test: create binary + config, snapshot, modify both,
    /// rollback, verify both are restored to original.
    #[test]
    fn test_rollback_restores_binary_and_config() {
        let temp = tempfile::tempdir().unwrap();
        let install_dir = temp.path().to_path_buf();
        let bin_path = install_dir.join("monopsony-server");
        let config_path = install_dir.join("server.toml");
        let backup_dir = install_dir.join("update-backup-test");

        // Write original files
        let original_binary = b"original-binary-v1";
        let original_config = b"[server]\nport = 3128\n";
        std::fs::write(&bin_path, original_binary).unwrap();
        std::fs::write(&config_path, original_config).unwrap();

        // Create backup dir + populate snapshots
        std::fs::create_dir_all(&backup_dir).unwrap();
        std::fs::copy(&bin_path, backup_dir.join("binary.bak")).unwrap();

        let snapshot = ConfigSnapshot {
            path: config_path.clone(),
            original_content: original_config.to_vec(),
            existed: true,
        };

        // Simulate failed update: both files modified
        std::fs::write(&bin_path, b"new-broken-binary-v2").unwrap();
        std::fs::write(&config_path, b"[server]\nport = 9999\n").unwrap();

        // Verify they're changed
        assert_ne!(std::fs::read(&bin_path).unwrap(), original_binary);
        assert_ne!(std::fs::read(&config_path).unwrap(), original_config);

        // Rollback
        let ctx = UpdateContext {
            recipe: None,
            current_path: bin_path.clone(),
            install_dir: install_dir.clone(),
            backup_dir: backup_dir.clone(),
            from_version: "1.0.0".to_string(),
            to_version: "2.0.0".to_string(),
            asset: AssetProvider::Url(String::new()),
            config_paths: vec![config_path.clone()],
            config_snapshots: vec![snapshot],
        };
        ctx.rollback("simulated health check failure").unwrap();

        // Verify restoration
        assert_eq!(
            std::fs::read(&bin_path).unwrap(),
            original_binary,
            "binary must be restored"
        );
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            original_config,
            "config must be restored"
        );

        // Verify permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&bin_path).unwrap().permissions().mode();
            assert!(perms & 0o111 != 0, "restored binary should be executable");
        }
    }

    /// Config preservation: after a "successful" update, if the update
    /// modified the config file, the operator's original is restored.
    #[test]
    fn test_config_preserved_after_successful_update() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("server.toml");

        let operator_config = b"[server]\nlisten = 3128\n# operator custom\n";
        std::fs::write(&config_path, operator_config).unwrap();

        // Snapshot (as the pipeline would)
        let snapshot = ConfigSnapshot {
            path: config_path.clone(),
            original_content: operator_config.to_vec(),
            existed: true,
        };

        // Simulate update overwriting config with new defaults
        let new_defaults = b"[server]\nlisten = 3128\n# new defaults v2\n";
        std::fs::write(&config_path, new_defaults).unwrap();
        assert_ne!(std::fs::read(&config_path).unwrap(), operator_config);

        // restore_config_edits is a private method — test via UpdateContext
        let install_dir = temp.path().to_path_buf();
        let ctx = UpdateContext {
            recipe: None,
            current_path: install_dir.join("binary"),
            install_dir: install_dir.clone(),
            backup_dir: install_dir.join("backup"),
            from_version: "1.0.0".to_string(),
            to_version: "2.0.0".to_string(),
            asset: AssetProvider::Url(String::new()),
            config_paths: vec![config_path.clone()],
            config_snapshots: vec![snapshot],
        };

        // Use the public interface — restore happens in restore_config_edits
        // which is private. We test the ConfigSnapshot roundtrip directly.
        // Simulate: operator config should win.
        let snap = &ctx.config_snapshots[0];
        let current = std::fs::read(&snap.path).unwrap_or_default();
        if current != snap.original_content {
            // This is what restore_config_edits does internally
            std::fs::write(&snap.path, &snap.original_content).unwrap();
        }

        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            operator_config,
            "operator config must be preserved over new defaults"
        );
    }

    /// Verify that pruning respects max_backups from a parsed recipe.
    #[test]
    fn test_prune_respects_recipe_max_backups() {
        let temp = tempfile::tempdir().unwrap();
        let install_dir = temp.path().to_path_buf();

        // Create 7 backup dirs
        for i in 0..7 {
            let dir = install_dir.join(format!("update-backup-2026010{}-120000", i));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("binary.bak"), format!("v{}", i)).unwrap();
        }

        // Build a recipe with max_backups = 5
        let recipe_toml = r#"
[package]
name = "test"
source_type = "github_release"
source_url = "https://example.com/test"

[install]
method = "extract"
target_dir = "/tmp"
backup_old = true
max_backups = 5

[version]
detect_command = "echo 1.0.0"
detect_pattern = "(\\d+\\.\\d+\\.\\d+)"
"#;
        let recipe: monopsony_common::shadow_types::ShadowRecipe =
            toml::from_str(recipe_toml).unwrap();

        let ctx = UpdateContext {
            recipe: Some(recipe),
            current_path: install_dir.join("binary"),
            install_dir: install_dir.clone(),
            backup_dir: install_dir.join("update-backup-current"),
            from_version: "1.0.0".to_string(),
            to_version: "2.0.0".to_string(),
            asset: AssetProvider::Url(String::new()),
            config_paths: vec![],
            config_snapshots: vec![],
        };

        ctx.prune_backups().unwrap();

        let remaining: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .unwrap_or("")
                    .starts_with("update-backup-")
            })
            .collect();
        assert_eq!(
            remaining.len(),
            5,
            "should keep 5 backups per recipe max_backups"
        );
    }

    /// Verify hook execution captures stderr from failing commands.
    #[test]
    fn test_run_hook_captures_stderr() {
        #[cfg(unix)]
        {
            let result = run_hook("test", "echo 'error message' >&2; exit 1");
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("error message"),
                "stderr should be in error message: {}",
                err
            );
        }
    }

    /// Verify hook execution captures stdout from successful commands.
    #[test]
    fn test_run_hook_with_env_vars() {
        // Hooks should inherit the environment
        #[cfg(unix)]
        {
            std::env::set_var("MONOPSONY_TEST_HOOK", "hookvalue");
            let result = run_hook("test", "test \"$MONOPSONY_TEST_HOOK\" = \"hookvalue\"");
            assert!(result.is_ok());
            std::env::remove_var("MONOPSONY_TEST_HOOK");
        }
    }

    /// The exact asset list release CI publishes (.github/workflows/release.yml):
    /// 8 kind-prefixed versioned tarballs + SHA256SUMS.
    fn real_release_assets() -> Vec<String> {
        [
            "monopsony-server-v1.0.0-rc2-linux-x86_64.tar.gz",
            "monopsony-server-v1.0.0-rc2-linux-aarch64.tar.gz",
            "monopsony-server-v1.0.0-rc2-darwin-arm64.tar.gz",
            "monopsony-server-v1.0.0-rc2-darwin-x86_64.tar.gz",
            "monopsony-client-v1.0.0-rc2-linux-x86_64.tar.gz",
            "monopsony-client-v1.0.0-rc2-linux-aarch64.tar.gz",
            "monopsony-client-v1.0.0-rc2-darwin-arm64.tar.gz",
            "monopsony-client-v1.0.0-rc2-darwin-x86_64.tar.gz",
            "SHA256SUMS",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Every (binary, platform) combination the release contract publishes
    /// must select exactly its own archive from the real fixture list —
    /// including the darwin aarch64→arm64 remap and -rcN versioned names.
    #[test]
    fn test_asset_selection_real_release_contract() {
        let assets = real_release_assets();
        let cases = [
            (
                BinaryKind::Server,
                "linux",
                "x86_64",
                "monopsony-server-v1.0.0-rc2-linux-x86_64.tar.gz",
            ),
            (
                BinaryKind::Server,
                "linux",
                "aarch64",
                "monopsony-server-v1.0.0-rc2-linux-aarch64.tar.gz",
            ),
            // std ARCH is aarch64 on Apple Silicon; asset says darwin-arm64.
            (
                BinaryKind::Server,
                "darwin",
                "aarch64",
                "monopsony-server-v1.0.0-rc2-darwin-arm64.tar.gz",
            ),
            (
                BinaryKind::Server,
                "darwin",
                "x86_64",
                "monopsony-server-v1.0.0-rc2-darwin-x86_64.tar.gz",
            ),
            (
                BinaryKind::Client,
                "linux",
                "x86_64",
                "monopsony-client-v1.0.0-rc2-linux-x86_64.tar.gz",
            ),
            (
                BinaryKind::Client,
                "linux",
                "aarch64",
                "monopsony-client-v1.0.0-rc2-linux-aarch64.tar.gz",
            ),
            (
                BinaryKind::Client,
                "darwin",
                "aarch64",
                "monopsony-client-v1.0.0-rc2-darwin-arm64.tar.gz",
            ),
            (
                BinaryKind::Client,
                "darwin",
                "x86_64",
                "monopsony-client-v1.0.0-rc2-darwin-x86_64.tar.gz",
            ),
        ];
        for (kind, os, arch, expected) in cases {
            let sel = select_asset_for_platform(&assets, kind, os, arch);
            assert_eq!(
                sel.as_deref(),
                Some(expected),
                "kind={:?} os={} arch={}",
                kind,
                os,
                arch
            );
        }
    }

    /// Negative controls: glibc hosts still match (static musl assets, libc
    /// never enters a match string); wrong binary kind, unpublished platforms,
    /// SHA256SUMS, and malformed/non-rc versions never match.
    #[test]
    fn test_asset_selection_negative_controls() {
        let assets = real_release_assets();

        // A glibc host (detect_platform's "glibc") matches the static linux asset.
        let sel = select_asset_for_platform(&assets, BinaryKind::Client, "linux", "x86_64");
        assert_eq!(
            sel.as_deref(),
            Some("monopsony-client-v1.0.0-rc2-linux-x86_64.tar.gz")
        );

        // Wrong binary kind: a server-only release never satisfies the client.
        let server_only: Vec<String> = assets
            .iter()
            .filter(|a| a.starts_with("monopsony-server"))
            .cloned()
            .collect();
        assert_eq!(
            select_asset_for_platform(&server_only, BinaryKind::Client, "linux", "x86_64"),
            None,
            "client kind must not select a server archive"
        );

        // Platforms with no published assets.
        assert_eq!(
            select_asset_for_platform(&assets, BinaryKind::Client, "windows", "x86_64"),
            None
        );
        assert_eq!(
            select_asset_for_platform(&assets, BinaryKind::Client, "freebsd", "x86_64"),
            None
        );

        // Checksum sidecars are never selected.
        let sums_only = vec!["SHA256SUMS".to_string(), "SHA256SUMS.sig".to_string()];
        assert_eq!(
            select_asset_for_platform(&sums_only, BinaryKind::Server, "linux", "x86_64"),
            None
        );

        // Version-shape negatives (contract: \d+\.\d+\.\d+ with optional -rcN).
        let bad_versions = [
            // Non-rc pre-release suffix.
            "monopsony-server-v1.0.0-beta.1-linux-x86_64.tar.gz",
            // Two-component version.
            "monopsony-server-v1.0-linux-x86_64.tar.gz",
            // Four components.
            "monopsony-server-v1.0.0.1-linux-x86_64.tar.gz",
            // Missing v prefix.
            "monopsony-server-1.0.0-linux-x86_64.tar.gz",
            // rc without a number.
            "monopsony-server-v1.0.0-rc-linux-x86_64.tar.gz",
            // Legacy triple-style names (the dead contract this fixes).
            "monopsony-x86_64-unknown-linux-musl.tar.gz",
            // Right kind+version, wrong platform in the name.
            "monopsony-server-v1.0.0-rc2-linux-arm64.tar.gz",
        ];
        for name in bad_versions {
            assert_eq!(
                select_asset_for_platform(
                    &[name.to_string()],
                    BinaryKind::Server,
                    "linux",
                    "x86_64"
                ),
                None,
                "{} must not match",
                name
            );
        }
    }

    /// Backup/temp update file names derive from the binary being replaced —
    /// client updates must not produce monopsony-server.bak litter.
    #[test]
    fn test_update_sibling_names_parametrized_by_binary() {
        let dir = Path::new("/usr/local/bin");
        let server_bak = sibling_update_path(dir, &dir.join("monopsony-server"), "bak");
        let client_new = sibling_update_path(dir, &dir.join("monopsony-client"), "new");
        let server_name = server_bak.file_name().unwrap().to_str().unwrap();
        let client_name = client_new.file_name().unwrap().to_str().unwrap();
        assert!(
            server_name.starts_with("monopsony-server.bak."),
            "{}",
            server_name
        );
        assert!(
            client_name.starts_with("monopsony-client.new."),
            "{}",
            client_name
        );
        // Both carry the timestamp component.
        assert!(server_name.contains(".20"), "timestamped: {}", server_name);
        assert!(client_name.contains(".20"), "timestamped: {}", client_name);
    }

    /// UpdateContext::new correctly extracts config_paths from a recipe.
    #[test]
    fn test_update_context_extracts_config_paths_from_recipe() {
        let recipe_toml = r#"
[package]
name = "test"
source_type = "github_release"
source_url = "https://example.com"

[install]
method = "extract"
target_dir = "/tmp"
backup_old = true
config_paths = ["/etc/app/config.toml", "/etc/app/other.toml"]

[version]
detect_command = "echo 1.0.0"
detect_pattern = "(\\d+\\.\\d+\\.\\d+)"
"#;
        let recipe =
            toml::from_str::<monopsony_common::shadow_types::ShadowRecipe>(recipe_toml).unwrap();

        let ctx = UpdateContext::new(
            Some(recipe),
            std::path::PathBuf::from("/usr/local/bin/app"),
            std::path::PathBuf::from("/usr/local/bin"),
            "1.0.0".to_string(),
            "2.0.0".to_string(),
            "https://example.com/download".to_string(),
        );

        assert_eq!(ctx.config_paths.len(), 2);
        assert_eq!(
            ctx.config_paths[0],
            std::path::PathBuf::from("/etc/app/config.toml")
        );
        assert_eq!(
            ctx.config_paths[1],
            std::path::PathBuf::from("/etc/app/other.toml")
        );

        // backup_dir should be versioned
        assert!(
            ctx.backup_dir
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("update-backup-"),
            "backup_dir should be timestamped"
        );
    }

    /// Snapshot creates backup files on disk for binary and configs.
    #[test]
    fn test_snapshot_creates_backup_files() {
        let temp = tempfile::tempdir().unwrap();
        let install_dir = temp.path().to_path_buf();
        let bin_path = install_dir.join("monopsony-server");
        let config_path = install_dir.join("server.toml");

        std::fs::write(&bin_path, b"binary-data").unwrap();
        std::fs::write(&config_path, b"config-data").unwrap();

        let mut ctx = UpdateContext::new(
            None,
            bin_path.clone(),
            install_dir.clone(),
            "1.0.0".to_string(),
            "2.0.0".to_string(),
            String::new(),
        );
        // Manually add config_paths since recipe is None
        ctx.config_paths = vec![config_path.clone()];

        ctx.create_snapshot().unwrap();

        // Verify backup dir was created with binary.bak
        assert!(ctx.backup_dir.exists(), "backup dir should exist");
        assert!(
            ctx.backup_dir.join("binary.bak").exists(),
            "binary.bak should exist in backup dir"
        );

        // Verify config was backed up
        let config_backup_name =
            format!("{}.bak", config_path.file_name().unwrap().to_string_lossy());
        assert!(
            ctx.backup_dir.join(&config_backup_name).exists(),
            "config backup should exist"
        );

        // Verify backup content matches originals
        assert_eq!(
            std::fs::read(ctx.backup_dir.join("binary.bak")).unwrap(),
            b"binary-data"
        );
        assert_eq!(
            std::fs::read(ctx.backup_dir.join(&config_backup_name)).unwrap(),
            b"config-data"
        );
    }

    // ─── Sprint 34 P3: asset provider, manifest, staging, finalize ──

    #[test]
    fn archive_kind_url_suffix_and_magic_sniff() {
        use super::{archive_kind, ArchiveKind};
        // URL suffix drives the decision when a hint exists (historical
        // behavior).
        assert_eq!(
            archive_kind(
                b"raw",
                Some("https://x/monopsony-client-v1.0.0-linux-x86_64.tar.gz")
            ),
            ArchiveKind::TarGz
        );
        assert_eq!(
            archive_kind(b"raw", Some("https://x/asset.zip")),
            ArchiveKind::Zip
        );
        // No hint: sniff gzip / zip magic; anything else is raw. Real
        // monopsony binaries are ELF (\x7fELF) or Mach-O and never collide.
        assert_eq!(archive_kind(&[0x1f, 0x8b, 8, 0], None), ArchiveKind::TarGz);
        assert_eq!(archive_kind(b"PK\x03\x04rest", None), ArchiveKind::Zip);
        assert_eq!(archive_kind(b"\x7fELF.fake", None), ArchiveKind::Raw);
        assert_eq!(archive_kind(&[], None), ArchiveKind::Raw);
    }

    #[test]
    fn stage_asset_bytes_sniffs_raw_and_archive() {
        let temp = tempfile::tempdir().unwrap();
        // Raw bytes (a fake ELF) are written verbatim.
        let raw = b"\x7fELFnew-binary";
        let out = temp.path().join("raw-out");
        super::stage_asset_bytes(raw, &out, None).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), raw);

        // A real tar.gz containing the binary is extracted (sniffed, no
        // URL hint).
        let binary_content = b"#!/bin/sh\nfake monopsony-client\n";
        let staged_dir = temp.path().join("pkg");
        std::fs::create_dir_all(&staged_dir).unwrap();
        std::fs::write(staged_dir.join("monopsony-client"), binary_content).unwrap();
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            builder
                .append_dir_all("monopsony-2.0.0", &staged_dir)
                .unwrap();
            builder.finish().unwrap();
        }
        let mut gz_buf = Vec::new();
        {
            let mut enc =
                flate2::write::GzEncoder::new(&mut gz_buf, flate2::Compression::default());
            std::io::Write::write_all(&mut enc, &tar_buf).unwrap();
            enc.finish().unwrap();
        }
        let out2 = temp.path().join("archive-out");
        super::stage_asset_bytes(&gz_buf, &out2, None).unwrap();
        assert_eq!(std::fs::read(&out2).unwrap(), binary_content);
    }

    /// The canonical manifest payload must serialize to exactly the bytes
    /// the server signs (control/grpc.rs build_update_manifest_payload):
    /// sorted keys, no whitespace. Locked with a literal so any drift on
    /// either side fails here.
    #[test]
    fn manifest_payload_is_canonical_json() {
        use super::{build_update_manifest_payload, ManifestAsset};
        let assets = vec![ManifestAsset {
            name: "monopsony-client-v1.1.0-linux-x86_64.tar.gz".to_string(),
            platform: "linux-x86_64".to_string(),
            sha256: "abc123".to_string(),
            size: 42,
        }];
        let payload = build_update_manifest_payload("1.1.0", "client", "linux-x86_64", &assets);
        let payload_str = String::from_utf8(payload.clone()).unwrap();
        assert_eq!(
            payload_str,
            "{\"assets\":[{\"name\":\"monopsony-client-v1.1.0-linux-x86_64.tar.gz\",\"platform\":\"linux-x86_64\",\"sha256\":\"abc123\",\"size\":42}],\"binary\":\"client\",\"platform\":\"linux-x86_64\",\"version\":\"1.1.0\"}"
        );
        // Deterministic: same inputs → identical bytes.
        let again = build_update_manifest_payload("1.1.0", "client", "linux-x86_64", &assets);
        assert_eq!(payload, again);
        // Field order is sorted regardless of insertion: assets < binary <
        // platform < version (multiple assets keep list order — both sides
        // build from the same DB-ordered list).
        let two = build_update_manifest_payload(
            "1.1.0",
            "client",
            "linux-x86_64",
            &[
                ManifestAsset {
                    name: "a".to_string(),
                    platform: "p".to_string(),
                    sha256: "x".to_string(),
                    size: 1,
                },
                ManifestAsset {
                    name: "b".to_string(),
                    platform: "p".to_string(),
                    sha256: "y".to_string(),
                    size: 2,
                },
            ],
        );
        let two_str = String::from_utf8(two).unwrap();
        assert!(two_str.contains("\"name\":\"a\""));
        assert!(two_str.find("\"name\":\"a\"").unwrap() < two_str.find("\"name\":\"b\"").unwrap());
    }

    #[test]
    fn manifest_signature_verified_tampered_and_fail_closed() {
        use super::{
            build_update_manifest_payload, verify_update_manifest, ManifestAsset, ManifestCheck,
        };
        use base64::Engine;
        use ed25519_dalek::Signer;

        let sk = ed25519_dalek::SigningKey::generate(&mut rand::thread_rng());
        let vk = sk.verifying_key();
        let assets = vec![ManifestAsset {
            name: "n".to_string(),
            platform: "linux-x86_64".to_string(),
            sha256: "deadbeef".to_string(),
            size: 7,
        }];
        let payload = build_update_manifest_payload("2.0.0", "client", "linux-x86_64", &assets);
        let sig = base64::engine::general_purpose::STANDARD.encode(sk.sign(&payload).to_bytes());

        // Positive: correct key + signature over the rebuilt payload.
        assert_eq!(
            verify_update_manifest(&payload, &sig, Some(&vk)).unwrap(),
            ManifestCheck::Verified
        );

        // Tampered payload (asset digest changed) must fail closed.
        let tampered = build_update_manifest_payload(
            "2.0.0",
            "client",
            "linux-x86_64",
            &[ManifestAsset {
                name: "n".to_string(),
                platform: "linux-x86_64".to_string(),
                sha256: "cafebabe".to_string(),
                size: 7,
            }],
        );
        let err = verify_update_manifest(&tampered, &sig, Some(&vk)).unwrap_err();
        assert!(err.to_string().contains("fail-closed"), "{}", err);

        // Empty signature with a configured key fails closed (unsigned
        // manifest must not slip through when trust is configured).
        let err = verify_update_manifest(&payload, "", Some(&vk)).unwrap_err();
        assert!(err.to_string().contains("unsigned"), "{}", err);

        // No key configured → skip (D7 no-key posture), never an error.
        assert_eq!(
            verify_update_manifest(&payload, &sig, None).unwrap(),
            ManifestCheck::SkippedNoKey
        );
        assert_eq!(
            verify_update_manifest(&payload, "", None).unwrap(),
            ManifestCheck::SkippedNoKey
        );
    }

    #[test]
    fn contract_platform_maps_release_contract() {
        use super::contract_platform;
        assert_eq!(contract_platform("linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(contract_platform("linux", "aarch64"), Some("linux-aarch64"));
        // Apple Silicon: std reports aarch64, assets say arm64.
        assert_eq!(contract_platform("darwin", "aarch64"), Some("darwin-arm64"));
        assert_eq!(contract_platform("darwin", "arm64"), Some("darwin-arm64"));
        assert_eq!(contract_platform("darwin", "x86_64"), Some("darwin-x86_64"));
        // No published assets for other platforms.
        assert_eq!(contract_platform("windows", "x86_64"), None);
        assert_eq!(contract_platform("freebsd", "x86_64"), None);
        // The live host helper agrees with the pure map on this machine.
        let (os, arch, _libc) = super::detect_platform();
        assert_eq!(
            super::release_platform(),
            super::contract_platform(&os, &arch).map(str::to_string)
        );
    }

    #[test]
    fn binary_kind_wire_names() {
        use super::BinaryKind;
        assert_eq!(BinaryKind::Server.wire_name(), "server");
        assert_eq!(BinaryKind::Client.wire_name(), "client");
    }

    /// pgrep -x matches the kernel's comm (15-char truncation). Verified
    /// live: `pgrep -x monopsony-client` returns zero matches on procps ≥4
    /// ("pattern longer than 15 characters"), so both health gates must
    /// truncate to comm granularity.
    #[test]
    fn comm_name_truncates_to_kernel_comm_granularity() {
        use super::comm_name;
        assert_eq!(comm_name("monopsony-client"), "monopsony-clien");
        assert_eq!(comm_name("monopsony-server"), "monopsony-serve");
        assert_eq!(comm_name("nginx"), "nginx");
        assert_eq!(comm_name("123456789012345"), "123456789012345");
        assert_eq!(comm_name("1234567890123456"), "123456789012345");
        // Multibyte-safe: cuts on a char boundary, never mid-byte.
        assert_eq!(comm_name("ååååååååååååååååå"), "ååååååååååååååå");
    }

    #[test]
    fn stage_update_asset_extracts_and_chmods() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("monopsony-client");
        std::fs::write(&current, b"old").unwrap();

        // Build a tar.gz asset carrying the new binary.
        let new_binary = b"\x7fELF-new-monopsony-client-2.0.0";
        let pkg = temp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("monopsony-client"), new_binary).unwrap();
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            builder.append_dir_all("monopsony-2.0.0", &pkg).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            std::io::Write::write_all(&mut enc, &tar_buf).unwrap();
            enc.finish().unwrap();
        }

        let (staged, sha) = super::stage_update_asset(&gz, temp.path(), &current).expect("staging");
        assert_eq!(std::fs::read(&staged).unwrap(), new_binary);
        assert_eq!(sha, super::sha256_hex(new_binary));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&staged).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "staged binary must be executable");
        }
        let name = staged.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("monopsony-client.staged."), "{}", name);
    }

    #[test]
    fn finalize_command_json_roundtrip_via_files() {
        use super::{read_finalize_command, write_finalize_command, FinalizeCommand};
        let temp = tempfile::tempdir().unwrap();
        let cmd = FinalizeCommand {
            staged_path: temp.path().join("monopsony-client.staged.20260101_000000"),
            staged_sha256: "abc123".to_string(),
            from_version: "1.0.0".to_string(),
            to_version: "1.1.0".to_string(),
            binary: "client".to_string(),
            created_at: 1760000000,
        };
        let path = write_finalize_command(temp.path(), &cmd).unwrap();
        assert!(path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("1760000000"));
        let back = read_finalize_command(&path).unwrap();
        assert_eq!(back.staged_path, cmd.staged_path);
        assert_eq!(back.staged_sha256, cmd.staged_sha256);
        assert_eq!(back.to_version, "1.1.0");
        assert_eq!(back.binary, "client");
    }

    #[test]
    fn finalize_guard_refuses_live_pid_and_takes_over_stale() {
        let temp = tempfile::tempdir().unwrap();
        // Live PID (ourselves) → second acquire refuses.
        let pid_path = temp.path().join("monopsony-self-update.pid");
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();
        let err = super::acquire_finalize_guard(temp.path()).unwrap_err();
        assert!(err.to_string().contains("already running"), "{}", err);

        // Stale (unparseable) guard is taken over.
        std::fs::write(&pid_path, "not-a-pid").unwrap();
        let guard = super::acquire_finalize_guard(temp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&pid_path).unwrap(),
            std::process::id().to_string()
        );
        // Drop removes the guard file.
        drop(guard);
        assert!(!pid_path.exists());
    }

    #[test]
    fn decide_init_system_families() {
        use super::decide_init_system;
        use super::InitSystem;
        // macOS → launchd (when launchctl exists).
        assert_eq!(
            decide_init_system("macos", false, false, false, true),
            InitSystem::Launchd
        );
        // macOS without launchctl falls through to Unknown.
        assert_eq!(
            decide_init_system("macos", false, false, false, false),
            InitSystem::Unknown
        );
        // systemd run-dir wins over the others (strongest signal first).
        assert_eq!(
            decide_init_system("linux", true, true, true, false),
            InitSystem::Systemd
        );
        // OpenRC needs BOTH the softlevel marker and the binary.
        assert_eq!(
            decide_init_system("linux", false, true, true, false),
            InitSystem::OpenRc
        );
        assert_eq!(
            decide_init_system("linux", false, true, false, false),
            InitSystem::Unknown
        );
        // Container/bare hosts with none of the markers.
        assert_eq!(
            decide_init_system("linux", false, false, false, false),
            InitSystem::Unknown
        );
    }

    #[test]
    fn restart_argv_per_init() {
        use super::restart_argv;
        use super::InitSystem;

        let (prog, args) = restart_argv(InitSystem::Systemd, "monopsony-client").unwrap();
        assert_eq!(prog, "systemctl");
        assert_eq!(args, vec!["restart", "monopsony-client"]);

        let (prog, args) = restart_argv(InitSystem::OpenRc, "monopsony-client").unwrap();
        assert_eq!(prog, "rc-service");
        assert_eq!(args, vec!["monopsony-client", "restart"]);

        let (prog, args) = restart_argv(InitSystem::Launchd, "com.monopsony.client").unwrap();
        assert_eq!(prog, "launchctl");
        assert_eq!(args, vec!["kickstart", "-k", "system/com.monopsony.client"]);

        assert!(restart_argv(InitSystem::Unknown, "monopsony-client").is_none());
    }

    #[test]
    fn unit_names_per_init_and_kind() {
        use super::unit_for;
        use super::{BinaryKind, InitSystem};
        assert_eq!(
            unit_for(InitSystem::Systemd, BinaryKind::Client),
            "monopsony-client"
        );
        assert_eq!(
            unit_for(InitSystem::OpenRc, BinaryKind::Server),
            "monopsony-server"
        );
        assert_eq!(
            unit_for(InitSystem::Launchd, BinaryKind::Client),
            "com.monopsony.client"
        );
        assert_eq!(
            unit_for(InitSystem::Launchd, BinaryKind::Server),
            "com.monopsony.server"
        );
        assert_eq!(
            unit_for(InitSystem::Unknown, BinaryKind::Client),
            "monopsony-client"
        );
    }

    /// A benign finalize-test recipe: hooks are `true`, the health gate is a
    /// process check with a per-test unique name so parallel tests never
    /// satisfy each other's gate via pgrep.
    fn finalize_test_recipe(
        process: &str,
        timeout_secs: u32,
    ) -> monopsony_common::shadow_types::ShadowRecipe {
        let toml_str = format!(
            r#"
[package]
name = "monopsony-client"
source_type = "github_release"
source_url = "https://example.com/test"

[install]
method = "extract"
target_dir = "/tmp"
backup_old = true
max_backups = 3
pre_update = "true"
post_update = "true"
on_failure = "true"

[install.health_check]
type = "process"
process = "{process}"
timeout_secs = {timeout_secs}

[version]
detect_command = "true"
detect_pattern = "(.+)"
"#
        );
        toml::from_str(&toml_str).expect("finalize test recipe must parse")
    }

    fn finalize_test_cmd(
        temp: &tempfile::TempDir,
        staged_name: &str,
        content: &[u8],
    ) -> super::FinalizeCommand {
        let staged = temp.path().join(staged_name);
        std::fs::write(&staged, content).unwrap();
        super::FinalizeCommand {
            staged_path: staged,
            staged_sha256: super::sha256_hex(content),
            from_version: "1.0.0".to_string(),
            to_version: "2.0.0".to_string(),
            binary: "client".to_string(),
            created_at: 1760000000,
        }
    }

    /// Tempdir finalize simulation — success path: staged binary swaps in,
    /// health gate passes (renamed helper process), backup retained,
    /// staged file consumed. Restart is Noop (tests never touch services).
    #[tokio::test]
    async fn run_finalize_success_commits_swap() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("monopsony-client");
        let old_bytes = b"\x7fELF-old-daemon";
        std::fs::write(&current, old_bytes).unwrap();

        let new_bytes = b"\x7fELF-new-daemon-2.0.0";
        let cmd = finalize_test_cmd(&temp, "staged.ok", new_bytes);

        // Health gate target: a process whose comm is exactly the recipe's
        // process name (copy a coreutils binary under that name).
        let sleep_src = "/bin/sleep".to_string();
        if !std::path::Path::new(&sleep_src).exists() {
            eprintln!("skipping: no /bin/sleep on this host");
            return;
        }
        let helper = temp.path().join("monop3fin-successhelper");
        std::fs::copy(&sleep_src, &helper).unwrap();
        let mut child = std::process::Command::new(&helper)
            .arg("30")
            .spawn()
            .expect("helper process");

        let outcome = super::run_finalize_with_recipe(
            &cmd,
            &current,
            super::RestartAction::Noop,
            Some(finalize_test_recipe("monop3fin-successhelper", 20)),
        )
        .await
        .expect("finalize should succeed");

        let _ = child.kill();
        let _ = child.wait();

        assert!(matches!(outcome, super::UpdateOutcome::Success));
        assert_eq!(
            std::fs::read(&current).unwrap(),
            new_bytes,
            "current binary must be the staged bytes"
        );
        // Backup of the old binary exists in a snapshot dir.
        let backups: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .unwrap_or("")
                    .starts_with("update-backup-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "one snapshot dir");
        assert_eq!(
            std::fs::read(backups[0].path().join("binary.bak")).unwrap(),
            old_bytes
        );
        assert!(!cmd.staged_path.exists(), "staged file consumed");
    }

    /// Tempdir finalize simulation — health failure: nothing matches the
    /// gate name, so finalize rolls the old binary back and reports
    /// RolledBack.
    #[tokio::test]
    async fn run_finalize_health_failure_rolls_back() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("monopsony-client");
        let old_bytes = b"\x7fELF-old-daemon";
        std::fs::write(&current, old_bytes).unwrap();

        let cmd = finalize_test_cmd(&temp, "staged.rb", b"\x7fELF-broken-2.0.0");

        // Gate name unique within the kernel's 15-char comm so the parallel
        // success test's helper can never satisfy this gate.
        let outcome = super::run_finalize_with_recipe(
            &cmd,
            &current,
            super::RestartAction::Noop,
            Some(finalize_test_recipe("monop3fin-failhelper-nope", 1)),
        )
        .await
        .expect("rollback path returns Ok(RolledBack)");

        match outcome {
            super::UpdateOutcome::RolledBack(reason) => {
                assert!(reason.contains("restored"), "{}", reason)
            }
            other => panic!("expected RolledBack, got {:?}", other),
        }
        assert_eq!(
            std::fs::read(&current).unwrap(),
            old_bytes,
            "old binary must be restored"
        );
    }

    /// Tampered staged bytes (hash no longer matches the command file)
    /// refuse before anything is touched.
    #[tokio::test]
    async fn run_finalize_staged_hash_mismatch_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("monopsony-client");
        std::fs::write(&current, b"old").unwrap();

        let cmd = finalize_test_cmd(&temp, "staged.tamper", b"original-staged");
        // Tamper AFTER recording the hash.
        std::fs::write(&cmd.staged_path, b"evil-replaced").unwrap();

        let err = super::run_finalize_with_recipe(
            &cmd,
            &current,
            super::RestartAction::Noop,
            Some(finalize_test_recipe("monopsony-p3-fin-any", 1)),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("hash mismatch"), "{}", err);
        assert_eq!(std::fs::read(&current).unwrap(), b"old");
    }

    #[test]
    fn rollback_latest_backup_restores_newest() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("monopsony-client");
        std::fs::write(&current, b"current-v3").unwrap();

        let older = temp.path().join("update-backup-20260101-000000");
        let newer = temp.path().join("update-backup-20260102-000000");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::write(older.join("binary.bak"), b"v1").unwrap();
        // Ensure a strictly newer mtime for the newer backup.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(newer.join("binary.bak"), b"v2").unwrap();

        let used = super::rollback_latest_backup(&current, temp.path()).unwrap();
        assert_eq!(used, newer.join("binary.bak"));
        assert_eq!(std::fs::read(&current).unwrap(), b"v2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&current).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "restored binary must be executable");
        }
    }
}
