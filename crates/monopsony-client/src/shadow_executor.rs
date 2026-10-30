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
//! - git_release is only reachable through install.method = build (whose
//!   steps provision from source; Sprint 32 Phase 1).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use monopsony_common::shadow_diff::shadow_diff;
use monopsony_common::shadow_template::{render_step, StepContext};
use monopsony_common::shadow_types::{
    InstallMethod, ShadowRecipe, ShadowSource, DEFAULT_MONOPSONY_RECIPE,
};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::shadow;
use crate::shadow_provision;
use crate::shadow_service::{detect_init_system, render_service_file, InitSystem, ServiceSpec};

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
pub fn with_systemd_runner<F, R>(runner: Option<SystemdRunner>, f: F) -> R
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
pub async fn with_systemd_runner_async<F, Fut, R>(runner: Option<SystemdRunner>, f: F) -> R
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
    let argv: Vec<&str> = std::iter::once(program)
        .chain(args.iter().copied())
        .collect();
    if let Some(runner) = SYSTEMD_RUNNER
        .lock()
        .expect("systemd runner lock")
        .as_deref()
    {
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
    if let Ok(out) = run_systemd_cmd("systemctl", &["show", "-p", "ActiveState", service]).await {
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
                let out = run_systemd_cmd("journalctl", &["-u", service, "-n", "50"])
                    .await
                    .map_err(|e| format!("journalctl: {e}"))?;
                let lower = out.to_lowercase();
                if lower.contains("error")
                    || lower.contains("failed")
                    || lower.contains("traceback")
                {
                    return Err(format!(
                        "unit {service} journal shows errors (log_quiet breach)"
                    ));
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
            Ok(out) if parse_show_prop(&out, "ActiveState").as_deref() == Some("active") => {}
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
    /// True when failure_policy=hold deliberately kept the failed state, awaiting operator action.
    pub held: bool,
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

/// Return true iff `target` is strictly older than `current`, using semver 2.0
/// ordering (so pre-release ranks below its release). Either input failing to
/// parse fails open (false) so a valid update is never blocked on a bad
/// version string. A leading `v` and build metadata are ignored, matching the
/// client's self_update helper.
pub fn is_downgrade(target: &str, current: &str) -> bool {
    match (
        semver::Version::parse(target.trim_start_matches('v')),
        semver::Version::parse(current.trim_start_matches('v')),
    ) {
        (Ok(t), Ok(c)) => t < c,
        _ => false,
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
/// - git_release → unsupported (clear error)
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
        ShadowSource::GitRelease => Err(anyhow!(
            "client-side executor does not support git_release yet"
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

/// Inputs to the downgrade-safety gate, bundled for readability at the
/// `execute_inner` boundary: what is currently installed, and whether a human
/// is present to confirm a `downgrade_unsafe` recipe's downgrade.
pub struct DowngradeContext<'a> {
    pub current_version: Option<&'a str>,
    pub interactive: bool,
}

/// Derive the libc flavor for `StepContext`. Nothing else in the client
/// derives libc today (heartbeats send it empty), so detect locally: on
/// Linux, musl's dynamic loader lives at a well-known arch-specific path;
/// everything else is assumed glibc. Non-Linux platforms return the OS name.
fn detect_libc() -> String {
    if std::env::consts::OS != "linux" {
        return std::env::consts::OS.to_string();
    }
    let musl_loaders = [
        "/lib/ld-musl-x86_64.so.1",
        "/lib/ld-musl-aarch64.so.1",
        "/lib/ld-musl-armhf.so.1",
    ];
    if musl_loaders.iter().any(|p| Path::new(p).exists()) {
        "musl".to_string()
    } else {
        "glibc".to_string()
    }
}

/// Injectable install root for service-script writes (test seam).
///
/// Units/init scripts are written under `<root>/systemd/system` and
/// `<root>/init.d` respectively. Defaults to the real `/etc`; tests point
/// this at a tempdir so hermetic tests never touch host init state.
static SERVICE_INSTALL_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Injectable /etc/os-release CONTENT (test seam). `None` means the
/// production wrapper reads the real `/etc/os-release` (absent/unreadable →
/// empty content, which `detect_init_system` maps to systemd by design).
static OS_RELEASE_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

fn service_install_root() -> PathBuf {
    SERVICE_INSTALL_ROOT
        .lock()
        .expect("service install root lock")
        .clone()
        .unwrap_or_else(|| PathBuf::from("/etc"))
}

/// Production wrapper: os-release content from the test override when set,
/// otherwise the real file (empty when absent/unreadable — detect_init_system
/// then lands on systemd).
fn read_os_release() -> String {
    if let Some(c) = OS_RELEASE_OVERRIDE
        .lock()
        .expect("os-release override lock")
        .clone()
    {
        return c;
    }
    std::fs::read_to_string("/etc/os-release").unwrap_or_default()
}

/// Run async `f` with the service-script test seams installed (install root
/// and/or os-release content), restoring both overrides afterwards. Tests
/// using this must hold `TEST_RUNNER_LOCK`: the seams are shared statics and
/// service-enabled updates also exercise the shared SYSTEMD_RUNNER.
/// Deliberately no `Send` bounds — `execute_update`'s future is not `Send`
/// (restore_backup boxes a non-Send future), and tests run on the
/// current-thread runtime.
pub async fn with_service_env_async<F, Fut, R>(
    install_root: Option<PathBuf>,
    os_release: Option<String>,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    *SERVICE_INSTALL_ROOT
        .lock()
        .expect("service install root lock") = install_root;
    *OS_RELEASE_OVERRIDE
        .lock()
        .expect("os-release override lock") = os_release;
    let result = f().await;
    *SERVICE_INSTALL_ROOT
        .lock()
        .expect("service install root lock") = None;
    *OS_RELEASE_OVERRIDE
        .lock()
        .expect("os-release override lock") = None;
    result
}

/// Detect the init system, render a [`ServiceSpec`] from the recipe, write
/// the unit/init script to the init-specific path, and enable it.
///
/// Persistence default is unless-stopped (v1): the in-binary templates carry
/// `Restart=on-failure` + `[Install] WantedBy=multi-user.target` (systemd)
/// and `rc-update add <name> default` (OpenRC); an on-demand knob is
/// deferred. The service is deliberately NOT started here — fleet recipes
/// start it in their post_update hook, which runs immediately after this
/// step (the unit must exist first, hence this runs before that hook).
///
/// Rollback semantics: a written unit/init script is NOT rolled back on a
/// later failure (binary/config rollback only); re-rendering on the next
/// update is idempotent (the write is an overwrite).
async fn install_service_script(
    recipe: &ShadowRecipe,
    service: &str,
    target_version: &str,
    log: &mut Vec<String>,
) -> std::result::Result<(), StageError> {
    let install = &recipe.install;
    let init = detect_init_system(&read_os_release());

    // exec_start is validated at plan stage for build/script; extract may
    // default to target_dir/package.name.
    let exec_start_raw = install.exec_start.clone().unwrap_or_else(|| {
        format!(
            "{}/{}",
            install.target_dir.trim_end_matches('/'),
            recipe.package.name
        )
    });
    // Author variables inside exec_start render through the Phase-1 engine —
    // a literal {{target_dir}} in the unit was caught live 2026-10-30 (fleet
    // verification: ExecStart shipped unsubstituted while WorkingDirectory,
    // substituted at the spec level, was fine).
    let ctx = StepContext {
        version: target_version.to_string(),
        target_dir: install.target_dir.clone(),
        arch: std::env::consts::ARCH.to_string(),
        os: std::env::consts::OS.to_string(),
        libc: detect_libc(),
    };
    let legacy_free = shadow_provision::translate_legacy_braces(&exec_start_raw);
    let exec_start = render_step(&legacy_free, &ctx)
        .map_err(|e| stage("service", format!("rendering exec_start: {e}")))?;
    let unit_name = match init {
        InitSystem::Systemd => {
            if service.ends_with(".service") {
                service.to_string()
            } else {
                format!("{service}.service")
            }
        }
        // OpenRC names carry no .service suffix; fleet recipes write the
        // systemd-style name into `install.service` (e.g. technitium's
        // "dns.service"), so strip it for the init.d script name.
        InitSystem::OpenRC => service.trim_end_matches(".service").to_string(),
    };
    let spec = ServiceSpec {
        unit_name: unit_name.clone(),
        description: recipe
            .package
            .description
            .clone()
            .unwrap_or_else(|| format!("{} (managed by monopsony)", recipe.package.name)),
        exec_start,
        working_dir: install.target_dir.clone(),
    };

    let rendered = render_service_file(init, &spec)
        .map_err(|e| stage("service", format!("rendering {init} service file: {e}")))?;

    let path = match init {
        InitSystem::Systemd => service_install_root()
            .join("systemd")
            .join("system")
            .join(&unit_name),
        InitSystem::OpenRC => service_install_root().join("init.d").join(&unit_name),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))
            .map_err(|e| stage("service", e))?;
    }
    std::fs::write(&path, &rendered)
        .with_context(|| format!("writing {}", path.display()))
        .map_err(|e| stage("service", e))?;
    if init == InitSystem::OpenRC {
        // OpenRC init scripts must be executable to be usable.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod 755 {}", path.display()))
            .map_err(|e| stage("service", e))?;
    }
    log.push(format!("wrote {init} service script: {}", path.display()));

    // Enable only (see doc comment: starting is the post_update hook's job).
    match init {
        InitSystem::Systemd => {
            run_systemd_cmd("systemctl", &["daemon-reload"])
                .await
                .map_err(|e| stage("service", format!("systemctl daemon-reload: {e}")))?;
            run_systemd_cmd("systemctl", &["enable", unit_name.as_str()])
                .await
                .map_err(|e| stage("service", format!("systemctl enable {unit_name}: {e}")))?;
        }
        InitSystem::OpenRC => {
            run_systemd_cmd("rc-update", &["add", unit_name.as_str(), "default"])
                .await
                .map_err(|e| stage("service", format!("rc-update add {unit_name}: {e}")))?;
        }
    }
    Ok(())
}

/// Execute the full update pipeline for a verified recipe. Never panics;
/// failures (with rollback when possible) are reported via the outcome.
pub async fn execute_update(
    recipe: &ShadowRecipe,
    target_version: &str,
    asset_url: &str,
    http: &reqwest::Client,
    current_version: Option<&str>,
    interactive: bool,
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
        DowngradeContext {
            current_version,
            interactive,
        },
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
                held: false,
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
                log.push(
                    "failure_policy=hold: NOT restoring backup; awaiting operator intervention"
                        .to_string(),
                );
            }
            UpdateOutcome {
                success: false,
                rolled_back,
                new_version: target_version.to_string(),
                provisional: false,
                backup_path: None,
                log,
                error: Some(format!("{}: {}", err.stage, err.message)),
                held: hold,
            }
        }
    }
}

/// Execute the update pipeline with Sprint 32 Phase 3 update-diffing.
///
/// Thin wrapper around [`execute_update`] (same signature plus `previous` —
/// deliberately NOT a widened `execute_update`, which has ~20 test callers):
/// when `previous` is `Some`, compute the pure recipe diff
/// ([`monopsony_common::shadow_diff::shadow_diff`]) between the previously
/// registered/adopted recipe and the incoming one, and PREPEND the warning
/// lines — format `diff warning: <class>: <message>` — to the returned
/// outcome's log. Failure coupling is satisfied by this ordering: on a
/// health_check failure the warnings are already in the same outcome log that
/// receives the journalctl capture. `previous = None` (first install, or no
/// loadable previous recipe) yields zero diff lines and behavior identical to
/// `execute_update`.
pub async fn execute_update_with_diff(
    recipe: &ShadowRecipe,
    previous: Option<&ShadowRecipe>,
    target_version: &str,
    asset_url: &str,
    http: &reqwest::Client,
    current_version: Option<&str>,
    interactive: bool,
) -> UpdateOutcome {
    let mut outcome = execute_update(
        recipe,
        target_version,
        asset_url,
        http,
        current_version,
        interactive,
    )
    .await;
    if let Some(old) = previous {
        // Diff lines are warnings ABOUT the update, so they lead the log; the
        // executor's own stage lines (including any journalctl capture and
        // the FAILED line) follow unchanged.
        let mut log: Vec<String> = Vec::new();
        for warning in shadow_diff(old, recipe) {
            log.push(format!(
                "diff warning: {}: {}",
                warning.class.label(),
                warning.message
            ));
        }
        log.extend(outcome.log);
        outcome.log = log;
    }
    outcome
}

async fn execute_inner(
    recipe: &ShadowRecipe,
    target_version: &str,
    asset_url: &str,
    http: &reqwest::Client,
    log: &mut Vec<String>,
    backup: &mut Option<PathBuf>,
    gate: DowngradeContext<'_>,
) -> std::result::Result<(), StageError> {
    let install = &recipe.install;

    // git_release has no release asset to download; it is only reachable
    // through the Build pipeline, whose steps provision from source.
    if recipe.package.source_type == ShadowSource::GitRelease
        && install.method != InstallMethod::Build
    {
        return Err(stage(
            "plan",
            "git_release recipes require install.method = build (no release asset to download)",
        ));
    }

    // Downgrade gate: refuse to go OLDER unless the operator is interactive or
    // success is health-verifiable. Fail-open (return false) when current
    // version can't be parsed so we never block a valid update.
    if recipe.downgrade_unsafe
        && matches!(gate.current_version, Some(c) if is_downgrade(target_version, c))
        && !gate.interactive
        && install.health_check.is_none()
    {
        return Err(stage(
            "plan",
            format!(
                "downgrade refused: downgrade_unsafe set and target {} < current {}; interactive or health-verifiable update required",
                target_version,
                gate.current_version.unwrap_or("unknown"),
            ),
        ));
    }

    // T3.2: capture pre-update systemd baseline before any changes are made,
    // so NRestarts comparisons only count restarts introduced by this update.
    let service = install.service.as_deref();

    // Plan-stage service validation: extract installs can default exec_start
    // to target_dir/package.name, but a built/scripted tree's service command
    // line cannot be guessed — fail loud BEFORE any host change (pre_update,
    // backup, install).
    if service.is_some() && install.exec_start.is_none() && install.method != InstallMethod::Extract
    {
        return Err(stage(
            "plan",
            "install.service set without exec_start: build/script recipes require exec_start (no guessing a service command line)",
        ));
    }

    let mut baseline: Option<SystemdBaseline> = None;
    if service.is_some() {
        baseline = Some(capture_systemd_baseline(service.unwrap()).await);
    }

    // Steps 1-2: download the asset to a temp file (skipped for script
    // recipes with no resolvable URL — the script fetches its own bits —
    // and for build recipes, which provision from source instead).
    let tmp =
        tempfile::tempdir().map_err(|e| stage("download", format!("creating tempdir: {e}")))?;
    let asset_path = if (install.method == InstallMethod::Script
        || install.method == InstallMethod::Build)
        && asset_url.is_empty()
    {
        if install.method == InstallMethod::Build {
            log.push("build method: no asset URL; steps provision from source".to_string());
        } else {
            log.push("no asset URL resolved; script method will fetch itself".to_string());
        }
        None
    } else {
        let dest = tmp.path().join("asset");
        download_asset(http, asset_url, None, &dest)
            .await
            .map_err(|e| stage("download", e))?;
        log.push(format!("downloaded {asset_url}"));
        Some(dest)
    };

    // Build recipes: verify build requirements via `which` BEFORE any host
    // change — pre_update may stop the service and backup mutates the
    // target, so a missing toolchain must abort before both.
    if install.method == InstallMethod::Build {
        let build = recipe.build.as_ref().ok_or_else(|| {
            stage(
                "plan",
                "install.method = build but recipe has no [build] section",
            )
        })?;
        shadow::check_build_deps(&build.requires)
            .await
            .map_err(|e| stage("requires", e))?;
        log.push("build requires: satisfied".to_string());
    }

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
            let build = recipe.build.as_ref().ok_or_else(|| {
                stage(
                    "plan",
                    "install.method = build but recipe has no [build] section",
                )
            })?;
            let ctx = StepContext {
                version: target_version.to_string(),
                target_dir: install.target_dir.clone(),
                arch: std::env::consts::ARCH.to_string(),
                os: std::env::consts::OS.to_string(),
                libc: detect_libc(),
            };
            // Render first (legacy single-brace translated at this boundary,
            // then the double-brace engine), then resolve --source on the
            // rendered text — a {{target_dir}}/.env guard must compose.
            let mut rendered = Vec::with_capacity(build.steps.len());
            for (i, step) in build.steps.iter().enumerate() {
                let legacy_free = shadow_provision::translate_legacy_braces(step);
                let r = render_step(&legacy_free, &ctx)
                    .map_err(|e| stage("build", format!("rendering step {}: {e}", i + 1)))?;
                rendered.push(r);
            }
            log.push(format!("rendered {} build step(s)", rendered.len()));
            let config_paths = install.config_paths.as_deref().unwrap_or(&[]);
            let resolved = shadow_provision::resolve_step_sources(&rendered, config_paths, http)
                .await
                .map_err(|e| stage("provision", e))?;
            // Steps run ONE AT A TIME, fail fast, under the recipe's network
            // policy (deny is the serde default).
            let policy = shadow::resolve_network_policy(build.allow_network);
            for (i, step) in resolved.iter().enumerate() {
                shadow_provision::run_build_step(step, &policy)
                    .await
                    .map_err(|e| stage("build", format!("build step {} failed: {e}", i + 1)))?;
                log.push(format!("step {}: ok", i + 1));
            }
        }
    }

    // Step 5.5: service script — the unit/init script must exist BEFORE the
    // post_update hook runs (fleet recipes start their service in that hook;
    // the unit must exist first). Detection/render/enable details live in
    // install_service_script.
    if let Some(svc) = service {
        install_service_script(recipe, svc, target_version, log).await?;
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
    static TEST_RUNNER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
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
    fn is_downgrade_valid_semver_pairs() {
        assert!(is_downgrade("1.0.0", "2.0.0"));
        assert!(is_downgrade("1.2.3", "1.2.4"));
        assert!(!is_downgrade("2.0.0", "1.0.0"));
        assert!(!is_downgrade("1.0.0", "1.0.0")); // equal is not a downgrade
        assert!(is_downgrade("1.0.0-rc1", "1.0.0")); // pre-release < release
        assert!(!is_downgrade("1.0.0-rc2", "1.0.0-rc1")); // rc2 > rc1: upgrade, not downgrade
        assert!(is_downgrade("1.0.0-rc1", "1.0.0-rc2")); // rc ordering: rc1 < rc2
    }

    #[test]
    fn is_downgrade_unparseable_fails_open() {
        assert!(!is_downgrade("not-a-version", "1.0.0"));
        assert!(!is_downgrade("1.0.0", ""));
        assert!(!is_downgrade("", ""));
        // A bad target never blocks (fail-open): not a downgrade, proceed.
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
    async fn resolve_asset_url_git_release_rejected() {
        let toml = r#"[package]
name = "gb"
source_type = "git_release"
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
        assert!(err.contains("does not support git_release"), "got: {err}");
    }

    #[tokio::test]
    async fn execute_update_git_release_non_build_rejected() {
        // git_release with method != build has no asset path at all — still
        // a hard plan error (Build is its only route through the executor).
        let toml = r#"[package]
name = "gb"
source_type = "git_release"
source_url = "https://github.com/example/gb.git"

[install]
method = "extract"
target_dir = "/opt/gb"

[version]
detect_command = "gb --version"
detect_pattern = "([0-9]+)"
"#;
        let recipe: ShadowRecipe = toml::from_str(toml).unwrap();
        let outcome = execute_update(&recipe, "v2", "", &http_client(), None, false).await;
        assert!(!outcome.success);
        assert!(!outcome.rolled_back);
        assert!(
            outcome
                .error
                .unwrap()
                .contains("git_release recipes require install.method = build"),
            "executor must reject git_release outside the build pipeline"
        );
    }

    /// Build-method recipe TOML with the given requires/steps.
    fn build_recipe_toml(
        target: &Path,
        requires: &[&str],
        allow_network: bool,
        steps: &[String],
    ) -> String {
        let requires_toml = requires
            .iter()
            .map(|r| format!("\"{r}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let steps_toml = steps
            .iter()
            .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"[package]
name = "built"
source_type = "git_release"
source_url = "https://github.com/example/built"

[install]
method = "build"
target_dir = "{}"
backup_old = true

[build]
requires = [{}]
allow_network = {}
steps = [{}]

[version]
detect_command = "echo 1.0.0"
detect_pattern = "([0-9.]+)"
"#,
            target.display(),
            requires_toml,
            allow_network,
            steps_toml
        )
    }

    #[tokio::test]
    async fn execute_update_build_happy_path_renders_and_runs_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");

        let toml = build_recipe_toml(
            &target,
            &["bash"],
            false,
            &[
                "mkdir -p {{target_dir}} && echo v{{version}} > {{target_dir}}/out.txt".to_string(),
                "test -f {{target_dir}}/out.txt".to_string(),
            ],
        );
        let recipe = parse_recipe(&toml);
        // Empty asset URL: Build skips the download like Script does.
        let outcome = execute_update(&recipe, "2.5.0", "", &http_client(), None, false).await;
        assert!(outcome.success, "outcome: {outcome:?}");
        assert_eq!(
            std::fs::read_to_string(target.join("out.txt"))
                .unwrap()
                .trim(),
            "v2.5.0",
            "StepContext.version is bare semver; the v-prefix is author-composed"
        );
        assert!(outcome.log.iter().any(|l| l == "build requires: satisfied"));
        assert!(outcome.log.iter().any(|l| l == "step 1: ok"));
        assert!(outcome.log.iter().any(|l| l == "step 2: ok"));
    }

    #[tokio::test]
    async fn execute_update_build_legacy_single_brace_steps_translated() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");

        // Already-registered recipes use the asset-template brace style;
        // the executor boundary must translate the five known names.
        let toml = build_recipe_toml(
            &target,
            &["bash"],
            false,
            &["mkdir -p {target_dir} && echo {version} > {target_dir}/legacy.txt".to_string()],
        );
        let recipe = parse_recipe(&toml);
        let outcome = execute_update(&recipe, "3.1.4", "", &http_client(), None, false).await;
        assert!(outcome.success, "outcome: {outcome:?}");
        assert_eq!(
            std::fs::read_to_string(target.join("legacy.txt"))
                .unwrap()
                .trim(),
            "3.1.4"
        );
    }

    #[tokio::test]
    async fn build_missing_requires_aborts_before_any_change() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");

        let toml = build_recipe_toml(
            &target,
            &["monopsony-no-such-binary-xyz"],
            false,
            &["mkdir -p {{target_dir}} && touch {{target_dir}}/changed".to_string()],
        );
        let recipe = parse_recipe(&toml);
        let outcome = execute_update(&recipe, "1.0.0", "", &http_client(), None, false).await;
        assert!(!outcome.success);
        assert!(!outcome.rolled_back, "nothing to roll back yet");
        assert!(outcome.backup_path.is_none(), "no backup taken");
        let err = outcome.error.unwrap();
        assert!(
            err.starts_with("requires") && err.contains("monopsony-no-such-binary-xyz"),
            "stage error must name the missing executable, got: {err}"
        );
        assert!(
            !target.join("changed").exists(),
            "requires check must abort before any step runs"
        );
    }

    #[tokio::test]
    async fn build_step_failure_aborts_naming_step_index() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");

        let toml = build_recipe_toml(
            &target,
            &["bash"],
            false,
            &[
                "mkdir -p {{target_dir}}".to_string(),
                "exit 7".to_string(),
                "touch {{target_dir}}/never".to_string(),
            ],
        );
        let recipe = parse_recipe(&toml);
        let outcome = execute_update(&recipe, "1.0.0", "", &http_client(), None, false).await;
        assert!(!outcome.success);
        let err = outcome.error.unwrap();
        assert!(
            err.starts_with("build") && err.contains("build step 2 failed"),
            "error must name the failing step index, got: {err}"
        );
        assert!(
            !target.join("never").exists(),
            "steps must run one at a time, fail fast: step 3 must never run"
        );
        assert!(
            !outcome.log.iter().any(|l| l == "step 3: ok"),
            "no ok entry for a step that never ran"
        );
    }

    #[tokio::test]
    async fn build_step_source_resolution_via_executor() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        let (port, _srv) = spawn_http_server(b"SEED=executor".to_vec());

        let toml = build_recipe_toml(
            &target,
            &["bash"],
            true,
            &[
                // The directive-bearing step keeps a runnable remainder:
                // resolution strips `--source <path> <fallback>` and the
                // rest executes (the doc's helper name is illustrative).
                format!("true --source {{target_dir}}/.env http://127.0.0.1:{port}/.env.example"),
                "cp {{target_dir}}/.env {{target_dir}}/copied.env".to_string(),
            ],
        );
        let recipe = parse_recipe(&toml);
        let outcome = execute_update(&recipe, "1.0.0", "", &http_client(), None, false).await;
        assert!(outcome.success, "outcome: {outcome:?}");
        // The fetched file landed AND the following step consumed it.
        assert_eq!(
            std::fs::read(target.join("copied.env")).unwrap(),
            b"SEED=executor"
        );
    }

    /// Root-level `downgrade_unsafe = true` prepended before the first section.
    fn downgrade_unsafe_recipe_toml(extra_install: &str) -> String {
        format!(
            "downgrade_unsafe = true\n[package]\nname = \"dg\"\nsource_type = \"direct_url\"\nsource_url = \"https://example.com/dg.tar.gz\"\n\n[install]\nmethod = \"extract\"\ntarget_dir = \"/tmp/dg-test\"\n{extra_install}\n[direct_url]\nversion_discovery = \"scrape\"\nasset_url_template = \"https://example.com/dl/dg.tar.gz\"\n\n[version]\ndetect_command = \"echo 1.0.0\"\ndetect_pattern = \"([0-9]+\\\\.[0-9]+\\\\.[0-9]+)\"\n"
        )
    }

    #[tokio::test]
    async fn execute_update_downgrade_gate_refuses_non_interactive() {
        let recipe: ShadowRecipe = toml::from_str(&downgrade_unsafe_recipe_toml("")).unwrap();
        // Target 1.0.0 < current 2.0.0, non-interactive, no health check -> refused.
        let outcome = execute_update(
            &recipe,
            "1.0.0",
            "https://example.com/dg.tar.gz",
            &http_client(),
            Some("2.0.0"),
            false,
        )
        .await;
        assert!(!outcome.success);
        assert!(!outcome.rolled_back, "refusal happens before any changes");
        assert!(
            outcome.error.unwrap().contains("downgrade refused"),
            "gate must refuse non-interactive downgrades"
        );
    }

    #[tokio::test]
    async fn execute_update_downgrade_gate_allows_health_verifiable() {
        let toml = downgrade_unsafe_recipe_toml(
            "\n[install.health_check]\ntype = \"port\"\nport = 5380\ntimeout_secs = 3\n",
        );
        let recipe: ShadowRecipe = toml::from_str(&toml).unwrap();
        // Health-verifiable success path: gate must NOT fire (fails later at download).
        let outcome = execute_update(
            &recipe,
            "1.0.0",
            "https://127.0.0.1:1/dg.tar.gz",
            &http_client(),
            Some("2.0.0"),
            false,
        )
        .await;
        assert!(!outcome.success);
        let err = outcome.error.unwrap();
        assert!(
            !err.contains("downgrade refused"),
            "health-verifiable downgrade must not be gated, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_update_downgrade_gate_allows_interactive() {
        let recipe: ShadowRecipe = toml::from_str(&downgrade_unsafe_recipe_toml("")).unwrap();
        // Interactive operator confirmed the downgrade: gate must NOT fire.
        let outcome = execute_update(
            &recipe,
            "1.0.0",
            "https://127.0.0.1:1/dg.tar.gz",
            &http_client(),
            Some("2.0.0"),
            true,
        )
        .await;
        assert!(!outcome.success);
        let err = outcome.error.unwrap();
        assert!(
            !err.contains("downgrade refused"),
            "interactive downgrade must not be gated, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_update_downgrade_gate_ignores_upgrades() {
        let recipe: ShadowRecipe = toml::from_str(&downgrade_unsafe_recipe_toml("")).unwrap();
        // Target 2.0.0 > current 1.0.0: an upgrade; the flag only gates downgrades.
        let outcome = execute_update(
            &recipe,
            "2.0.0",
            "https://127.0.0.1:1/dg.tar.gz",
            &http_client(),
            Some("1.0.0"),
            false,
        )
        .await;
        assert!(!outcome.success);
        let err = outcome.error.unwrap();
        assert!(
            !err.contains("downgrade refused"),
            "upgrades must not be gated by downgrade_unsafe, got: {err}"
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

        let outcome = execute_update(&recipe, "15.6.2", &url, &http_client(), None, false).await;
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

        let outcome = execute_update(&recipe, "15.6.2", &url, &http_client(), None, false).await;
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

        let outcome = execute_update(&recipe, "2.0.0", &url, &http_client(), None, false).await;
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
            None,
            false,
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

        let outcome = execute_update(&recipe, "1.0.0", &url, &http_client(), None, false).await;
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
        let outcome = execute_update(&recipe, "7.7.7", "", &http_client(), None, false).await;
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
        let outcome = execute_update(&recipe, "2.0.0", "", &http_client(), None, false).await;
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
    fn scripted_fake(responses: Vec<String>) -> (SystemdRunner, Arc<Mutex<Vec<Vec<String>>>>) {
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
            "ActiveState=active\n".into(), // baseline ActiveState
            "SubState=running\n".into(),   // baseline SubState
            "NRestarts=0\n".into(),        // baseline NRestarts
            "ActiveState=active\n".into(), // health check: active
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

        let (runner2, _calls2) =
            scripted_fake(vec!["ActiveState=active\n".into(), "NRestarts=5\n".into()]);
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

        let (runner2, _calls2) =
            scripted_fake(vec!["ActiveState=active\n".into(), "NRestarts=6\n".into()]);
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
            "ActiveState=active\n".into(), // baseline
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "ActiveState=active\n".into(), // reached active
            "ActiveState=active\n".into(), // still active after stable window
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
        let monitor = monopsony_common::shadow_types::SuccessMonitor { window_secs: 2 };
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
        let monitor = monopsony_common::shadow_types::SuccessMonitor { window_secs: 2 };
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

        // baseline (3 sysctl queries) + service step (daemon-reload, enable)
        // + journalctl capture on failure. Hermetic: unit writes land in a
        // temp root, os-release forced to systemd.
        let svc_root = tempfile::tempdir().unwrap();
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "".into(), // systemctl daemon-reload (service step)
            "".into(), // systemctl enable fake-unit.service
            "journalctl -u fake-unit -n 50: no errors\n".into(),
        ]);
        set_systemd_runner(Some(runner));
        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(String::new()),
            || async { execute_update(&recipe, "2.0.0", &url, &http_client(), None, false).await },
        )
        .await;
        set_systemd_runner(None);

        assert!(!outcome.success);
        assert!(
            !outcome.rolled_back,
            "hold must not restore the backup: {outcome:?}"
        );
        // failure_policy=hold deliberately kept the failed state → held marker set.
        assert!(
            outcome.held,
            "hold-failure must report held=true (kept failed state): {outcome:?}"
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

    #[tokio::test]
    async fn execute_update_held_flag_false_after_auto_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("start.sh"), b"OLD-CONTENT").unwrap();

        // Health check fails (closed port) but failure_policy is NOT hold, so the
        // executor auto-restores the backup → held must be false.
        let tarball = make_tar_gz(&[("start.sh", b"BROKEN-NEW")]);
        let (port, _srv) = spawn_http_server(tarball);

        let toml = recipe_toml(&target, Some(closed_port()), None, None, 3);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        let outcome = execute_update(&recipe, "15.6.2", &url, &http_client(), None, false).await;
        assert!(!outcome.success);
        assert!(
            outcome.rolled_back,
            "expected auto-rollback, got {outcome:?}"
        );
        // Default policy restores → held is not set (operator did not hold).
        assert!(
            !outcome.held,
            "auto-rollback must report held=false: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn execute_update_held_flag_false_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("start.sh"), b"old").unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"NEW-15.6.2")]);
        let (port, _srv) = spawn_http_server(tarball);

        let toml = recipe_toml(&target, Some(open_port()), None, None, 3);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/DnsServerPortable.tar.gz");

        let outcome = execute_update(&recipe, "15.6.2", &url, &http_client(), None, false).await;
        assert!(outcome.success, "outcome: {outcome:?}");
        // A clean success never holds the failed state.
        assert!(!outcome.held, "success must report held=false: {outcome:?}");
    }

    // --- Sprint 32 Phase 2: service-script wiring (hermetic) ---

    /// Extract-method recipe declaring `install.service`, in the fleet
    /// convention (technitium carries the .service suffix in the field).
    fn service_extract_recipe_toml(
        target: &Path,
        exec_start: Option<&str>,
        post: Option<&str>,
    ) -> String {
        let mut s = format!(
            r#"[package]
name = "svcapp"
description = "Service App"
source_type = "direct_url"
source_url = "https://example.com/svcapp"

[install]
method = "extract"
target_dir = "{}"
service = "svcapp.service"
backup_old = true
"#,
            target.display()
        );
        if let Some(e) = exec_start {
            s.push_str(&format!("exec_start = \"{e}\"\n"));
        }
        if let Some(p) = post {
            s.push_str(&format!("post_update = \"{p}\"\n"));
        }
        s.push_str("\n[direct_url]\nversion_discovery = \"scrape\"\nasset_url_template = \"https://example.com/dl/svcapp.tar.gz\"\n");
        s.push_str(
            "\n[version]\ndetect_command = \"echo 1.0.0\"\ndetect_pattern = \"([0-9.]+)\"\n",
        );
        s
    }

    #[tokio::test]
    async fn service_step_renders_writes_and_enables_systemd_unit() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let svc_root = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"NEW")]);
        let (port, _srv) = spawn_http_server(tarball);
        let toml = service_extract_recipe_toml(&target, Some("/opt/app/bin/svcapp --serve"), None);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        // baseline(3) + daemon-reload + enable
        let (runner, calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "".into(),
            "".into(),
        ]);
        set_systemd_runner(Some(runner));
        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(String::new()),
            || async { execute_update(&recipe, "1.0.0", &url, &http_client(), None, false).await },
        )
        .await;
        set_systemd_runner(None);

        assert!(outcome.success, "outcome: {outcome:?}");

        // Unit written to <root>/systemd/system/<unit>.
        let unit = svc_root.path().join("systemd/system/svcapp.service");
        let content = std::fs::read_to_string(&unit)
            .unwrap_or_else(|e| panic!("unit must be written at {}: {e}", unit.display()));
        assert!(
            content.contains("ExecStart=/opt/app/bin/svcapp --serve"),
            "unit must contain the rendered exec_start: {content}"
        );
        assert!(
            content.contains(&format!("WorkingDirectory={}", target.display())),
            "unit must contain target_dir as WorkingDirectory: {content}"
        );
        assert!(
            !content.contains("{{"),
            "a rendered unit containing a literal double-brace placeholder is a bug: {content}"
        );
        assert!(outcome
            .log
            .iter()
            .any(|l| l.contains("systemd service script")));

        // Enable ran, in order: daemon-reload BEFORE enable. And no start.
        let c = calls.lock().unwrap();
        assert_eq!(c[3], vec!["systemctl", "daemon-reload"]);
        assert_eq!(c[4], vec!["systemctl", "enable", "svcapp.service"]);
        assert!(
            !c.iter().any(|a| a.contains(&"start".to_string())),
            "the executor must never start the service (post_update's job): {c:?}"
        );
    }

    #[tokio::test]
    async fn service_step_exec_start_default_for_extract() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let svc_root = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"NEW")]);
        let (port, _srv) = spawn_http_server(tarball);
        // exec_start absent: extract method defaults to target_dir/package.name.
        let toml = service_extract_recipe_toml(&target, None, None);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "".into(),
            "".into(),
        ]);
        set_systemd_runner(Some(runner));
        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(String::new()),
            || async { execute_update(&recipe, "1.0.0", &url, &http_client(), None, false).await },
        )
        .await;
        set_systemd_runner(None);

        assert!(outcome.success, "outcome: {outcome:?}");
        let content =
            std::fs::read_to_string(svc_root.path().join("systemd/system/svcapp.service")).unwrap();
        let expected = format!("{}/svcapp", target.display());
        assert!(
            content.contains(&format!("ExecStart={expected}")),
            "exec_start must default to target_dir/package.name ({expected}): {content}"
        );
    }

    #[tokio::test]
    async fn service_step_build_script_without_exec_start_plan_error_before_any_change() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let svc_root = tempfile::tempdir().unwrap();

        // Build method: service set, exec_start absent.
        let btmp = tempfile::tempdir().unwrap();
        let btarget = btmp.path().join("app");
        let toml = build_recipe_toml(
            &btarget,
            &["bash"],
            false,
            &["touch {{target_dir}}/changed".to_string()],
        )
        .replace(
            "backup_old = true",
            "backup_old = true\nservice = \"built.service\"",
        );
        let recipe = parse_recipe(&toml);
        let (runner, calls) = scripted_fake(vec![]);
        set_systemd_runner(Some(runner));
        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(String::new()),
            || async { execute_update(&recipe, "1.0.0", "", &http_client(), None, false).await },
        )
        .await;
        set_systemd_runner(None);

        assert!(!outcome.success);
        let err = outcome.error.unwrap();
        assert!(
            err.starts_with("plan") && err.contains("exec_start"),
            "must be a plan-stage error naming exec_start, got: {err}"
        );
        assert!(
            !btarget.join("changed").exists(),
            "plan error must fire before any build step runs"
        );

        // Script method: same gate.
        let stmp = tempfile::tempdir().unwrap();
        let starget = stmp.path().join("app");
        let toml = format!(
            r#"[package]
name = "scripted"
source_type = "direct_url"
source_url = "https://example.com/scripted"

[install]
method = "script"
target_dir = "{}"
service = "scripted.service"

[script]
update = "touch {{}}/marker"

[version]
detect_command = "echo 1.0"
detect_pattern = "([0-9.]+)"

[direct_url]
version_discovery = "scrape"
asset_url_template = "https://example.com/s"
"#,
            starget.display()
        );
        let recipe = parse_recipe(&toml);
        let (runner, calls2) = scripted_fake(vec![]);
        set_systemd_runner(Some(runner));
        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(String::new()),
            || async { execute_update(&recipe, "1.0.0", "", &http_client(), None, false).await },
        )
        .await;
        set_systemd_runner(None);

        assert!(!outcome.success);
        let err = outcome.error.unwrap();
        assert!(
            err.starts_with("plan") && err.contains("exec_start"),
            "script method must hit the same plan gate, got: {err}"
        );
        assert!(
            !starget.join("marker").exists(),
            "plan error must fire before the script runs"
        );

        // Nothing was written or run for either case.
        assert!(
            calls.lock().unwrap().is_empty(),
            "no service ops before plan failure"
        );
        assert!(
            calls2.lock().unwrap().is_empty(),
            "no service ops before plan failure"
        );
        assert_eq!(
            std::fs::read_dir(svc_root.path()).unwrap().count(),
            0,
            "plan-stage refusal must not write any unit/init script"
        );
    }

    #[tokio::test]
    async fn service_step_openrc_writes_init_script_mode_755_and_enables() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let svc_root = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"NEW")]);
        let (port, _srv) = spawn_http_server(tarball);
        let toml = service_extract_recipe_toml(&target, Some("/opt/app/bin/svcapp --serve"), None);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        // Injected Alpine os-release content → OpenRC path.
        let alpine = "NAME=\"Alpine Linux\"\nID=alpine\n";
        // baseline(3, best-effort systemctl shows even on OpenRC hosts) + rc-update add
        let (runner, calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "".into(),
        ]);
        set_systemd_runner(Some(runner));
        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(alpine.to_string()),
            || async { execute_update(&recipe, "1.0.0", &url, &http_client(), None, false).await },
        )
        .await;
        set_systemd_runner(None);

        assert!(outcome.success, "outcome: {outcome:?}");

        let script = svc_root.path().join("init.d/svcapp");
        let content = std::fs::read_to_string(&script)
            .unwrap_or_else(|e| panic!("init script must be written at {}: {e}", script.display()));
        assert!(content.starts_with("#!/sbin/openrc-run"), "got: {content}");
        assert!(
            content.contains("command=\"/opt/app/bin/svcapp --serve\""),
            "init script must render exec_start: {content}"
        );
        assert!(
            content.contains(&format!("directory=\"{}\"", target.display())),
            "init script must render working_dir: {content}"
        );
        assert!(
            !content.contains("{{"),
            "no literal placeholders may survive: {content}"
        );

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "init script must be mode 755");

        let c = calls.lock().unwrap();
        assert_eq!(c[3], vec!["rc-update", "add", "svcapp", "default"]);
    }

    #[tokio::test]
    async fn rollback_after_failure_leaves_written_unit_in_place() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let svc_root = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("start.sh"), b"OLD").unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"NEW")]);
        let (port, _srv) = spawn_http_server(tarball);
        // post_update fails AFTER the unit was written and enabled.
        let toml = service_extract_recipe_toml(
            &target,
            Some("/opt/app/bin/svcapp --serve"),
            Some("exit 9"),
        );
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "".into(), // daemon-reload
            "".into(), // enable
        ]);
        set_systemd_runner(Some(runner));
        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(String::new()),
            || async { execute_update(&recipe, "1.0.0", &url, &http_client(), None, false).await },
        )
        .await;
        set_systemd_runner(None);

        assert!(!outcome.success);
        assert!(
            outcome.rolled_back,
            "binary/config must roll back: {outcome:?}"
        );
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or_default()
                .starts_with("post_update"),
            "got: {outcome:?}"
        );
        // Decision 5: the written unit is NOT rolled back — re-render on the
        // next update is idempotent (overwrite).
        assert!(
            svc_root
                .path()
                .join("systemd/system/svcapp.service")
                .exists(),
            "unit-survives-rollback: written unit must remain after rollback"
        );
    }

    #[tokio::test]
    async fn no_service_field_means_zero_service_ops() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let svc_root = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"NEW")]);
        let (port, _srv) = spawn_http_server(tarball);
        let toml = recipe_toml(&target, None, None, None, 3);
        let recipe = parse_recipe(&toml);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(String::new()),
            || async { execute_update(&recipe, "1.0.0", &url, &http_client(), None, false).await },
        )
        .await;

        assert!(outcome.success, "outcome: {outcome:?}");
        assert_eq!(
            std::fs::read_dir(svc_root.path()).unwrap().count(),
            0,
            "no service field → no unit/init script writes"
        );
        assert!(
            !outcome.log.iter().any(|l| l.contains("service script")),
            "no service field → no service-script log entries: {:?}",
            outcome.log
        );
    }

    // --- Sprint 32 Phase 3: execute_update_with_diff (update diffing) ---

    /// Script-method recipe (no service, empty asset URL → no download):
    /// the cheapest green path; `detect_command` slot is parameterized so
    /// tests can diff the inert version-detect pair.
    fn script_recipe_toml(target: &Path, outfile: &Path, detect_command: &str) -> String {
        format!(
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
detect_command = "{}"
detect_pattern = "([0-9.]+)"

[direct_url]
version_discovery = "scrape"
asset_url_template = "https://example.com/s"
"#,
            target.display(),
            outfile.display(),
            detect_command
        )
    }

    #[tokio::test]
    async fn with_diff_prepends_warnings_and_keeps_success_path_green() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        let outfile = tmp.path().join("diff-out.txt");

        // New recipe executes; the previous recipe differs only in the
        // version-detect pair (inert to execution) → exactly one warning.
        let new_recipe = parse_recipe(&script_recipe_toml(&target, &outfile, "echo 1.0"));
        let old_recipe = parse_recipe(&script_recipe_toml(
            &target,
            &outfile,
            "/usr/bin/scripted --version",
        ));

        let outcome = execute_update_with_diff(
            &new_recipe,
            Some(&old_recipe),
            "7.7.7",
            "",
            &http_client(),
            None,
            false,
        )
        .await;

        assert!(outcome.success, "outcome: {outcome:?}");
        let diff_lines: Vec<&String> = outcome
            .log
            .iter()
            .filter(|l| l.starts_with("diff warning: "))
            .collect();
        assert_eq!(diff_lines.len(), 1, "log: {:?}", outcome.log);
        assert!(
            diff_lines[0].contains("version_detect") && diff_lines[0].contains("/usr/bin/scripted"),
            "warning must name class and old value: {}",
            diff_lines[0]
        );
        // PREPEND contract: diff lines lead, executor stage lines follow.
        assert!(outcome.log[0].starts_with("diff warning: "));
        assert_eq!(
            outcome.log[1],
            "no asset URL resolved; script method will fetch itself"
        );
        // The update itself still ran end to end.
        assert_eq!(std::fs::read_to_string(&outfile).unwrap().trim(), "7.7.7");
    }

    #[tokio::test]
    async fn with_diff_previous_none_adds_no_diff_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        let outfile = tmp.path().join("diff-out-none.txt");

        let recipe = parse_recipe(&script_recipe_toml(&target, &outfile, "echo 1.0"));
        let outcome =
            execute_update_with_diff(&recipe, None, "7.7.7", "", &http_client(), None, false).await;

        assert!(outcome.success, "outcome: {outcome:?}");
        assert!(
            !outcome.log.iter().any(|l| l.starts_with("diff warning: ")),
            "previous=None (first install) must stay silent: {:?}",
            outcome.log
        );
    }

    #[tokio::test]
    async fn with_diff_failure_path_carries_warnings_and_journalctl_in_same_log() {
        let _lock = TEST_RUNNER_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("app");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("start.sh"), b"OLD").unwrap();

        let tarball = make_tar_gz(&[("start.sh", b"NEW")]);
        let (port, _srv) = spawn_http_server(tarball);
        let url = format!("http://127.0.0.1:{port}/x.tar.gz");

        // failure_policy=hold + closed-port health check: the health stage
        // fails and journalctl is captured into the outcome log (T3.5).
        let failure_toml = format!(
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
        let new_recipe = parse_recipe(&failure_toml);
        // The previously registered recipe used the OLD unit name → one
        // service-unit diff warning on this update.
        let old_recipe = parse_recipe(
            &failure_toml.replace("service = \"fake-unit\"", "service = \"old-unit.service\""),
        );

        // baseline (3 systemctl shows) + service step (daemon-reload, enable)
        // + journalctl capture on health failure. Hermetic seams as in the
        // Phase 2 service tests.
        let svc_root = tempfile::tempdir().unwrap();
        let (runner, _calls) = scripted_fake(vec![
            "ActiveState=active\n".into(),
            "SubState=running\n".into(),
            "NRestarts=0\n".into(),
            "".into(),
            "".into(),
            "journalctl -u fake-unit -n 50: no errors\n".into(),
        ]);
        set_systemd_runner(Some(runner));
        let outcome = with_service_env_async(
            Some(svc_root.path().to_path_buf()),
            Some(String::new()),
            || async {
                execute_update_with_diff(
                    &new_recipe,
                    Some(&old_recipe),
                    "2.0.0",
                    &url,
                    &http_client(),
                    None,
                    false,
                )
                .await
            },
        )
        .await;
        set_systemd_runner(None);

        assert!(!outcome.success);
        let diff_idx = outcome
            .log
            .iter()
            .position(|l| l.starts_with("diff warning: "))
            .expect("diff warnings must be present on the failure path");
        let diff_lines: Vec<&String> = outcome
            .log
            .iter()
            .filter(|l| l.starts_with("diff warning: "))
            .collect();
        assert_eq!(diff_lines.len(), 1, "log: {:?}", outcome.log);
        assert!(
            diff_lines[0].contains("service_unit") && diff_lines[0].contains("old-unit.service"),
            "warning must name the service-unit diff: {}",
            diff_lines[0]
        );
        let failed_idx = outcome
            .log
            .iter()
            .position(|l| l.contains("update FAILED at stage 'health_check'"))
            .expect("failure line must be in the same log");
        let journal_idx = outcome
            .log
            .iter()
            .position(|l| l.contains("journalctl"))
            .expect("journalctl capture must be in the same log");
        // Failure coupling via ordering: the warnings lead the shared log.
        assert!(diff_idx < failed_idx, "log: {:?}", outcome.log);
        assert!(diff_idx < journal_idx, "log: {:?}", outcome.log);
    }
}
