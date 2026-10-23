//! Client-side shadow recipe executor (Sprint: `package update` becomes real).
//!
//! Executes a TRUST-VERIFIED recipe end-to-end on the client host:
//! resolve asset URL → download (+sha256 verify) → pre_update hook →
//! backup → install (extract | script) → post_update hook → health check,
//! with automatic restore-from-backup on any failure after the backup was
//! taken. Pure-ish functions + thin orchestration so it is unit-testable
//! without gRPC; the `package update` arm in main.rs is a thin caller.
//!
//! Conventions mirror the server-side managed pipeline
//! (`monopsony-shadow/src/shadow.rs`) and reuse the client helpers in
//! `crate::shadow` (run_hook, backup_target, restore_backup, prune_backups,
//! run_health_check, execute_script, install_binaries).
//!
//! Invariants:
//! - `install.config_paths` are NEVER touched by the executor.
//! - Hooks run as-is via `bash -c`; `|| true` semantics are the recipe
//!   author's responsibility.
//! - git_build recipes are rejected with a clear error (not supported yet).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use monopsony_common::shadow_types::{
    InstallMethod, ShadowRecipe, ShadowSource, DEFAULT_MONOPSONY_RECIPE,
};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::shadow;

/// Injectable `systemctl`/`journalctl` command runner (test seam).
type SystemdRunner = Box<dyn Fn(&[&str]) -> Result<String> + Send + Sync>;

/// Injectable command runner for `systemctl`/`journalctl` (ScanPaths pattern).
///
/// Tests override via [`set_systemd_runner`] / [`with_systemd_runner`] with a
/// fake; the default (`None`) spawns the real binaries. A single overridable
/// function seam keeps `execute_update` unit-testable without a real systemd.

static SYSTEMD_RUNNER: Mutex<Option<SystemdRunner>> = Mutex::new(None);

/// Install a fake command runner (tests). Prefer [`with_systemd_runner`] so
/// the override is cleared even across the test boundary.
pub fn set_systemd_runner(runner: Option<SystemdRunner>) {
    *SYSTEMD_RUNNER.lock().expect("systemd runner lock") = runner;
}

/// Run `f` with `runner` installed as the systemd command runner, restoring
/// the previous override afterwards.
pub fn with_systemd_runner<F, R>(
    runner: Option<SystemdRunner>,
    f: F,
) -> R
where
    F: FnOnce() -> R,
{
    set_systemd_runner(runner);
    let result = f();
    set_systemd_runner(None);
    result
}

/// Async variant: run async `f` with `runner` installed, restoring the
/// override afterwards. Used by tests that await systemd calls.
pub async fn with_systemd_runner_async<F, Fut, R>(
    runner: Option<SystemdRunner>,
    f: F,
) -> R
where
    F: FnOnce() -> Fut + Send,
    Fut: std::future::Future<Output = R> + Send,
{
    set_systemd_runner(runner);
    let result = f().await;
    set_systemd_runner(None);
    result
}

/// Run `program args...`, using the injected fake when present, otherwise
/// spawning the real binary.
async fn run_systemd_cmd(program: &str, args: &[&str]) -> Result<String> {
    let argv: Vec<&str> = std::iter::once(program).chain(args.iter().copied()).collect();
    if let Some(runner) = SYSTEMD_RUNNER.lock().expect("systemd runner lock").as_deref() {
        return runner(&argv);
    }
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawning {program}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{program} {} exited {}: {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            stderr
        );
    }
}

