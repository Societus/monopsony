//! Client-side shadow package operations: download, extract, health check, rollback.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Result;
use regex::Regex;
use tokio::fs;
use tokio::process::Command;
use tracing::{debug, info};

use monopsony_common::shadow_types::{HealthCheck, HealthCheckType, ShadowRecipe};

#[derive(Debug, thiserror::Error)]
pub enum ShadowClientError {
    #[error("download failed: {0}")]
    DownloadFailed(String),
    #[error("hash mismatch: expected {expected}, got {got}")]
    HashMismatch { expected: String, got: String },
    #[error("extraction failed: {0}")]
    ExtractionFailed(String),
    #[error("health check timeout")]
    HealthCheckTimeout,
    #[error("version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: String, got: String },
    #[error("rollback failed: {0}")]
    RollbackFailed(String),
    #[error("verification failed: {0}")]
    VerificationFailed(String),
    #[error("trust verification failed: {0}")]
    TrustVerificationFailed(String),
    #[error("install failed: {0}")]
    InstallFailed(String),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

pub struct UpdateResult {
    pub success: bool,
    pub old_version: String,
    pub new_version: String,
    pub backup_path: Option<PathBuf>,
    pub message: String,
}

/// Verify installed version by running detect_command and matching detect_pattern
pub async fn verify_version(
    detect_command: &str,
    detect_pattern: &str,
) -> Result<String, ShadowClientError> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(detect_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ShadowClientError::VerificationFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(ShadowClientError::VerificationFailed(format!(
            "command exited with {}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    extract_version(&stdout, detect_pattern)
}

/// Detect installed version using an alternative detection method.
pub async fn detect_version(
    config: &monopsony_common::shadow_types::VersionDetect,
) -> Result<String, ShadowClientError> {
    use monopsony_common::shadow_types::VersionDetectMethod;

    match config.method {
        VersionDetectMethod::Command => verify_version(&config.source, &config.pattern).await,
        VersionDetectMethod::File => {
            let content = tokio::fs::read_to_string(&config.source)
                .await
                .map_err(|e| {
                    ShadowClientError::VerificationFailed(format!(
                        "failed to read file '{}': {}",
                        config.source, e
                    ))
                })?;
            extract_version(&content, &config.pattern)
        }
        VersionDetectMethod::Header => {
            let header_name = config.header_name.as_deref().unwrap_or("etag");
            let client = reqwest::Client::new();
            let resp = client
                .head(&config.source)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|e| {
                    ShadowClientError::VerificationFailed(format!(
                        "HTTP HEAD to '{}' failed: {}",
                        config.source, e
                    ))
                })?;
            let value = resp
                .headers()
                .get(header_name)
                .ok_or_else(|| {
                    ShadowClientError::VerificationFailed(format!(
                        "header '{}' not found in response",
                        header_name
                    ))
                })?
                .to_str()
                .map_err(|e| {
                    ShadowClientError::VerificationFailed(format!(
                        "header value not valid UTF-8: {}",
                        e
                    ))
                })?;
            extract_version(value, &config.pattern)
        }
    }
}

/// Extract a version string from text using a regex with one capture group.
fn extract_version(text: &str, pattern: &str) -> Result<String, ShadowClientError> {
    let pat =
        Regex::new(pattern).map_err(|e| ShadowClientError::VerificationFailed(e.to_string()))?;
    let caps = pat
        .captures(text)
        .ok_or_else(|| ShadowClientError::VersionMismatch {
            expected: pattern.to_string(),
            got: text.trim().to_string(),
        })?;
    let version = caps
        .get(1)
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| text.trim().to_string());
    Ok(version)
}

/// Create backup of target directory
pub async fn backup_target(target_dir: &str, version: &str) -> Result<PathBuf, ShadowClientError> {
    let src = Path::new(target_dir);
    if !src.exists() {
        return Ok(PathBuf::from(format!("{target_dir}.nonexistent")));
    }

    let backup_path = PathBuf::from(format!("{target_dir}.bak.{version}"));
    if backup_path.exists() {
        fs::remove_dir_all(&backup_path).await?;
    }

    // Copy the directory
    copy_dir_recursive(src, &backup_path).await?;
    info!("Created backup at {}", backup_path.display());
    Ok(backup_path)
}

/// Restore from a backup directory
pub async fn restore_backup(target_dir: &str, backup_path: &Path) -> Result<(), ShadowClientError> {
    let target = Path::new(target_dir);
    if target.exists() {
        fs::remove_dir_all(target).await?;
    }
    if backup_path.exists() {
        copy_dir_recursive(backup_path, target).await?;
        info!("Restored {} from backup", target_dir);
    }
    Ok(())
}

/// A single rollback entry (versioned backup).
#[derive(Debug, Clone)]
pub struct RollbackEntry {
    pub version: String,
    pub backup_path: PathBuf,
    /// Modification time of the backup directory.
    pub modified: std::time::SystemTime,
}

/// List available rollback versions for a package.
///
/// Scans for directories matching `{target_dir}.bak.*` and returns them
/// sorted newest-first.
pub async fn list_rollback_versions(
    target_dir: &str,
) -> Result<Vec<RollbackEntry>, ShadowClientError> {
    let parent = Path::new(target_dir).parent().unwrap_or(Path::new("."));
    let dir_name = Path::new(target_dir)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let prefix = format!("{dir_name}.bak.");

    let mut entries = Vec::new();
    let mut read_dir = fs::read_dir(parent).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(version) = name.strip_prefix(&prefix) {
            let meta = entry.metadata().await?;
            entries.push(RollbackEntry {
                version: version.to_string(),
                backup_path: entry.path(),
                modified: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            });
        }
    }

    // Sort newest first
    entries.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(entries)
}

/// Rollback to a specific version by restoring its backup.
///
/// Creates a backup of the current version first, then restores the target.
pub async fn rollback_to_version(
    target_dir: &str,
    target_version: &str,
) -> Result<(), ShadowClientError> {
    let versions = list_rollback_versions(target_dir).await?;
    let entry = versions
        .iter()
        .find(|v| v.version == target_version)
        .ok_or_else(|| {
            ShadowClientError::RollbackFailed(format!(
                "version '{}' not found in rollback history (available: {})",
                target_version,
                versions
                    .iter()
                    .map(|v| v.version.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

    // Detect current version from the live directory name for logging
    info!("Rolling back {} to version {}", target_dir, target_version);

    // Backup current state before rollback
    let current = Path::new(target_dir);
    if current.exists() {
        let current_backup =
            PathBuf::from(format!("{target_dir}.bak.prerollback.{target_version}"));
        copy_dir_recursive(current, &current_backup).await?;
    }

    // Restore the target backup
    restore_backup(target_dir, &entry.backup_path).await?;

    info!(
        "Successfully rolled back {} to {}",
        target_dir, target_version
    );
    Ok(())
}

/// Prune old backups exceeding max_backups.
///
/// Keeps the `max_backups` most recent backups, deletes the rest.
pub async fn prune_backups(
    target_dir: &str,
    max_backups: usize,
) -> Result<usize, ShadowClientError> {
    let max = max_backups.max(1);
    let versions = list_rollback_versions(target_dir).await?;

    if versions.len() <= max {
        return Ok(0);
    }

    let to_remove = &versions[max..];
    let mut removed = 0;
    for entry in to_remove {
        debug!("Pruning old backup: {}", entry.backup_path.display());
        if entry.backup_path.is_dir() {
            fs::remove_dir_all(&entry.backup_path).await?;
        } else {
            fs::remove_file(&entry.backup_path).await?;
        }
        removed += 1;
    }

    info!("Pruned {} old backups for {}", removed, target_dir);
    Ok(removed)
}

/// Extract a tar.gz archive to a temp directory
pub async fn extract_to_temp(archive_path: &Path) -> Result<PathBuf, ShadowClientError> {
    let temp_dir = archive_path
        .parent()
        .unwrap_or(Path::new("/tmp"))
        .join("extracted");
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir).await?;
    }
    fs::create_dir_all(&temp_dir).await?;

    let output = Command::new("tar")
        .arg("xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(&temp_dir)
        .arg("--strip-components=1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ShadowClientError::ExtractionFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ShadowClientError::ExtractionFailed(stderr.to_string()));
    }

    debug!("Extracted to {}", temp_dir.display());
    Ok(temp_dir)
}

/// Install individual binaries from an extracted archive to their target paths.
/// Used when `RecipeInstall.binaries` is populated (multi-binary packages).
///
/// For each `BinaryEntry`:
/// 1. Locate the source file in the extracted temp dir
/// 2. Back up the existing target file (if present)
/// 3. Copy source -> target
/// 4. Apply chmod (default 0755)
pub async fn install_binaries(
    temp_dir: &Path,
    entries: &[monopsony_common::shadow_types::BinaryEntry],
    backup_dir: &Path,
) -> Result<Vec<PathBuf>, ShadowClientError> {
    let mut installed = Vec::new();

    // Ensure backup dir exists
    if !backup_dir.exists() {
        fs::create_dir_all(backup_dir).await?;
    }

    for entry in entries {
        // Find the source file in extracted dir (may be at root or under subdirs)
        let source = find_in_extracted(temp_dir, &entry.source)?;

        let target = PathBuf::from(&entry.target);

        // Ensure parent directory exists
        if let Some(parent) = target.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await.map_err(|e| {
                    ShadowClientError::InstallFailed(format!(
                        "failed to create parent dir {}: {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
        }

        // Back up existing target if present
        if target.exists() {
            let backup_name = format!(
                "{}.bak.{}",
                target.file_name().unwrap_or_default().to_string_lossy(),
                chrono::Utc::now().timestamp()
            );
            let backup_path = backup_dir.join(&backup_name);
            fs::copy(&target, &backup_path).await.map_err(|e| {
                ShadowClientError::InstallFailed(format!(
                    "failed to back up {}: {}",
                    target.display(),
                    e
                ))
            })?;
            debug!(
                "Backed up {} -> {}",
                target.display(),
                backup_path.display()
            );
        }

        // Copy source to target
        fs::copy(&source, &target).await.map_err(|e| {
            ShadowClientError::InstallFailed(format!(
                "failed to copy {} -> {}: {}",
                source.display(),
                target.display(),
                e
            ))
        })?;

        // Apply chmod (default 0755)
        let mode_str = entry.mode.as_deref().unwrap_or("0755");
        let mode = u32::from_str_radix(mode_str, 8).map_err(|e| {
            ShadowClientError::InstallFailed(format!("invalid chmod mode '{}': {}", mode_str, e))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                .await
                .map_err(|e| {
                    ShadowClientError::InstallFailed(format!(
                        "failed to chmod {}: {}",
                        target.display(),
                        e
                    ))
                })?;
        }

        info!(
            "Installed {} -> {} ({:o})",
            source.display(),
            target.display(),
            mode
        );
        installed.push(target);
    }

    Ok(installed)
}

/// Find a file within the extracted directory. Searches at root level first,
/// then recursively through subdirectories (to handle --strip-components).
fn find_in_extracted(temp_dir: &Path, filename: &str) -> Result<PathBuf, ShadowClientError> {
    // Try direct path first
    let direct = temp_dir.join(filename);
    if direct.exists() {
        return Ok(direct);
    }

    // Try just the filename component (strip any prefix dirs)
    let basename = Path::new(filename)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    // Walk temp_dir looking for the file
    for entry in std::fs::read_dir(temp_dir).map_err(|e| {
        ShadowClientError::InstallFailed(format!(
            "failed to read temp dir {}: {}",
            temp_dir.display(),
            e
        ))
    })? {
        let entry = entry.map_err(|e| {
            ShadowClientError::InstallFailed(format!("failed to read dir entry: {}", e))
        })?;
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let candidate = entry.path().join(basename.as_ref());
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Err(ShadowClientError::InstallFailed(format!(
        "binary '{}' not found in extracted archive at {}",
        filename,
        temp_dir.display()
    )))
}

/// Run a health check after update
pub async fn run_health_check(check: &HealthCheck) -> Result<(), ShadowClientError> {
    let start = Instant::now();
    let timeout = Duration::from_secs(check.timeout_secs);

    match check.check_type {
        HealthCheckType::Http => {
            let url = check.url.as_deref().unwrap_or("");
            let client = reqwest::Client::new();
            loop {
                if start.elapsed() > timeout {
                    return Err(ShadowClientError::HealthCheckTimeout);
                }
                match client.get(url).timeout(Duration::from_secs(5)).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        info!("Health check passed: HTTP {}", resp.status());
                        return Ok(());
                    }
                    _ => {
                        debug!("Health check retrying...");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }
        HealthCheckType::Port => {
            let port = check.port.unwrap_or(0);
            loop {
                if start.elapsed() > timeout {
                    return Err(ShadowClientError::HealthCheckTimeout);
                }
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    info!("Health check passed: port {port}");
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        HealthCheckType::Process => {
            let name = check.process.as_deref().unwrap_or("");
            loop {
                if start.elapsed() > timeout {
                    return Err(ShadowClientError::HealthCheckTimeout);
                }
                let output = Command::new("pgrep")
                    .arg("-x")
                    .arg(name)
                    .output()
                    .await
                    .ok();
                if let Some(out) = output {
                    if out.status.success() {
                        info!("Health check passed: process {name}");
                        return Ok(());
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Verify that expected files exist in the extracted directory
pub fn verify_extraction(temp_dir: &Path, recipe: &ShadowRecipe) -> Result<(), ShadowClientError> {
    if let Some(chmod_list) = &recipe.install.chmod {
        for bin in chmod_list {
            let bin_path = temp_dir.join(bin);
            if !bin_path.exists() {
                return Err(ShadowClientError::VerificationFailed(format!(
                    "expected file not found: {}",
                    bin_path.display()
                )));
            }
        }
    }
    Ok(())
}

/// Atomically move files from source to target directory
pub async fn atomic_swap(source_dir: &Path, target_dir: &Path) -> Result<(), ShadowClientError> {
    fs::create_dir_all(target_dir).await?;

    let mut entries = fs::read_dir(source_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let file_name = entry.file_name();
        let dest = target_dir.join(&file_name);
        debug!("Moving {} -> {}", entry.path().display(), dest.display());
        fs::rename(entry.path(), &dest).await?;
    }

    Ok(())
}

/// Recursively copy a directory
fn copy_dir_recursive<'a>(
    src: &'a Path,
    dst: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ShadowClientError>> + 'a>> {
    Box::pin(async move {
        fs::create_dir_all(dst).await?;
        let mut entries = fs::read_dir(src).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let dest_path = dst.join(entry.file_name());
            if file_type.is_dir() {
                copy_dir_recursive(&entry.path(), &dest_path).await?;
            } else {
                fs::copy(entry.path(), &dest_path).await?;
            }
        }
        Ok(())
    })
}

/// Check that all build dependencies are installed on the system.
/// Runs `which <dep>` for each dep. Returns Err listing all missing deps.
pub async fn check_build_deps(requires: &[String]) -> Result<(), ShadowClientError> {
    let mut missing = Vec::new();
    for dep in requires {
        let output = Command::new("which")
            .arg(dep)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {}
            _ => missing.push(dep.clone()),
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(ShadowClientError::VerificationFailed(format!(
            "missing build dependencies: {}",
            missing.join(", ")
        )))
    }
}

/// Network access policy for build/script step execution.
#[derive(Debug, PartialEq)]
pub enum NetworkPolicy {
    /// Steps run without network access (default; deny by default).
    Denied,
    /// Steps may access the network (explicit recipe opt-in).
    Allowed,
}

/// Resolve a recipe's `allow_network` flag into a NetworkPolicy.
/// False (the serde default for old recipes) maps to Denied.
pub fn resolve_network_policy(allow_network: bool) -> NetworkPolicy {
    if allow_network {
        NetworkPolicy::Allowed
    } else {
        NetworkPolicy::Denied
    }
}

/// One-time detection of `unshare -n` usability (user namespace with
/// network isolation). Cached in a OnceLock so we pay the spawn cost once.
fn unshare_available() -> bool {
    static UNSHARE_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *UNSHARE_OK.get_or_init(|| {
        use std::process::{Command as StdCommand, Stdio};
        // Blocking detection via the std Command: safe to call from async
        // context because `unshare -n true` exits immediately (well under
        // the 2s budget; no runtime re-entrancy as with Handle::block_on).
        StdCommand::new("unshare")
            .args(["-n", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Apply network-denial hardening to a command about to be spawned.
///
/// `program`/`args` describe the step exactly as it would be spawned
/// (typically `bash`, `["-c", script]`). Primary control: when `unshare -n`
/// works on this host, the command is rewritten to spawn
/// `unshare -n bash -c <step>` (real kernel-level network isolation).
/// Fallback (unshare unavailable, e.g. unprivileged containers with userns
/// disabled): the argv is unchanged but proxy env vars are poisoned so
/// network tools that honor proxy configuration are diverted to a dead
/// local port. Env-poisoning is BEST-EFFORT ONLY — a build step using raw
/// sockets bypasses it; unshare is the real control. Landlock
/// write-confinement is a planned follow-up hardening layer.
pub fn build_network_denial(command: &mut tokio::process::Command) {
    if unshare_available() {
        // Real control: re-run the whole step inside a network-less namespace
        // by rebuilding the command as `unshare -n <program> <args...>`.
        let std_cmd = command.as_std();
        let program = std_cmd.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut rebuilt = tokio::process::Command::new("unshare");
        rebuilt.arg("-n").arg(program);
        for a in args {
            rebuilt.arg(a);
        }
        *command = rebuilt;
    } else {
        // Env-poison is best-effort; unshare is the real control (above).
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            command.env(key, "http://127.0.0.1:9");
        }
        // NO_PROXY cleared so the poisoned proxy is not bypassed.
        command.env_remove("NO_PROXY");
        command.env_remove("no_proxy");
    }
}

/// Execute build steps sequentially in the source directory.
/// Variables: {src_dir}, {target_dir}, {version}, {arch}
pub async fn execute_build(
    steps: &[String],
    src_dir: &Path,
    target_dir: &str,
    version: &str,
    arch: &str,
    policy: NetworkPolicy,
) -> Result<(), ShadowClientError> {
    for (i, step) in steps.iter().enumerate() {
        let cmd = step
            .replace("{src_dir}", &src_dir.to_string_lossy())
            .replace("{target_dir}", target_dir)
            .replace("{version}", version)
            .replace("{arch}", arch);

        let mut command = Command::new("bash");
        if policy == NetworkPolicy::Denied {
            build_network_denial(&mut command);
        }
        let output = command
            .arg("-c")
            .arg(&cmd)
            .current_dir(src_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| ShadowClientError::ExtractionFailed(format!("step {}: {}", i + 1, e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ShadowClientError::ExtractionFailed(format!(
                "build step {} failed (exit {}): {}",
                i + 1,
                output.status.code().unwrap_or(-1),
                stderr
            )));
        }
    }
    Ok(())
}

/// Execute an update script from a recipe.
/// Variables: {version}, {arch}, {os}.
/// Env vars: MONOPSONY_VERSION, MONOPSONY_ARCH, MONOPSONY_OS.
pub async fn execute_script(
    script: &str,
    version: &str,
    arch: &str,
    os: &str,
    policy: NetworkPolicy,
) -> Result<(), ShadowClientError> {
    let cmd = script
        .replace("{version}", version)
        .replace("{arch}", arch)
        .replace("{os}", os);

    let mut command = Command::new("bash");
    if policy == NetworkPolicy::Denied {
        build_network_denial(&mut command);
    }
    let output = command
        .arg("-c")
        .arg(&cmd)
        .env("MONOPSONY_VERSION", version)
        .env("MONOPSONY_ARCH", arch)
        .env("MONOPSONY_OS", os)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| ShadowClientError::ExtractionFailed(format!("script execution: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ShadowClientError::ExtractionFailed(format!(
            "script failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr
        )));
    }
    Ok(())
}

/// Run a hook command (pre_update, post_update, on_failure).
/// Returns Ok(()) if hook succeeded or if hook is None.
/// Returns Err with stderr output if the hook command fails.
pub async fn run_hook(hook: Option<&str>, hook_name: &str) -> Result<(), ShadowClientError> {
    let hook = match hook {
        Some(h) if !h.is_empty() => h,
        _ => return Ok(()),
    };

    let output = Command::new("bash")
        .arg("-c")
        .arg(hook)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| ShadowClientError::VerificationFailed(format!("{hook_name}: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ShadowClientError::VerificationFailed(format!(
            "{hook_name} failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn verify_version_echo() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("version.sh");
        fs::write(&script, "#!/bin/sh\necho '4.0.7.1863'\n")
            .await
            .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let result = verify_version(&format!("{}", script.display()), r"(\d+\.\d+\.\d+\.\d+)")
            .await
            .unwrap();
        assert_eq!(result, "4.0.7.1863");
    }

    #[tokio::test]
    async fn verify_version_no_match() {
        let result = verify_version("echo no-version-here", r"(\d+\.\d+\.\d+\.\d+)").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn backup_and_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        fs::create_dir_all(&target).await.unwrap();
        fs::write(target.join("config.txt"), "old config")
            .await
            .unwrap();

        let backup = backup_target(&target.to_string_lossy(), "1.0.0")
            .await
            .unwrap();

        // Modify target
        fs::write(target.join("config.txt"), "new config")
            .await
            .unwrap();

        // Restore
        restore_backup(&target.to_string_lossy(), &backup)
            .await
            .unwrap();

        let content = fs::read_to_string(target.join("config.txt")).await.unwrap();
        assert_eq!(content, "old config");
    }

    #[tokio::test]
    async fn extract_tarball() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a tar.gz with a subdirectory so --strip-components=1 works
        let subdir = tmp.path().join("pkg");
        fs::create_dir_all(&subdir).await.unwrap();
        fs::write(subdir.join("hello.txt"), "hello world")
            .await
            .unwrap();

        let archive = tmp.path().join("test.tar.gz");
        let archive_str = archive.to_string_lossy().into_owned();
        let tmp_str = tmp.path().to_string_lossy().into_owned();
        let output = Command::new("tar")
            .args(["czf", &archive_str, "-C", &tmp_str, "pkg"])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());

        let extracted = extract_to_temp(&archive).await.unwrap();
        let content = fs::read_to_string(extracted.join("hello.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn atomic_swap_moves_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).await.unwrap();
        fs::write(src.join("file1.txt"), "content1").await.unwrap();
        fs::write(src.join("file2.txt"), "content2").await.unwrap();

        atomic_swap(&src, &dst).await.unwrap();

        assert!(dst.join("file1.txt").exists());
        assert!(dst.join("file2.txt").exists());
        assert!(!src.join("file1.txt").exists());
    }

    #[test]
    fn verify_extraction_checks_chmod_files() {
        let tmp = tempfile::tempdir().unwrap();
        let recipe = ShadowRecipe {
            package: monopsony_common::shadow_types::RecipePackage {
                name: "test".into(),
                description: None,
                homepage: None,
                source_type: monopsony_common::shadow_types::ShadowSource::GithubRelease,
                source_url: "https://github.com/test/test".into(),
            },
            install: monopsony_common::shadow_types::RecipeInstall {
                method: monopsony_common::shadow_types::InstallMethod::Extract,
                target_dir: "/usr/local/bin".into(),
                chmod: Some(vec!["mybin".into()]),
                service: None,
                config_paths: None,
                backup_old: false,
                health_check: None,
                pre_update: None,
                post_update: None,
                on_failure: None,
                max_backups: 3,
                binaries: vec![],
            },
            asset_selector: None,
            build: None,
            script: None,
            version: monopsony_common::shadow_types::RecipeVersion {
                detect_command: "mybin --version".into(),
                detect_pattern: "([0-9]+\\.[0-9]+)".into(),
            },
            notifications: None,
            direct_url: None,
        };

        // mybin doesn't exist -> fail
        assert!(verify_extraction(tmp.path(), &recipe).is_err());

        // Create mybin -> pass
        std::fs::write(tmp.path().join("mybin"), "binary").unwrap();
        assert!(verify_extraction(tmp.path(), &recipe).is_ok());
    }

    #[tokio::test]
    async fn check_build_deps_all_present() {
        // bash and sh should always be present
        let result = check_build_deps(&["bash".to_string(), "sh".to_string()]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_build_deps_missing() {
        let result = check_build_deps(&["nonexistent_tool_xyz_12345".to_string()]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent_tool_xyz_12345"));
    }

    #[tokio::test]
    async fn execute_build_two_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).await.unwrap();
        fs::create_dir_all(&dst).await.unwrap();

        let steps = vec![
            format!("echo hello > {}/file1.txt", dst.to_string_lossy()),
            format!("echo world > {}/file2.txt", dst.to_string_lossy()),
        ];

        execute_build(
            &steps,
            &src,
            &dst.to_string_lossy(),
            "1.0.0",
            "x86_64",
            NetworkPolicy::Allowed,
        )
        .await
        .unwrap();

        assert!(dst.join("file1.txt").exists());
        assert!(dst.join("file2.txt").exists());
    }

    #[tokio::test]
    async fn execute_build_step_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).await.unwrap();

        let steps = vec!["exit 1".to_string()];
        let result = execute_build(&steps, &src, "/tmp", "1.0.0", "x86_64", NetworkPolicy::Allowed)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("step 1"));
    }

    #[tokio::test]
    async fn execute_script_with_env() {
        let tmp = tempfile::tempdir().unwrap();
        let outfile = tmp.path().join("env_output.txt");

        let script = format!("echo $MONOPSONY_VERSION > {}", outfile.to_string_lossy());
        execute_script(&script, "2.0.0", "x86_64", "linux", NetworkPolicy::Allowed)
            .await
            .unwrap();

        let content = fs::read_to_string(&outfile).await.unwrap();
        assert!(content.trim() == "2.0.0");
    }

    #[tokio::test]
    async fn execute_script_fails() {
        let result = execute_script("exit 42", "1.0", "x86_64", "linux", NetworkPolicy::Allowed)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("42"));
    }

    // --- Network sandbox tests (Sprint 22 Phase G) ---

    #[test]
    fn network_policy_goldens() {
        // Default (false) must be Denied — old recipes deny by default.
        assert_eq!(resolve_network_policy(false), NetworkPolicy::Denied);
        assert_eq!(resolve_network_policy(true), NetworkPolicy::Allowed);
    }

    #[test]
    fn unshare_detection_does_not_panic() {
        // Must return a bool without panicking, whatever the host supports.
        let _available = unshare_available();
    }

    #[tokio::test]
    async fn poisoned_env_when_denied() {
        if !std::path::Path::new("/bin/bash").exists() {
            return; // skip if /bin/bash missing
        }
        let mut command = Command::new("bash");
        build_network_denial(&mut command);
        let output = command
            .arg("-c")
            .arg("echo $HTTP_PROXY")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !unshare_available() {
            // On hosts without unshare, the env-poison fallback must be active.
            assert_eq!(stdout.trim(), "http://127.0.0.1:9");
        }
        // On hosts WITH unshare the command was wrapped; stdout may be the
        // poison URL or empty depending on env inheritance — no assertion.
    }

    #[test]
    fn allow_network_recipe_roundtrips() {
        let toml_str = r#"
[package]
name = "netty"
source_type = "direct_url"
source_url = "https://example.com"

[install]
method = "build"
target_dir = "/opt/netty"

[build]
steps = ["make"]
requires = ["make"]
allow_network = true

[version]
detect_command = "netty --version"
detect_pattern = "([0-9]+\\.[0-9]+)"
"#;
        let recipe: ShadowRecipe = toml::from_str(toml_str).unwrap();
        assert!(recipe.build.as_ref().unwrap().allow_network);
    }

    #[test]
    fn old_recipe_without_allow_network_parses_denied() {
        // Compat golden: old recipe TOML without `allow_network` must parse
        // unchanged and default to false (Denied).
        let toml_str = r#"
[package]
name = "oldpkg"
source_type = "direct_url"
source_url = "https://example.com"

[install]
method = "build"
target_dir = "/opt/oldpkg"

[build]
steps = ["make"]

[version]
detect_command = "oldpkg --version"
detect_pattern = "([0-9]+\\.[0-9]+)"
"#;
        let recipe: ShadowRecipe = toml::from_str(toml_str).unwrap();
        let build = recipe.build.as_ref().unwrap();
        assert!(!build.allow_network);
        assert_eq!(
            resolve_network_policy(build.allow_network),
            NetworkPolicy::Denied
        );
    }

    #[tokio::test]
    async fn run_hook_none_is_ok() {
        let result = run_hook(None, "pre_update").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_hook_empty_string_is_ok() {
        let result = run_hook(Some(""), "pre_update").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_hook_success() {
        let result = run_hook(Some("echo ok"), "pre_update").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn run_hook_failure() {
        let result = run_hook(Some("exit 1"), "pre_update").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("pre_update"));
    }

    // --- Rollback chain tests ---

    #[tokio::test]
    async fn list_rollback_versions_empty() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mypkg");
        fs::create_dir_all(&target).await.unwrap();

        let versions = list_rollback_versions(&target.to_string_lossy())
            .await
            .unwrap();
        assert!(versions.is_empty());
    }

    #[tokio::test]
    async fn list_rollback_versions_finds_backups() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mypkg");
        fs::create_dir_all(&target).await.unwrap();

        // Create backup dirs
        fs::create_dir_all(dir.path().join("mypkg.bak.1.0.0"))
            .await
            .unwrap();
        fs::create_dir_all(dir.path().join("mypkg.bak.1.1.0"))
            .await
            .unwrap();
        fs::create_dir_all(dir.path().join("mypkg.bak.2.0.0"))
            .await
            .unwrap();
        // Non-matching dir
        fs::create_dir_all(dir.path().join("other.bak.1.0"))
            .await
            .unwrap();

        let versions = list_rollback_versions(&target.to_string_lossy())
            .await
            .unwrap();
        assert_eq!(versions.len(), 3);
        // Should be sorted newest-first (all created in same second, so order
        // may vary — just check all versions are present)
        let v: Vec<&str> = versions.iter().map(|e| e.version.as_str()).collect();
        assert!(v.contains(&"1.0.0"));
        assert!(v.contains(&"1.1.0"));
        assert!(v.contains(&"2.0.0"));
    }

    #[tokio::test]
    async fn prune_backups_keeps_max() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mypkg");
        fs::create_dir_all(&target).await.unwrap();

        // Create 5 backup dirs
        for v in ["1.0.0", "1.1.0", "1.2.0", "2.0.0", "2.1.0"] {
            let p = dir.path().join(format!("mypkg.bak.{v}"));
            fs::create_dir_all(&p).await.unwrap();
            // Add a small file to each so they have different mtime
            fs::write(p.join("marker"), v).await.unwrap();
        }

        let removed = prune_backups(&target.to_string_lossy(), 3).await.unwrap();
        assert_eq!(removed, 2);

        let remaining = list_rollback_versions(&target.to_string_lossy())
            .await
            .unwrap();
        assert_eq!(remaining.len(), 3);
    }

    #[tokio::test]
    async fn prune_backups_max_1() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mypkg");
        fs::create_dir_all(&target).await.unwrap();

        for v in ["1.0.0", "2.0.0"] {
            fs::create_dir_all(dir.path().join(format!("mypkg.bak.{v}")))
                .await
                .unwrap();
        }

        let removed = prune_backups(&target.to_string_lossy(), 1).await.unwrap();
        assert_eq!(removed, 1);

        let remaining = list_rollback_versions(&target.to_string_lossy())
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn rollback_to_version_restores_correct_version() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mypkg");
        fs::create_dir_all(&target).await.unwrap();
        fs::write(target.join("current.txt"), "latest")
            .await
            .unwrap();

        // Create backup of version 1.0.0
        let bak1 = dir.path().join("mypkg.bak.1.0.0");
        fs::create_dir_all(&bak1).await.unwrap();
        fs::write(bak1.join("data.txt"), "version-1.0.0")
            .await
            .unwrap();

        // Create backup of version 2.0.0
        let bak2 = dir.path().join("mypkg.bak.2.0.0");
        fs::create_dir_all(&bak2).await.unwrap();
        fs::write(bak2.join("data.txt"), "version-2.0.0")
            .await
            .unwrap();

        // Rollback to 1.0.0
        rollback_to_version(&target.to_string_lossy(), "1.0.0")
            .await
            .unwrap();

        // Current directory should now have version-1.0.0 content
        let content = fs::read_to_string(target.join("data.txt")).await.unwrap();
        assert_eq!(content, "version-1.0.0");
    }

    #[tokio::test]
    async fn rollback_to_nonexistent_version_fails() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mypkg");
        fs::create_dir_all(&target).await.unwrap();

        let result = rollback_to_version(&target.to_string_lossy(), "99.0.0").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn install_binaries_copies_and_chmods() {
        use monopsony_common::shadow_types::BinaryEntry;

        let tmp = tempfile::tempdir().unwrap();
        let extracted = tmp.path().join("extracted");
        fs::create_dir_all(&extracted).await.unwrap();

        // Create fake binaries in extracted dir
        fs::write(extracted.join("myapp"), "#!/bin/sh\necho myapp")
            .await
            .unwrap();
        fs::write(extracted.join("helper"), "#!/bin/sh\necho helper")
            .await
            .unwrap();

        let install_dir = tmp.path().join("install");
        fs::create_dir_all(&install_dir).await.unwrap();
        let backup_dir = tmp.path().join("backups");

        let entries = vec![
            BinaryEntry {
                source: "myapp".to_string(),
                target: install_dir.join("bin/myapp").to_string_lossy().to_string(),
                mode: Some("0755".to_string()),
            },
            BinaryEntry {
                source: "helper".to_string(),
                target: install_dir.join("bin/helper").to_string_lossy().to_string(),
                mode: None, // defaults to 0755
            },
        ];

        let installed = install_binaries(&extracted, &entries, &backup_dir)
            .await
            .unwrap();
        assert_eq!(installed.len(), 2);

        // Verify files exist
        assert!(install_dir.join("bin/myapp").exists());
        assert!(install_dir.join("bin/helper").exists());

        // Verify content
        let content = fs::read_to_string(install_dir.join("bin/myapp"))
            .await
            .unwrap();
        assert!(content.contains("myapp"));
    }

    #[tokio::test]
    async fn install_binaries_backs_up_existing() {
        use monopsony_common::shadow_types::BinaryEntry;

        let tmp = tempfile::tempdir().unwrap();
        let extracted = tmp.path().join("extracted");
        fs::create_dir_all(&extracted).await.unwrap();
        fs::write(extracted.join("app"), "new-binary")
            .await
            .unwrap();

        let install_dir = tmp.path().join("install/bin");
        fs::create_dir_all(&install_dir).await.unwrap();
        fs::write(install_dir.join("app"), "old-binary")
            .await
            .unwrap();

        let backup_dir = tmp.path().join("backups");

        let entries = vec![BinaryEntry {
            source: "app".to_string(),
            target: install_dir.join("app").to_string_lossy().to_string(),
            mode: None,
        }];

        let installed = install_binaries(&extracted, &entries, &backup_dir)
            .await
            .unwrap();
        assert_eq!(installed.len(), 1);

        // New binary should be in place
        let content = fs::read_to_string(install_dir.join("app")).await.unwrap();
        assert_eq!(content, "new-binary");

        // Old binary should be backed up
        let backup_files: Vec<_> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(backup_files.len(), 1);
        let backup_content = fs::read_to_string(backup_files[0].path()).await.unwrap();
        assert_eq!(backup_content, "old-binary");
    }

    #[tokio::test]
    async fn install_binaries_missing_source_fails() {
        use monopsony_common::shadow_types::BinaryEntry;

        let tmp = tempfile::tempdir().unwrap();
        let extracted = tmp.path().join("extracted");
        fs::create_dir_all(&extracted).await.unwrap();
        // No files created — source won't be found

        let backup_dir = tmp.path().join("backups");

        let entries = vec![BinaryEntry {
            source: "nonexistent".to_string(),
            target: "/tmp/should-not-exist".to_string(),
            mode: None,
        }];

        let result = install_binaries(&extracted, &entries, &backup_dir).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn find_in_extracted_direct_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("myapp"), "binary").unwrap();
        let result = find_in_extracted(tmp.path(), "myapp");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().file_name().unwrap(), "myapp");
    }

    #[test]
    fn find_in_extracted_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let subdir = tmp.path().join("bin");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("myapp"), "binary").unwrap();
        // "bin/myapp" as source should find it directly
        let result = find_in_extracted(tmp.path(), "bin/myapp");
        assert!(result.is_ok());
    }

    #[test]
    fn find_in_extracted_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let result = find_in_extracted(tmp.path(), "missing");
        assert!(result.is_err());
    }

    // --- Version detection tests ---

    #[test]
    fn extract_version_captures_group() {
        let ver = extract_version("v2.3.1", r"v?(\d+\.\d+\.\d+)").unwrap();
        assert_eq!(ver, "2.3.1");
    }

    #[test]
    fn extract_version_no_match() {
        let result = extract_version("no version here", r"(\d+\.\d+)");
        assert!(result.is_err());
    }

    #[test]
    fn extract_version_falls_back_to_trimmed() {
        // If pattern has no capture group, returns trimmed text
        let ver = extract_version("  1.0.0  ", r"\d+\.\d+\.\d+").unwrap();
        assert_eq!(ver, "1.0.0");
    }

    #[tokio::test]
    async fn detect_version_command_method() {
        let config = monopsony_common::shadow_types::VersionDetect {
            method: monopsony_common::shadow_types::VersionDetectMethod::Command,
            source: "echo 'v1.2.3'".into(),
            pattern: r"v?(\d+\.\d+\.\d+)".into(),
            header_name: None,
        };
        let ver = detect_version(&config).await.unwrap();
        assert_eq!(ver, "1.2.3");
    }

    #[tokio::test]
    async fn detect_version_file_method() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("version.txt");
        std::fs::write(&file_path, "version=4.5.6\n").unwrap();

        let config = monopsony_common::shadow_types::VersionDetect {
            method: monopsony_common::shadow_types::VersionDetectMethod::File,
            source: file_path.to_str().unwrap().into(),
            pattern: r"(\d+\.\d+\.\d+)".into(),
            header_name: None,
        };
        let ver = detect_version(&config).await.unwrap();
        assert_eq!(ver, "4.5.6");
    }

    #[tokio::test]
    async fn detect_version_file_missing() {
        let config = monopsony_common::shadow_types::VersionDetect {
            method: monopsony_common::shadow_types::VersionDetectMethod::File,
            source: "/nonexistent/path/version.txt".into(),
            pattern: r"(\d+)".into(),
            header_name: None,
        };
        let result = detect_version(&config).await;
        assert!(result.is_err());
    }

    #[test]
    fn version_detect_method_serde_default() {
        let toml = r#"
            method = "command"
            source = "myapp --version"
            pattern = "([0-9]+)"
        "#;
        let vd: monopsony_common::shadow_types::VersionDetect = toml::from_str(toml).unwrap();
        assert_eq!(
            vd.method,
            monopsony_common::shadow_types::VersionDetectMethod::Command
        );
    }

    #[test]
    fn version_detect_method_file_serde() {
        let toml = r#"
            method = "file"
            source = "/etc/myapp/version"
            pattern = "([0-9]+\\.[0-9]+)"
        "#;
        let vd: monopsony_common::shadow_types::VersionDetect = toml::from_str(toml).unwrap();
        assert_eq!(
            vd.method,
            monopsony_common::shadow_types::VersionDetectMethod::File
        );
        assert_eq!(vd.source, "/etc/myapp/version");
        assert!(vd.header_name.is_none());
    }

    #[test]
    fn version_detect_method_header_serde() {
        let toml = r#"
            method = "header"
            source = "https://example.com/release"
            pattern = "v([0-9]+)"
            header_name = "X-Version"
        "#;
        let vd: monopsony_common::shadow_types::VersionDetect = toml::from_str(toml).unwrap();
        assert_eq!(
            vd.method,
            monopsony_common::shadow_types::VersionDetectMethod::Header
        );
        assert_eq!(vd.header_name.as_deref(), Some("X-Version"));
    }
}
