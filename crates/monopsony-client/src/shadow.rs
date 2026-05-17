//! Client-side shadow package operations: download, extract, health check, rollback.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::Regex;
use tokio::fs;
use tokio::process::Command;
use tracing::{debug, info, warn};

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
pub async fn verify_version(detect_command: &str, detect_pattern: &str) -> Result<String, ShadowClientError> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(detect_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| ShadowClientError::VerificationFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(ShadowClientError::VerificationFailed(
            format!("command exited with {}", output.status),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pat = Regex::new(detect_pattern)
        .map_err(|e| ShadowClientError::VerificationFailed(e.to_string()))?;

    let caps = pat
        .captures(&stdout)
        .ok_or_else(|| ShadowClientError::VersionMismatch {
            expected: detect_pattern.to_string(),
            got: stdout.trim().to_string(),
        })?;

    let version = caps
        .get(1)
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| stdout.trim().to_string());

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

/// Extract a tar.gz archive to a temp directory
pub async fn extract_to_temp(archive_path: &Path) -> Result<PathBuf, ShadowClientError> {
    let temp_dir = archive_path.parent().unwrap_or(Path::new("/tmp")).join("extracted");
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
                return Err(ShadowClientError::VerificationFailed(
                    format!("expected file not found: {}", bin_path.display()),
                ));
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
fn copy_dir_recursive<'a>(src: &'a Path, dst: &'a Path) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ShadowClientError>> + 'a>> {
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

/// Execute build steps sequentially in the source directory.
/// Variables: {src_dir}, {target_dir}, {version}, {arch}
pub async fn execute_build(
    steps: &[String],
    src_dir: &Path,
    target_dir: &str,
    version: &str,
    arch: &str,
) -> Result<(), ShadowClientError> {
    for (i, step) in steps.iter().enumerate() {
        let cmd = step
            .replace("{src_dir}", &src_dir.to_string_lossy())
            .replace("{target_dir}", target_dir)
            .replace("{version}", version)
            .replace("{arch}", arch);

        let output = Command::new("bash")
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
) -> Result<(), ShadowClientError> {
    let cmd = script
        .replace("{version}", version)
        .replace("{arch}", arch)
        .replace("{os}", os);

    let output = Command::new("bash")
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
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let result = verify_version(
            &format!("{}", script.display()),
            r"(\d+\.\d+\.\d+\.\d+)",
        )
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

        let content = fs::read_to_string(target.join("config.txt"))
            .await
            .unwrap();
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

        execute_build(&steps, &src, &dst.to_string_lossy(), "1.0.0", "x86_64")
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
        let result = execute_build(&steps, &src, "/tmp", "1.0.0", "x86_64").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("step 1"));
    }

    #[tokio::test]
    async fn execute_script_with_env() {
        let tmp = tempfile::tempdir().unwrap();
        let outfile = tmp.path().join("env_output.txt");

        let script = format!("echo $MONOPSONY_VERSION > {}", outfile.to_string_lossy());
        execute_script(&script, "2.0.0", "x86_64", "linux")
            .await
            .unwrap();

        let content = fs::read_to_string(&outfile).await.unwrap();
        assert!(content.trim() == "2.0.0");
    }

    #[tokio::test]
    async fn execute_script_fails() {
        let result = execute_script("exit 42", "1.0", "x86_64", "linux").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("42"));
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
}