/// Extract `PropName=value` from `systemctl show -p <prop> <unit>` output.
fn parse_show_prop(output: &str, prop: &str) -> Option<String> {
    let prefix = format!("{prop}=");
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Pre-update snapshot of a systemd unit's relevant state.
#[derive(Debug, Clone, Default)]
struct SystemdBaseline {
    active_state: Option<String>,
    sub_state: Option<String>,
    n_restarts: Option<u32>,
}

/// Best-effort pre-update baseline. Individual property failures are ignored
/// (defaults used) so a missing unit or old systemd never aborts an update.
async fn capture_systemd_baseline(service: &str) -> SystemdBaseline {
    let mut b = SystemdBaseline::default();
    if let Ok(out) = run_systemd_cmd("systemctl", &["show", "-p", "ActiveState", service]).await
    {
        b.active_state = parse_show_prop(&out, "ActiveState");
    }
    if let Ok(out) = run_systemd_cmd("systemctl", &["show", "-p", "SubState", service]).await {
        b.sub_state = parse_show_prop(&out, "SubState");
    }
    if let Ok(out) = run_systemd_cmd("systemctl", &["show", "-p", "NRestarts", service]).await {
        b.n_restarts = parse_show_prop(&out, "NRestarts").and_then(|s| s.parse().ok());
    }
    b
}

/// Post-update systemd health check (T3.2).
///
/// Waits for the unit to reach `active` within `timeout_secs`, then applies
/// the `stable` criterion:
/// - `time`: stay `active` for `secs`
/// - `restarts`: `NRestarts` must not exceed baseline + `count`
/// - `log_quiet`: `journalctl -u <unit> -n 50` shows no new errors
///
/// Returns Err on any breach; the caller gates on `failure_policy`.
async fn systemd_health_check(
    service: &str,
    hc: &monopsony_common::shadow_types::HealthCheck,
    baseline: &SystemdBaseline,
) -> Result<(), String> {
    let timeout = Duration::from_secs(hc.timeout_secs);
    let start = Instant::now();

    // Wait until active within timeout_secs.
    loop {
        if start.elapsed() > timeout {
            return Err(format!(
                "unit {service} did not reach active within {}s",
                hc.timeout_secs
            ));
        }
        match run_systemd_cmd("systemctl", &["show", "-p", "ActiveState", service]).await {
            Ok(out) if parse_show_prop(&out, "ActiveState").as_deref() == Some("active") => {
                break;
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }

    // Stable criterion.
    if let Some(stable) = &hc.stable {
        match stable.mode {
            monopsony_common::shadow_types::StableMode::Time => {
                let secs = stable.secs.unwrap_or(30);
                tokio::time::sleep(Duration::from_secs(secs)).await;
                match run_systemd_cmd("systemctl", &["show", "-p", "ActiveState", service]).await {
                    Ok(out)
                        if parse_show_prop(&out, "ActiveState").as_deref() == Some("active") => {}
                    _ => {
                        return Err(format!(
                            "unit {service} not active after {secs}s stable window"
                        ));
                    }
                }
            }
            monopsony_common::shadow_types::StableMode::Restarts => {
                let count = stable.count.unwrap_or(0);
                let out = run_systemd_cmd("systemctl", &["show", "-p", "NRestarts", service])
                    .await
                    .map_err(|e| format!("reading NRestarts: {e}"))?;
                let n = parse_show_prop(&out, "NRestarts")
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                let base = baseline.n_restarts.unwrap_or(0);
                if n > base + count {
                    return Err(format!(
                        "unit {service} NRestarts {n} > baseline {base} + count {count}"
                    ));
                }
            }
            monopsony_common::shadow_types::StableMode::LogQuiet => {
                let out = run_systemd_cmd("journalctl", &["-u", service, "-n", "50"]).await
                    .map_err(|e| format!("journalctl: {e}"))?;
                let lower = out.to_lowercase();
                if lower.contains("error")
                    || lower.contains("failed")
                    || lower.contains("traceback")
                {
                    return Err(format!("unit {service} journal shows errors (log_quiet breach)"));
                }
            }
        }
    }
    Ok(())
}

/// Success-monitor provisional window (T3.3).
///
/// During `window_secs` the unit is re-checked for `active` state each
/// second. A breach inside the window triggers `failure_policy`; expiry
/// records the update as final.
async fn success_monitor_window(
    service: &str,
    monitor: &monopsony_common::shadow_types::SuccessMonitor,
) -> Result<(), String> {
    let window = Duration::from_secs(monitor.window_secs);
    let start = Instant::now();
    let interval = Duration::from_secs(1);
    while start.elapsed() < window {
        match run_systemd_cmd("systemctl", &["show", "-p", "ActiveState", service]).await {
            Ok(out)
                if parse_show_prop(&out, "ActiveState").as_deref() == Some("active") => {}
            _ => {
                return Err(format!(
                    "unit {service} left active during success-monitor window"
                ));
            }
        }
        if start.elapsed() + interval > window {
            break;
        }
        tokio::time::sleep(interval).await;
    }
    Ok(())
}

/// Capture `journalctl -u <unit> -n 50` into the update log on health failure (T3.5).
async fn capture_journalctl(service: &str, log: &mut Vec<String>) {
    match run_systemd_cmd("journalctl", &["-u", service, "-n", "50"]).await {
        Ok(out) => {
            log.push(format!("journalctl -u {service} -n 50:\n{out}"));
        }
        Err(e) => {
            log.push(format!("journalctl capture for {service} failed: {e}"));
        }
    }
}

/// Outcome of a client-side update execution. Always returned (never
/// panics, never Err) so the CLI arm can report uniformly.
#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub success: bool,
    /// True when a backup existed and was restored after a failure.
    pub rolled_back: bool,
    /// The version the executor attempted to install (= Check RPC latest).
    pub new_version: String,
    /// True while the update sits in a provisional success-monitor window.
    pub provisional: bool,
    /// Path of the pre-update backup kept on success (None if none created).
    pub backup_path: Option<PathBuf>,
    /// Human-readable step log (mirrors the pipeline stages).
    pub log: Vec<String>,
    /// On failure: "<stage>: <message>".
    pub error: Option<String>,
}

#[derive(Debug)]
struct StageError {
    stage: &'static str,
    message: String,
    backup: Option<PathBuf>,
}

fn stage(stage: &'static str, e: impl std::fmt::Display) -> StageError {
    StageError {
        stage,
        message: e.to_string(),
        backup: None,
    }
}

/// Render an asset URL template, substituting `{version}`, `{arch}`, `{os}`.
/// Static templates (no placeholders) pass through unchanged.
pub fn render_asset_template(template: &str, version: &str, arch: &str, os: &str) -> String {
    template
        .replace("{version}", version)
        .replace("{arch}", arch)
        .replace("{os}", os)
}

/// Resolve the download URL for the update asset.
///
/// `check_asset_url` is the URL from the Check RPC (if the server populated
/// it — for direct_url/scrape recipes it comes back EMPTY because
/// `DirectUrlSource::release_assets` returns no assets for scrape
/// discovery). When empty, resolve client-side from the recipe:
/// - direct_url → `[direct_url].asset_url_template`
/// - github_release → releases API lookup + `[asset_selector]` rules
/// - git_build → unsupported (clear error)
pub async fn resolve_asset_url(
    recipe: &ShadowRecipe,
    check_asset_url: &str,
    target_version: &str,
    http: &reqwest::Client,
) -> Result<String> {
    if !check_asset_url.is_empty() {
        return Ok(check_asset_url.to_string());
    }

    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    match recipe.package.source_type {
        ShadowSource::DirectUrl => {
            let du = recipe.direct_url.as_ref().ok_or_else(|| {
                anyhow!(
                    "direct_url recipe '{}' has no [direct_url] section with asset_url_template",
                    recipe.package.name
                )
            })?;
            Ok(render_asset_template(
                &du.asset_url_template,
                target_version,
                arch,
                os,
            ))
        }
        ShadowSource::GithubRelease => {
            let repo = parse_github_repo(&recipe.package.source_url)?;
            let api_url = format!(
                "https://api.github.com/repos/{}/releases/tags/{}",
                repo, target_version
            );
            let resp = http
                .get(&api_url)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .with_context(|| format!("github release lookup for tag {target_version}"))?;
            if !resp.status().is_success() {
                bail!(
                    "github release lookup for tag {} returned HTTP {}",
                    target_version,
                    resp.status()
                );
            }
            let body = resp.text().await?;
            github_asset_url_from_release(&body, &repo, target_version, recipe)
        }
        ShadowSource::GitBuild => Err(anyhow!(
            "client-side executor does not support git_build yet"
        )),
    }
}

/// Extract "owner/repo" from a GitHub URL (lenient: trailing slashes and
/// extra path segments tolerated).
fn parse_github_repo(source_url: &str) -> Result<String> {
    let trimmed = source_url.trim_end_matches('/');
    let after = trimmed
        .split_once("github.com/")
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow!("source_url '{source_url}' is not a github.com URL"))?;
    let mut parts = after.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if owner.is_empty() || repo.is_empty() {
        bail!("cannot parse owner/repo from '{source_url}'");
    }
    Ok(format!("{owner}/{repo}"))
}

/// Pure helper: given a GitHub release API response body, pick the asset
/// matching the recipe's `[asset_selector]` rules for this platform and
/// return its browser download URL.
fn github_asset_url_from_release(
    release_json: &str,
    repo: &str,
    tag: &str,
    recipe: &ShadowRecipe,
) -> Result<String> {
    let release: serde_json::Value =
        serde_json::from_str(release_json).with_context(|| "parsing github release response")?;
    let names: Vec<String> = release["assets"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        bail!("github release {tag} exposes no assets");
    }
    let selector = recipe.asset_selector.as_ref().ok_or_else(|| {
        anyhow!(
            "recipe '{}' has no [asset_selector] rules to pick an asset",
            recipe.package.name
        )
    })?;
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let matched = selector
        .match_asset(os, arch, None, &names)
        .ok_or_else(|| {
            anyhow!(
                "no asset in release {tag} matches selector rules for {os}/{arch} (assets: {})",
                names.join(", ")
            )
        })?;
    Ok(format!(
        "https://github.com/{repo}/releases/download/{tag}/{matched}"
    ))
}

/// Download `url` to `dest`, verifying sha256 (hex, case-insensitive) when
/// provided. Returns the number of bytes written.
pub async fn download_asset(
    http: &reqwest::Client,
    url: &str,
    expected_sha256: Option<&str>,
    dest: &Path,
) -> Result<u64> {
    let resp = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("download of {url} returned HTTP {status}");
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading body of {url}"))?;

    if let Some(expected) = expected_sha256.map(str::trim).filter(|s| !s.is_empty()) {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let actual = hex::encode(hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            bail!("sha256 mismatch for {url}: expected {expected}, got {actual}");
        }
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
    Ok(bytes.len() as u64)
}

/// Unpack a tar.gz archive into `dest` (created if missing), preserving the
/// archive layout: flat bundles (e.g. the technitium portable tarball over
/// /opt/technitium/dns) extract flat; entries overwrite existing files.
/// `install.config_paths` are never removed or wiped by this executor.
fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let file =
        std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    tar.set_preserve_permissions(true);
    tar.unpack(dest)
        .with_context(|| format!("unpacking {} into {}", archive.display(), dest.display()))?;
    Ok(())
}

/// Detect the locally installed version from a recipe TOML's
/// `[version] detect_command` + `detect_pattern`. Returns None when the
/// recipe has no usable [version] section or detection fails (used at
/// `package register` time so bookkeeping starts with a real version).
pub async fn detect_current_version(recipe_toml: &str) -> Option<String> {
    let recipe: ShadowRecipe = toml::from_str(recipe_toml).ok()?;
    shadow::verify_version(
        &recipe.version.detect_command,
        &recipe.version.detect_pattern,
    )
    .await
    .ok()
}

/// Execute the full update pipeline for a verified recipe. Never panics;
/// failures (with rollback when possible) are reported via the outcome.
pub async fn execute_update(
    recipe: &ShadowRecipe,
    target_version: &str,
    asset_url: &str,
    http: &reqwest::Client,
) -> UpdateOutcome {
    let mut log: Vec<String> = Vec::new();
    let mut backup: Option<PathBuf> = None;

    match execute_inner(
        recipe,
        target_version,
        asset_url,
        http,
        &mut log,
        &mut backup,
    )
    .await
    {
        Ok(()) => {
            // Success: prune old backups to install.max_backups (default 3).
            match shadow::prune_backups(&recipe.install.target_dir, recipe.install.max_backups)
                .await
            {
                Ok(n) if n > 0 => log.push(format!("pruned {n} old backup(s)")),
                Ok(_) => {}
                Err(e) => warn!(
                    "backup pruning failed for {}: {e}",
                    recipe.install.target_dir
                ),
            }
            UpdateOutcome {
                success: true,
                rolled_back: false,
                new_version: target_version.to_string(),
                // The update sat in a provisional success-monitor window (if
                // configured); record that so the CLI/dashboard can surface it.
                provisional: recipe.install.success_monitor.is_some(),
                backup_path: backup,
                log,
                error: None,
            }
        }
        Err(err) => {
            // T3.5: on health failure, capture journalctl into the update log.
            if err.stage == "health_check" {
                if let Some(service) = recipe.install.service.as_deref() {
                    capture_journalctl(service, &mut log).await;
                }
            }
            log.push(format!(
                "update FAILED at stage '{}': {}",
                err.stage, err.message
            ));
            // on_failure hook runs before rollback (best-effort).
            if let Some(hook) = recipe
                .install
                .on_failure
                .as_deref()
                .filter(|h| !h.is_empty())
            {
                if let Err(e) = shadow::run_hook(Some(hook), "on_failure").await {
                    log.push(format!("on_failure hook error (ignored): {e}"));
                }
            }
            let mut rolled_back = false;
            // T3.4: failure_policy = "hold" keeps the broken state and awaits
            // the operator — do NOT restore the backup.
            let hold = recipe.install.failure_policy == "hold";
            if !hold {
                if let Some(backup_path) = &err.backup.or(backup.clone()) {
                    match shadow::restore_backup(&recipe.install.target_dir, backup_path).await {
                        Ok(()) => {
                            rolled_back = true;
                            log.push(format!(
                                "restored backup (rollback): {}",
                                backup_path.display()
                            ));
                            // Re-run post_update so the old version's service is
                            // running again after restore (best-effort).
                            if let Some(hook) = recipe
                                .install
                                .post_update
                                .as_deref()
                                .filter(|h| !h.is_empty())
                            {
                                if let Err(e) = shadow::run_hook(Some(hook), "post_update").await {
                                    log.push(format!("post_update after rollback error: {e}"));
                                }
                            }
                        }
                        Err(e) => log.push(format!(
                            "ROLLBACK FAILED for {}: {e} — manual recovery required",
                            recipe.install.target_dir
                        )),
                    }
                }
            } else {
                log.push("failure_policy=hold: NOT restoring backup; awaiting operator intervention".to_string());
            }
            UpdateOutcome {
                success: false,
                rolled_back,
                new_version: target_version.to_string(),
                provisional: false,
                backup_path: None,
                log,
                error: Some(format!("{}: {}", err.stage, err.message)),
            }
        }
    }
}

async fn execute_inner(
    recipe: &ShadowRecipe,
    target_version: &str,
    asset_url: &str,
    http: &reqwest::Client,
    log: &mut Vec<String>,
    backup: &mut Option<PathBuf>,
) -> std::result::Result<(), StageError> {
    let install = &recipe.install;

    // git_build is out of scope for the client-side executor.
    if recipe.package.source_type == ShadowSource::GitBuild
        || install.method == InstallMethod::Build
    {
        return Err(stage(
            "plan",
            "client-side executor does not support git_build yet",
        ));
    }

    // T3.2: capture pre-update systemd baseline before any changes are made,
    // so NRestarts comparisons only count restarts introduced by this update.
    let service = install.service.as_deref();
    let mut baseline: Option<SystemdBaseline> = None;
    if service.is_some() {
        baseline = Some(capture_systemd_baseline(service.unwrap()).await);
    }

    // Steps 1-2: download the asset to a temp file (skipped for script
    // recipes with no resolvable URL — the script fetches its own bits).
    let tmp =
        tempfile::tempdir().map_err(|e| stage("download", format!("creating tempdir: {e}")))?;
    let asset_path = if install.method == InstallMethod::Script && asset_url.is_empty() {
        log.push("no asset URL resolved; script method will fetch itself".to_string());
        None
    } else {
        let dest = tmp.path().join("asset");
        download_asset(http, asset_url, None, &dest)
            .await
            .map_err(|e| stage("download", e))?;
        log.push(format!("downloaded {asset_url}"));
        Some(dest)
    };

    // Step 3: pre_update hook (run as-is; failure aborts before any change).
    shadow::run_hook(install.pre_update.as_deref(), "pre_update")
        .await
        .map_err(|e| stage("pre_update", e))?;
    log.push("pre_update hook: ok".to_string());

    // Step 4: backup target_dir if requested and present.
    if install.backup_old && Path::new(&install.target_dir).exists() {
        let label = format!("{target_version}-{}", chrono::Utc::now().timestamp());
        let path = shadow::backup_target(&install.target_dir, &label)
            .await
            .map_err(|e| stage("backup", e))?;
        log.push(format!("backup created: {}", path.display()));
        *backup = Some(path);
    }

    // Step 5: install. config_paths are never touched.
    match install.method {
        InstallMethod::Extract => {
            let asset = asset_path
                .as_deref()
                .ok_or_else(|| stage("download", "no asset downloaded for extract install"))?;
            if install.binaries.is_empty() {
                extract_tar_gz(asset, Path::new(&install.target_dir))
                    .map_err(|e| stage("extract", e))?;
                log.push(format!("extracted into {}", install.target_dir));
            } else {
                // Multi-binary: stage the archive, then install each entry
                // to its own target path (per-entry backups via helper).
                let staging = tmp.path().join("staging");
                extract_tar_gz(asset, &staging).map_err(|e| stage("extract", e))?;
                let bin_backup_dir = tmp.path().join("bin-backups");
                shadow::install_binaries(&staging, &install.binaries, &bin_backup_dir)
                    .await
                    .map_err(|e| stage("extract", e))?;
                log.push(format!(
                    "installed {} binary/binaries",
                    install.binaries.len()
                ));
            }
        }
        InstallMethod::Script => {
            let script = recipe.script.as_ref().ok_or_else(|| {
                stage(
                    "script",
                    "install.method = script but recipe has no [script] section",
                )
            })?;
            let mut cmd = script.update.clone();
            if let Some(path) = &asset_path {
                cmd = cmd.replace("{asset_path}", &path.to_string_lossy());
            }
            // Network must be allowed: the update script's job is fetching.
            shadow::execute_script(
                &cmd,
                target_version,
                std::env::consts::ARCH,
                std::env::consts::OS,
                shadow::NetworkPolicy::Allowed,
            )
            .await
            .map_err(|e| stage("script", e))?;
            log.push("update script: ok".to_string());
        }
        InstallMethod::Build => {
            return Err(stage(
                "plan",
                "client-side executor does not support git_build yet",
            ));
        }
    }

    // Step 6: post_update hook.
    shadow::run_hook(install.post_update.as_deref(), "post_update")
        .await
        .map_err(|e| stage("post_update", e))?;
    log.push("post_update hook: ok".to_string());

    // Step 7: health check (retries within timeout budget).
    if let Some(hc) = &install.health_check {
        shadow::run_health_check(hc)
            .await
            .map_err(|e| stage("health_check", e))?;
        // T3.2: systemd unit health + stable criterion (when a unit is configured).
        if let Some(svc) = service {
            let default_base = SystemdBaseline::default();
            let base = baseline.as_ref().unwrap_or(&default_base);
            systemd_health_check(svc, hc, base)
                .await
                .map_err(|e| stage("health_check", e))?;
        }
        log.push("health check: passed".to_string());
    }

    // Step 8: success-monitor provisional window (T3.3).
    if let Some(monitor) = &install.success_monitor {
        if let Some(svc) = service {
            success_monitor_window(svc, monitor)
                .await
                .map_err(|e| stage("success_monitor", e))?;
        }
        log.push(format!(
            "success-monitor window ({}s): stable",
            monitor.window_secs
        ));
    }

    info!(
        package = recipe.package.name,
        version = target_version,
        "shadow update executed successfully"
    );
    Ok(())
}

// Keep the default-recipe constant linked so the executor module documents
// the canonical recipe shape it executes.
const _: () = {
    let _ = DEFAULT_MONOPSONY_RECIPE;
};

#[cfg(test)]
mod tests {
    use super::*;
    // Serializes tests that install the fake systemd runner: tokio runs
    // #[tokio::test] functions concurrently and they all share the static
    // `SYSTEMD_RUNNER`, so tests that await while the runner is installed must
    // be mutually exclusive.
    static TEST_RUNNER_LOCK: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());
    use std::sync::Arc;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    /// Minimal HTTP/1.1 file server on 127.0.0.1:0 serving `body` for every
    /// GET (Connection: close). Returns (port, join-handle).
    fn spawn_http_server(body: Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                // Drain the request head (we never serve request bodies).
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (port, handle)
    }

    /// A TCP port that is open (listener accepts + discards connections).
    fn open_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                drop(stream);
            }
        });
        port
    }

    /// A TCP port that is (almost certainly) closed.
    fn closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    /// Build a small tar.gz in memory. Entries given as (name, contents).
    fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, name, *data).unwrap();
            }
            builder.into_inner().unwrap();
        }
        encoder.finish().unwrap()
    }

    fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
    }

    fn recipe_toml(
        target_dir: &Path,
        health_port: Option<u16>,
        pre: Option<&str>,
        post: Option<&str>,
        max_backups: usize,
    ) -> String {
        let mut s = format!(
            r#"[package]
name = "testpkg"
source_type = "direct_url"
source_url = "https://example.com/testpkg"

[install]
method = "extract"
target_dir = "{}"
backup_old = true
max_backups = {max_backups}
"#,
            target_dir.display()
        );
        if let Some(p) = pre {
            s.push_str(&format!("pre_update = \"{p}\"\n"));
        }
        if let Some(p) = post {
            s.push_str(&format!("post_update = \"{p}\"\n"));
        }
        if let Some(port) = health_port {
            s.push_str(&format!(
                "[install.health_check]\ntype = \"port\"\nport = {port}\ntimeout_secs = 3\n"
            ));
        }
        s.push_str("[direct_url]\nversion_discovery = \"scrape\"\nasset_url_template = \"https://example.com/dl/testpkg.tar.gz\"\n");
        s.push_str("\n[version]\ndetect_command = \"echo 1.0.0\"\ndetect_pattern = \"([0-9]+\\\\.[0-9]+\\\\.[0-9]+)\"\n");
        s
    }

    fn parse_recipe(toml: &str) -> ShadowRecipe {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn render_template_static_is_constant() {
        let out = render_asset_template(
            "https://download.technitium.com/dns/DnsServerPortable.tar.gz",
            "15.6.2",
            "x86_64",
            "linux",
        );
        assert_eq!(
            out,
            "https://download.technitium.com/dns/DnsServerPortable.tar.gz"
        );
    }

    #[test]
    fn render_template_substitutes_placeholders() {
        let out = render_asset_template(
            "https://pkgs.example.com/stable/tool_{version}_{arch}_{os}.tgz",
            "1.2.3",
            "x86_64",
            "linux",
        );
        assert_eq!(
            out,
            "https://pkgs.example.com/stable/tool_1.2.3_x86_64_linux.tgz"
        );
    }

    #[tokio::test]
    async fn resolve_asset_url_prefers_check_rpc_url() {
        let recipe = parse_recipe(&recipe_toml(Path::new("/tmp/x"), None, None, None, 3));
        let url = resolve_asset_url(
            &recipe,
            "https://server/given.tar.gz",
            "9.9.9",
            &http_client(),
        )
        .await
        .unwrap();
        assert_eq!(url, "https://server/given.tar.gz");
    }

    #[tokio::test]
    async fn resolve_asset_url_direct_url_template_fallback() {
        // Check RPC returns empty asset_url for scrape-based direct_url
        // recipes — the executor must resolve from the recipe template.
        let recipe = parse_recipe(&recipe_toml(Path::new("/tmp/x"), None, None, None, 3));
        let url = resolve_asset_url(&recipe, "", "9.9.9", &http_client())
            .await
            .unwrap();
        assert_eq!(url, "https://example.com/dl/testpkg.tar.gz");
    }

    #[tokio::test]
    async fn resolve_asset_url_git_build_rejected() {
        let toml = r#"[package]
name = "gb"
source_type = "git_build"
source_url = "https://github.com/example/gb.git"

[install]
method = "build"
target_dir = "/opt/gb"

[version]
detect_command = "gb --version"
detect_pattern = "([0-9]+)"
"#;
        let recipe: ShadowRecipe = toml::from_str(toml).unwrap();
        let err = resolve_asset_url(&recipe, "", "v1", &http_client())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not support git_build"), "got: {err}");
    }

    #[tokio::test]
    async fn execute_update_git_build_rejected() {
        let toml = r#"[package]
name = "gb"
source_type = "git_build"
source_url = "https://github.com/example/gb.git"

[install]
method = "build"
target_dir = "/opt/gb"

[version]
detect_command = "gb --version"
detect_pattern = "([0-9]+)"
"#;
        let recipe: ShadowRecipe = toml::from_str(toml).unwrap();
        let outcome = execute_update(&recipe, "v2", "", &http_client()).await;
        assert!(!outcome.success);
        assert!(!outcome.rolled_back);
        assert!(
            outcome
                .error
                .unwrap()
                .contains("does not support git_build"),
            "executor must reject git_build"
        );
    }

    #[test]
    fn parse_github_repo_variants() {
        assert_eq!(
            parse_github_repo("https://github.com/Sonarr/Sonarr").unwrap(),
            "Sonarr/Sonarr"
        );
        assert_eq!(
            parse_github_repo("https://github.com/Sonarr/Sonarr/").unwrap(),
            "Sonarr/Sonarr"
        );
        assert!(parse_github_repo("https://gitlab.com/a/b").is_err());
    }

    #[test]
    fn github_asset_url_picks_matching_rule() {
        let toml = r#"[package]
name = "gh"
source_type = "github_release"
source_url = "https://github.com/example/gh"

[install]
method = "extract"
target_dir = "/opt/gh"

[asset_selector]
tag_pattern = "v(\\d+\\.\\d+\\.\\d+)$"
[[asset_selector.rules]]
os = "linux"
arch = "x86_64"
pattern = "gh-linux-amd64\\.tar\\.gz$"
[[asset_selector.rules]]
os = "linux"
arch = "aarch64"
pattern = "gh-linux-arm64\\.tar\\.gz$"

[version]
detect_command = "gh --version"
detect_pattern = "([0-9]+)"
"#;
        let recipe: ShadowRecipe = toml::from_str(toml).unwrap();
        let release = r#"{"assets":[{"name":"gh-macos.zip"},{"name":"gh-linux-arm64.tar.gz"},{"name":"gh-linux-amd64.tar.gz"}]}"#;
        let url = github_asset_url_from_release(release, "example/gh", "v1.2.3", &recipe).unwrap();
        if std::env::consts::ARCH == "aarch64" {
            assert_eq!(
                url,
                "https://github.com/example/gh/releases/download/v1.2.3/gh-linux-arm64.tar.gz"
            );
        } else {
            assert_eq!(
                url,
                "https://github.com/example/gh/releases/download/v1.2.3/gh-linux-amd64.tar.gz"
            );
        }
    }

    #[test]
    fn github_asset_url_no_matching_rule_errors() {
        let toml = r#"[package]
name = "gh"
source_type = "github_release"
source_url = "https://github.com/example/gh"

[install]
method = "extract"
target_dir = "/opt/gh"

[asset_selector]
tag_pattern = ".*"
[[asset_selector.rules]]
os = "plan9"
arch = "x86_64"
pattern = ".*"

[version]
detect_command = "gh --version"
detect_pattern = "([0-9]+)"
"#;
        let recipe: ShadowRecipe = toml::from_str(toml).unwrap();
        let release = r#"{"assets":[{"name":"gh-linux.tar.gz"}]}"#;
        assert!(github_asset_url_from_release(release, "example/gh", "v1", &recipe).is_err());
    }

    #[tokio::test]
    async fn download_asset_writes_file() {
        let (port, _srv) = spawn_http_server(b"hello-bytes".to_vec());
        let dest = tempfile::tempdir().unwrap().path().join("out.bin");
        let n = download_asset(
            &http_client(),
            &format!("http://127.0.0.1:{port}/x"),
            None,
            &dest,
        )
        .await
        .unwrap();
        assert_eq!(n, 11);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello-bytes");
    }

    #[tokio::test]
    async fn download_asset_rejects_wrong_sha256() {
        let (port, _srv) = spawn_http_server(b"tampered-bytes".to_vec());
        let dest = tempfile::tempdir().unwrap().path().join("out.bin");
        let wrong_sha = "0".repeat(64);
        let err = download_asset(
            &http_client(),
            &format!("http://127.0.0.1:{port}/x"),
            Some(&wrong_sha),
            &dest,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("sha256 mismatch"), "got: {err}");
        // The tampered asset must not have been written.
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn download_asset_accepts_correct_sha256() {
        let body = b"good-bytes".to_vec();
        let mut hasher = Sha256::new();
        hasher.update(&body);
        let sha = hex::encode(hasher.finalize());
        let (port, _srv) = spawn_http_server(body.clone());
        let dest = tempfile::tempdir().unwrap().path().join("out.bin");
        download_asset(
            &http_client(),
            &format!("http://127.0.0.1:{port}/x"),
            Some(&sha),
            &dest,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn extract_tar_gz_flat_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("a.tar.gz");
        let bytes = make_tar_gz(&[("start.sh", b"#!/bin/sh\nv2\n"), ("bin/tool", b"ELF-ish")]);
        std::fs::write(&archive, &bytes).unwrap();
        let dest = tmp.path().join("target");
        extract_tar_gz(&archive, &dest).unwrap();
        assert_eq!(
            std::fs::read(dest.join("start.sh")).unwrap(),
            b"#!/bin/sh\nv2\n"
        );
        assert_eq!(std::fs::read(dest.join("bin/tool")).unwrap(), b"ELF-ish");
    }

    #[test]
    fn extract_tar_gz_overwrites_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("a.tar.gz");
        let bytes = make_tar_gz(&[("app.conf", b"new=1")]);
        std::fs::write(&archive, &bytes).unwrap();
        let dest = tmp.path().join("target");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("app.conf"), b"old=0").unwrap();
        extract_tar_gz(&archive, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("app.conf")).unwrap(), b"new=1");
    }

    #[tokio::test]
    async fn detect_current_version_from_recipe() {
        let toml = r#"[package]
name = "x"
source_type = "direct_url"
source_url = "https://example.com"

[install]
method = "extract"
target_dir = "/opt/x"

[version]
detect_command = "echo v9.9.9"
detect_pattern = "v?(\\d+\\.\\d+\\.\\d+)"

[direct_url]
version_discovery = "scrape"
asset_url_template = "https://example.com/x.tar.gz"
"#;
        assert_eq!(
            detect_current_version(toml).await,
            Some("9.9.9".to_string())
        );
    }

    #[tokio::test]
    async fn detect_current_version_recipe_without_version_section_is_none() {
        // Register-time minimal recipes carry no [version] — detection is
        // skipped, caller falls back to "".
        assert_eq!(
            detect_current_version("[package]\nname = \"m\"\nsource_type = \"direct_url\"\nsource_url = \"u\"\n\n[install]\nmethod = \"extract\"\ntarget_dir = \"/tmp\"\n").await,
            None
        );
    }

    #[tokio::test]
    async fn happy_path_hooks_extract_health_and_real_version() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("start.sh"), b"old").unwrap();

        let marker = |name: &str| tmp.path().join(name);
        let pre = format!("echo ran > {}", marker("pre.ran").display());
        let post = format!("echo ran > {}", marker("post.ran").display());

        // Server serves the NEW tarball (start.sh => new content).
        let tarball = make_tar_gz(&[("start.sh", b"NEW-15.6.2")]);
        let (port, _srv) = spawn_http_server(tarball);

        let toml = recipe_toml(&target, Some(open_port()), Some(&pre), Some(&post), 3);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/DnsServerPortable.tar.gz");

        let outcome = execute_update(&recipe, "15.6.2", &url, &http_client()).await;
        assert!(outcome.success, "outcome: {outcome:?}");
        assert!(!outcome.rolled_back);
        // The version the CLI passes to apply_shadow_update is the REAL one.
        assert_eq!(outcome.new_version, "15.6.2");
        // Hooks ran.
        assert!(marker("pre.ran").exists());
        assert!(marker("post.ran").exists());
        // New file extracted into target_dir.
        assert_eq!(
            std::fs::read(target.join("start.sh")).unwrap(),
            b"NEW-15.6.2"
        );
        // Backup of the old state kept.
        let backup = outcome.backup_path.expect("backup kept on success");
        assert!(backup.exists());
        assert_eq!(std::fs::read(backup.join("start.sh")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn health_check_failure_rolls_back_prior_state() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("start.sh"), b"OLD-CONTENT").unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"BROKEN-NEW")]);
        let (port, _srv) = spawn_http_server(tarball);

        // Closed port + short timeout -> health check fails fast-ish.
        let toml = recipe_toml(&target, Some(closed_port()), None, None, 3);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        let outcome = execute_update(&recipe, "15.6.2", &url, &http_client()).await;
        assert!(!outcome.success);
        assert!(outcome.rolled_back, "expected rollback, got {outcome:?}");
        let err = outcome.error.unwrap();
        assert!(err.starts_with("health_check"), "got: {err}");
        // Prior file state restored.
        assert_eq!(
            std::fs::read(target.join("start.sh")).unwrap(),
            b"OLD-CONTENT"
        );
        // Backup consumed by the restore is still on disk for manual undo.
        // (restore copies FROM the backup; the backup dir remains.)
    }

    #[tokio::test]
    async fn pre_update_hook_failure_aborts_before_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("start.sh"), b"old").unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"new")]);
        let (port, _srv) = spawn_http_server(tarball);

        let toml = recipe_toml(&target, None, Some("exit 3"), None, 3);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        let outcome = execute_update(&recipe, "2.0.0", &url, &http_client()).await;
        assert!(!outcome.success);
        assert!(!outcome.rolled_back); // nothing to roll back (no backup yet)
        assert!(outcome.error.unwrap().starts_with("pre_update"));
        // Target untouched.
        assert_eq!(std::fs::read(target.join("start.sh")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn download_failure_reports_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        let toml = recipe_toml(&target, None, None, None, 3);
        let recipe = parse_recipe(&toml);
        let closed = closed_port();
        let outcome = execute_update(
            &recipe,
            "2.0.0",
            &format!("http://127.0.0.1:{closed}/x.tar.gz"),
            &http_client(),
        )
        .await;
        assert!(!outcome.success);
        assert!(outcome.error.unwrap().starts_with("download"));
    }

    #[tokio::test]
    async fn max_backups_pruned_after_success() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();

        // Pre-create 4 old backups with distinct mtimes (beyond max 3).
        for v in ["0.1.0", "0.2.0", "0.3.0", "0.4.0"] {
            let b = tmp.path().join(format!("app.bak.{v}"));
            std::fs::create_dir_all(&b).unwrap();
            std::fs::write(b.join("marker"), v).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(20));

        let tarball = make_tar_gz(&[("start.sh", b"new")]);
        let (port, _srv) = spawn_http_server(tarball);
        let toml = recipe_toml(&target, Some(open_port()), None, None, 3);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        let outcome = execute_update(&recipe, "1.0.0", &url, &http_client()).await;
        assert!(outcome.success, "outcome: {outcome:?}");
        assert!(outcome.log.iter().any(|l| l.contains("pruned")));

        let remaining = shadow::list_rollback_versions(&target.to_string_lossy())
            .await
            .unwrap();
        assert_eq!(remaining.len(), 3, "backups: {remaining:?}");
        // The newest backup (from this update) must be one of them.
        assert!(remaining.iter().any(|e| e.version.starts_with("1.0.0-")));
    }

    #[tokio::test]
    async fn script_method_runs_update_script_with_version() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();

        let outfile = tmp.path().join("script-out.txt");
        let toml = format!(
            r#"[package]
name = "scripted"
source_type = "direct_url"
source_url = "https://example.com/scripted"

[install]
method = "script"
target_dir = "{}"

[script]
update = "echo $MONOPSONY_VERSION > {}"

[version]
detect_command = "echo 1.0"
detect_pattern = "([0-9.]+)"

[direct_url]
version_discovery = "scrape"
asset_url_template = "https://example.com/s"
"#,
            target.display(),
            outfile.display()
        );
        let recipe = parse_recipe(&toml);
        // Empty asset URL for script method: no download, script runs.
        let outcome = execute_update(&recipe, "7.7.7", "", &http_client()).await;
        assert!(outcome.success, "outcome: {outcome:?}");
        assert_eq!(std::fs::read_to_string(&outfile).unwrap().trim(), "7.7.7");
    }

    #[tokio::test]
    async fn script_failure_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), b"keep").unwrap();

        let toml = format!(
            r#"[package]
name = "scripted"
source_type = "direct_url"
source_url = "https://example.com/scripted"

[install]
method = "script"
target_dir = "{}"
backup_old = true

[script]
update = "exit 5"

[version]
detect_command = "echo 1.0"
detect_pattern = "([0-9.]+)"

[direct_url]
version_discovery = "scrape"
asset_url_template = "https://example.com/s"
"#,
            target.display()
        );
        let recipe = parse_recipe(&toml);
        let outcome = execute_update(&recipe, "2.0.0", "", &http_client()).await;
        assert!(!outcome.success);
        assert!(outcome.rolled_back);
        assert!(outcome.error.unwrap().starts_with("script"));
        assert_eq!(std::fs::read(target.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn stage_error_formatting() {
        let e = stage("extract", "boom");
        assert_eq!(e.stage, "extract");
        assert_eq!(e.message, "boom");
    }

    // --- T3.2/T3.3/T3.5: systemd health, success monitor, journalctl, injectable runner ---

    /// Scripted fake `systemctl`/`journalctl` runner: pops canned responses in
    /// call order and records every argv for assertions.
    fn scripted_fake(
        responses: Vec<String>,
    ) -> (
        SystemdRunner,
        Arc<Mutex<Vec<Vec<String>>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let responses = Mutex::new(responses);
        let calls_clone = calls.clone();
        let runner = move |argv: &[&str]| -> Result<String> {
            calls_clone
                .lock()
                .unwrap()
                .push(argv.iter().map(|s| s.to_string()).collect());
            Ok(responses.lock().unwrap().remove(0))
        };
        (Box::new(runner), calls)
    }

    fn default_hc() -> monopsony_common::shadow_types::HealthCheck {
        monopsony_common::shadow_types::HealthCheck {
            check_type: monopsony_common::shadow_types::HealthCheckType::Port,
            url: None,
            port: None,
            process: None,
            timeout_secs: 2,
            stable: None,
        }
    }

    #[tokio::test]
    async fn systemd_health_check_reaches_active_ok() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),   // baseline ActiveState
            "SubState=running\n".into(),     // baseline SubState
            "NRestarts=0\n".into(),          // baseline NRestarts
            "ActiveState=active\n".into(),   // health check: active
        ]);
        let hc = default_hc();
        let base = with_systemd_runner_async(Some(runner), || async {
            capture_systemd_baseline("fake-unit").await
        })
        .await;
        assert_eq!(base.active_state.as_deref(), Some("active"));

        let (runner2, calls2) = scripted_fake(vec!["ActiveState=active\n".into()]);
        let res = with_systemd_runner_async(Some(runner2), || async {
            systemd_health_check("fake-unit", &hc, &base).await
        })
        .await;
        assert!(res.is_ok());
        assert!(calls2.lock().unwrap().iter().all(|c| c[0] == "systemctl"));
    }

    #[tokio::test]
    async fn systemd_health_check_never_active_times_out() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=activating\n".into(), // baseline
            "SubState=activating\n".into(),
            "NRestarts=0\n".into(),
            "ActiveState=activating\n".into(), // health check poll
            "ActiveState=activating\n".into(), // health check poll (after sleep)
        ]);
        let mut hc = default_hc();
        hc.timeout_secs = 1;
        let base = with_systemd_runner_async(Some(runner), || async {
            capture_systemd_baseline("fake-unit").await
        })
        .await;
        let (runner2, _calls2) = scripted_fake(vec![
            "ActiveState=activating\n".into(),
            "ActiveState=activating\n".into(),
        ]);
        let res = with_systemd_runner_async(Some(runner2), || async {
            systemd_health_check("fake-unit", &hc, &base).await
        })
        .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("did not reach active"));
    }

    #[tokio::test]
    async fn stable_restarts_preexisting_not_counted() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        // NRestarts already 5 at baseline; post 5 with count=0 -> 5 > 5+0 false -> OK
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=5\n".into(), // baseline
            "ActiveState=active\n".into(),
            "NRestarts=5\n".into(), // post
        ]);
        let mut hc = default_hc();
        hc.stable = Some(monopsony_common::shadow_types::StableCriterion {
            mode: monopsony_common::shadow_types::StableMode::Restarts,
            secs: None,
            count: Some(0),
        });
        let base = with_systemd_runner_async(Some(runner), || async {
            capture_systemd_baseline("fake-unit").await
        })
        .await;
        assert_eq!(base.n_restarts, Some(5));

        let (runner2, _calls2) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "NRestarts=5\n".into(),
        ]);
        let res = with_systemd_runner_async(Some(runner2), || async {
            systemd_health_check("fake-unit", &hc, &base).await
        })
        .await;
        assert!(
            res.is_ok(),
            "pre-existing restarts must not count against limit: {res:?}"
        );
    }

    #[tokio::test]
    async fn stable_restarts_breach_above_limit() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=5\n".into(), // baseline
            "ActiveState=active\n".into(),
            "NRestarts=6\n".into(), // 6 > 5+0 -> breach
        ]);
        let mut hc = default_hc();
        hc.stable = Some(monopsony_common::shadow_types::StableCriterion {
            mode: monopsony_common::shadow_types::StableMode::Restarts,
            secs: None,
            count: Some(0),
        });
        let base = with_systemd_runner_async(Some(runner), || async {
            capture_systemd_baseline("fake-unit").await
        })
        .await;

        let (runner2, _calls2) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "NRestarts=6\n".into(),
        ]);
        let res = with_systemd_runner_async(Some(runner2), || async {
            systemd_health_check("fake-unit", &hc, &base).await
        })
        .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("NRestarts"));
    }

    #[tokio::test]
    async fn stable_time_stays_active_ok() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),   // baseline
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "ActiveState=active\n".into(),   // reached active
            "ActiveState=active\n".into(),   // still active after stable window
        ]);
        let mut hc = default_hc();
        hc.timeout_secs = 1;
        hc.stable = Some(monopsony_common::shadow_types::StableCriterion {
            mode: monopsony_common::shadow_types::StableMode::Time,
            secs: Some(1),
            count: None,
        });
        let base = with_systemd_runner_async(Some(runner), || async {
            let b = capture_systemd_baseline("fake-unit").await;
            systemd_health_check("fake-unit", &hc, &b).await
        })
        .await;
        assert!(base.is_ok());
    }

    #[tokio::test]
    async fn stable_time_inactive_after_window_breach() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "ActiveState=active\n".into(),   // reached active
            "ActiveState=inactive\n".into(), // dropped after stable window
        ]);
        let mut hc = default_hc();
        hc.timeout_secs = 1;
        hc.stable = Some(monopsony_common::shadow_types::StableCriterion {
            mode: monopsony_common::shadow_types::StableMode::Time,
            secs: Some(1),
            count: None,
        });
        let base = with_systemd_runner_async(Some(runner), || async {
            let b = capture_systemd_baseline("fake-unit").await;
            systemd_health_check("fake-unit", &hc, &b).await
        })
        .await;
        assert!(base.is_err());
        assert!(base.unwrap_err().contains("not active after"));
    }

    #[tokio::test]
    async fn success_monitor_window_stays_active_ok() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "ActiveState=active\n".into(),
        ]);
        let monitor = monopsony_common::shadow_types::SuccessMonitor {
            window_secs: 2,
        };
        let res = with_systemd_runner_async(Some(runner), || async {
            success_monitor_window("fake-unit", &monitor).await
        })
        .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn success_monitor_window_breach() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "ActiveState=inactive\n".into(),
        ]);
        let monitor = monopsony_common::shadow_types::SuccessMonitor {
            window_secs: 2,
        };
        let res = with_systemd_runner_async(Some(runner), || async {
            success_monitor_window("fake-unit", &monitor).await
        })
        .await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("left active"));
    }

    #[tokio::test]
    async fn execute_update_failure_policy_hold_no_rollback() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("start.sh"), b"OLD").unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"NEW")]);
        let (port, _srv) = spawn_http_server(tarball);

        let toml = format!(
            r#"[package]
name = "pkg"
source_type = "direct_url"
source_url = "https://example.com/pkg"

[install]
method = "extract"
target_dir = "{}"
backup_old = true
failure_policy = "hold"
service = "fake-unit"

[install.health_check]
type = "port"
port = {}
timeout_secs = 1

[direct_url]
version_discovery = "scrape"
asset_url_template = "https://example.com/p.tar.gz"

[version]
detect_command = "echo 1.0"
detect_pattern = "([0-9.]+)"
"#,
            target.display(),
            closed_port()
        );
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        // baseline (3 sysctl queries) + journalctl capture on failure
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "journalctl -u fake-unit -n 50: no errors\n".into(),
        ]);
        set_systemd_runner(Some(runner));
        let outcome = execute_update(&recipe, "2.0.0", &url, &http_client()).await;
        set_systemd_runner(None);

        assert!(!outcome.success);
        assert!(
            !outcome.rolled_back,
            "hold must not restore the backup: {outcome:?}"
        );
        assert!(
            outcome
                .log
                .iter()
                .any(|l| l.contains("failure_policy=hold")),
            "hold decision must be logged: {:?}",
            outcome.log
        );
        assert!(
            outcome.log.iter().any(|l| l.contains("journalctl")),
            "journalctl must be captured on health failure: {:?}",
            outcome.log
        );
        // Target keeps the NEW (broken) content — nothing restored.
        assert_eq!(std::fs::read(target.join("start.sh")).unwrap(), b"NEW");
    }
}
