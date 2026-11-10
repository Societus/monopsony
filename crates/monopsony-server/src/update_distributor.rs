//! Server-side update distributor (Sprint 34 P1).
//!
//! Pulls each GitHub release ONCE over the WAN (D2), verifies it, and stores
//! it content-addressed in the shadow cache (D5) so every client can be
//! served over the LAN by the P2 gRPC surface. This is the same
//! single-point-of-cache economics as package caching, applied to the
//! monopsony binaries themselves.
//!
//! Verification policy (D7): every tarball's sha256 is checked against the
//! release's `SHA256SUMS`. GPG verification of `SHA256SUMS.sig` applies ONLY
//! when the operator pinned `trusted_gpg_key` in `[update]` — then the
//! signature is REQUIRED and must verify (fail-closed); without a pinned key
//! the trust basis is sha256-of-GitHub-served-bytes and a warning metric is
//! emitted. The rpgp mechanics are the proven mirror-integrity lane
//! ([`monopsony_mirror::integrity`]) — no keyserver fetches, no TOFU.
//!
//! Storage reuses the existing shadow cache conventions: blobs are
//! content-addressed (`store_blob`), lineage is recorded per
//! (package, version, platform) via `index_asset`, and retention (keep-N
//! releases, never pruning the server's own running version) removes DB rows
//! first, drops index entries, then sweeps unreferenced blobs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use monopsony_common::config::UpdateConfig;
use monopsony_db::{Database, UpdateArtifactInsert};
use monopsony_mirror::integrity::{load_trusted_keys, verify_detached_release};
use monopsony_shadow::shadow_cache::ShadowCache;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

/// Platforms the release CI publishes (`.github/workflows/release.yml`).
pub const RELEASE_PLATFORMS: [&str; 4] = [
    "linux-x86_64",
    "linux-aarch64",
    "darwin-arm64",
    "darwin-x86_64",
];

/// The two distributable binaries; release assets are kind-prefixed.
pub const BINARY_KINDS: [&str; 2] = ["server", "client"];

/// Release checksum manifest the CI generates (`sha256sum *.tar.gz`).
pub const SUMS_NAME: &str = "SHA256SUMS";

/// Detached armored signature over [`SUMS_NAME`] (present when the release
/// CI has GPG secrets configured).
pub const SUMS_SIG_NAME: &str = "SHA256SUMS.sig";

/// server_settings key overriding `[update].enabled` at runtime (JSON bool).
pub const UPDATE_ENABLED_SETTING: &str = "update_distributor_enabled";

/// User-Agent for GitHub API/download requests.
const USER_AGENT: &str = "monopsony-update-distributor";

/// List published releases for a repo (newest GitHub-order first). A 404
/// means the repo has no published releases — "no releases", not an error
/// (the existing self-update convention; the public mirror may lag the tag).
pub async fn list_github_releases(
    repo: &str,
    github_token: Option<&str>,
    http: &reqwest::Client,
) -> Result<Vec<GhRelease>> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    let mut rb = http.get(&url).header("User-Agent", USER_AGENT);
    if let Some(token) = github_token {
        rb = rb.header("Authorization", format!("Bearer {token}"));
    }
    let resp = rb.send().await.context("GitHub releases request")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    if !resp.status().is_success() {
        bail!(
            "GitHub API returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
    }
    resp.json().await.context("decode GitHub releases JSON")
}

/// Failure of one asset's sha256 verification against the SUMS manifest.
#[derive(Debug, PartialEq, Eq)]
pub enum AssetVerifyError {
    /// Asset not listed in SHA256SUMS — nothing anchors its bytes.
    NotListed,
    /// Digest mismatch (fail-closed).
    Mismatch { expected: String, got: String },
}

/// Verify one downloaded asset's sha256 against the parsed SUMS entries.
/// Fail-closed: an asset the manifest does not list is a failure, not a
/// pass-with-warning — release CI lists every published tarball, so an
/// unlisted name means the manifest and the release disagree.
pub fn verify_asset_sha256(
    asset_name: &str,
    bytes: &[u8],
    sums: &[(String, String)],
) -> Result<(), AssetVerifyError> {
    let Some((_, expected)) = sums.iter().find(|(n, _)| n == asset_name) else {
        monopsony_common::metrics::update_hash_verify("unlisted_fail_closed");
        return Err(AssetVerifyError::NotListed);
    };
    let got = hex::encode(Sha256::digest(bytes));
    if got != expected.to_ascii_lowercase() {
        monopsony_common::metrics::update_hash_verify("mismatch_fail_closed");
        return Err(AssetVerifyError::Mismatch {
            expected: expected.clone(),
            got,
        });
    }
    monopsony_common::metrics::update_hash_verify("pass");
    Ok(())
}

/// Release asset name for one binary/platform, per the release contract:
/// `monopsony-{kind}-v{version}-{platform}.tar.gz`.
pub fn tarball_name(kind: &str, version: &str, platform: &str) -> String {
    format!("monopsony-{kind}-v{version}-{platform}.tar.gz")
}

/// ShadowCache index package name for one binary kind.
pub fn kind_package(kind: &str) -> String {
    format!("monopsony-{kind}")
}

/// GitHub release asset (the fields the distributor consumes).
#[derive(Debug, Deserialize)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    #[allow(dead_code)]
    pub size: u64,
}

/// GitHub release (the fields the distributor consumes).
#[derive(Debug, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    pub html_url: String,
    pub prerelease: bool,
    pub draft: bool,
    pub assets: Vec<GhAsset>,
}

/// One poll cycle's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// A new release was fetched, verified, and stored.
    Fetched(String),
    /// The chosen release is already fully stored.
    AlreadyCached(String),
    /// No eligible release on the repo (404 or nothing parseable).
    NoRelease,
}

/// Failure of the D7 SHA256SUMS signature policy.
#[derive(Debug, PartialEq, Eq)]
pub enum SumsSigError {
    /// A key is pinned but the release carries no signature asset.
    MissingSignature,
    /// Key malformed, signature malformed, or verification failed.
    Invalid(String),
}

/// The update distributor: poll → verify → store → retain.
pub struct UpdateDistributor {
    db: Arc<Database>,
    cache: ShadowCache,
    config: UpdateConfig,
    github_token: Option<String>,
    http: reqwest::Client,
}

impl UpdateDistributor {
    /// Build a distributor. `cache_dir` should be the shadow manager's cache
    /// base (same store — ShadowCache is a stateless base-path wrapper, so
    /// blobs dedup across the two consumers).
    pub fn new(
        db: Arc<Database>,
        cache_dir: PathBuf,
        config: UpdateConfig,
        github_token: Option<String>,
    ) -> Self {
        Self {
            db,
            cache: ShadowCache::new(cache_dir),
            config,
            github_token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Effective enabled state: `server_settings[UPDATE_ENABLED_SETTING]`
    /// (JSON bool) overrides `[update].enabled` when present — the runtime
    /// toggle the dashboard can flip without a restart.
    async fn enabled_now(&self) -> bool {
        match self.db.get_setting(UPDATE_ENABLED_SETTING).await {
            Ok(Some(v)) => v.as_bool().unwrap_or(self.config.enabled),
            _ => self.config.enabled,
        }
    }

    /// One poll cycle: choose the latest eligible release, skip if already
    /// stored complete, else fetch + verify + store, then apply retention.
    pub async fn poll_once(&self, own_version: &str) -> Result<PollOutcome> {
        let releases =
            list_github_releases(&self.config.repo, self.github_token.as_deref(), &self.http)
                .await
                .context("list GitHub releases")?;
        let Some(release) = choose_latest_release(&releases, self.config.include_prerelease) else {
            monopsony_common::metrics::update_release_fetched("none");
            return Ok(PollOutcome::NoRelease);
        };
        // choose_latest_release only returns semver-parseable tags.
        let Some(version) = parse_version_tag(&release.tag_name) else {
            bail!(
                "chosen release tag {} is not a semver version",
                release.tag_name
            );
        };

        let already = self
            .db
            .get_update_release(&version)
            .await
            .context("read update_releases")?
            .is_some_and(|r| r.assets_complete);
        if already {
            monopsony_common::metrics::update_release_fetched("already_cached");
            return Ok(PollOutcome::AlreadyCached(version));
        }

        info!(
            "update distributor: fetching release {} -> {}",
            release.tag_name, version
        );
        self.fetch_release(release, &version).await?;
        monopsony_common::metrics::update_release_fetched("new");

        let pruned = self.apply_retention(own_version).await.unwrap_or_else(|e| {
            warn!("update retention failed: {e:#}");
            0
        });
        if pruned > 0 {
            info!(count = pruned, "update retention pruned releases");
        }
        Ok(PollOutcome::Fetched(version))
    }

    /// Request builder with the shared User-Agent and the optional GitHub
    /// token (the `MONOPSONY_GITHUB_TOKEN` / `--github-token` path already
    /// wired through server main.rs).
    fn gh_request(&self, url: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.get(url).header("User-Agent", USER_AGENT);
        if let Some(token) = &self.github_token {
            rb = rb.header("Authorization", format!("Bearer {token}"));
        }
        rb
    }

    /// Download one release asset by name; `None` when the release does not
    /// carry it (or GitHub 404s the download).
    async fn download_asset_opt(&self, release: &GhRelease, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(url) = release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.as_str())
        else {
            return Ok(None);
        };
        let resp = self
            .gh_request(url)
            .send()
            .await
            .with_context(|| format!("download {name}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            bail!("download {name}: status {}", resp.status());
        }
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("read body of {name}"))?;
        Ok(Some(bytes.to_vec()))
    }

    /// Like [`Self::download_asset_opt`] but required.
    async fn download_asset(&self, release: &GhRelease, name: &str) -> Result<Vec<u8>> {
        self.download_asset_opt(release, name)
            .await?
            .ok_or_else(|| anyhow!("asset {name} not present on release {}", release.tag_name))
    }

    /// Fetch, verify, and store one release. Every tarball is sha256-verified
    /// against `SHA256SUMS` before anything is stored; the release row is
    /// marked complete only after all 8 platform tarballs are indexed, so the
    /// DB row is the completeness gate for serving (P2).
    async fn fetch_release(&self, release: &GhRelease, version: &str) -> Result<()> {
        let release_id = self
            .db
            .upsert_update_release(version, release.prerelease, Some(&release.html_url))
            .await
            .context("upsert update_releases row")?;

        // The checksum manifest anchors every per-asset verification.
        let sums_bytes = self
            .download_asset(release, SUMS_NAME)
            .await
            .with_context(|| {
                format!(
                    "SHA256SUMS unavailable on release {} — cannot verify assets",
                    release.tag_name
                )
            })?;

        // The signature asset is only downloaded when a key is pinned — an
        // unpinned server runs sha256-only (D7) and skips the request.
        let pinned = self.config.trusted_gpg_key.as_deref();
        let sig_bytes = if pinned.is_some() {
            self.download_asset_opt(release, SUMS_SIG_NAME).await?
        } else {
            None
        };
        match verify_sums_signature(&sums_bytes, sig_bytes.as_deref(), pinned) {
            Ok(()) => {
                if pinned.is_some() {
                    monopsony_common::metrics::update_signature_verify("pass");
                } else {
                    monopsony_common::metrics::update_signature_verify("no_key");
                    warn!(
                        "no trusted_gpg_key pinned — trusting sha256 of GitHub-served bytes \
                         only (pin the release key in [update] to close this)"
                    );
                }
            }
            Err(SumsSigError::MissingSignature) => {
                monopsony_common::metrics::update_signature_verify("missing_sig_fail_closed");
                bail!(
                    "trusted_gpg_key is pinned but {} is missing on release {} — fail-closed",
                    SUMS_SIG_NAME,
                    release.tag_name
                );
            }
            Err(e @ SumsSigError::Invalid(_)) => {
                monopsony_common::metrics::update_signature_verify("fail_fail_closed");
                bail!(
                    "SHA256SUMS signature verification failed ({e:?}) — release {} not stored",
                    release.tag_name
                );
            }
        }

        let sums_text =
            std::str::from_utf8(&sums_bytes).context("SHA256SUMS is not valid UTF-8")?;
        let sums = parse_sha256_sums(sums_text);
        if sums.is_empty() {
            bail!(
                "SHA256SUMS on release {} parsed to zero entries",
                release.tag_name
            );
        }

        for kind in BINARY_KINDS {
            for platform in RELEASE_PLATFORMS {
                let name = tarball_name(kind, version, platform);
                let bytes = self
                    .download_asset(release, &name)
                    .await
                    .with_context(|| format!("fetch {name}"))?;
                if let Err(e) = verify_asset_sha256(&name, &bytes, &sums) {
                    match e {
                        AssetVerifyError::NotListed => {
                            bail!("{name} is not listed in SHA256SUMS — refusing to store")
                        }
                        AssetVerifyError::Mismatch { expected, got } => {
                            bail!(
                                "{name}: sha256 mismatch (expected {expected}, got {got}) — not stored"
                            )
                        }
                    }
                }

                // Verified: content-addressed store + lineage index + DB row.
                // store_blob's digest IS the verified sha256.
                let blob_hash = self
                    .cache
                    .store_blob(&bytes)
                    .await
                    .context("shadow cache store_blob")?;
                self.cache
                    .index_asset(
                        &kind_package(kind),
                        version,
                        platform,
                        &blob_hash,
                        &name,
                        bytes.len() as u64,
                    )
                    .await
                    .context("shadow cache index_asset")?;
                self.db
                    .insert_update_artifact(
                        &release_id,
                        &UpdateArtifactInsert {
                            package: kind_package(kind),
                            platform: (*platform).to_string(),
                            asset_name: name.clone(),
                            sha256: blob_hash,
                            size: bytes.len() as i64,
                            blob_indexed: true,
                        },
                    )
                    .await
                    .with_context(|| format!("insert artifact row for {name}"))?;
                debug!("{name} verified + stored ({} bytes)", bytes.len());
            }
        }

        self.db
            .set_update_release_complete(&release_id)
            .await
            .context("mark update release complete")?;
        info!("update distributor: stored release {version} (8 platform tarballs)");
        Ok(())
    }

    /// Retention: keep the newest [`UpdateConfig::retain_versions`] complete
    /// releases plus the server's own running version; delete everything
    /// else. Order is DB-rows-first (artifact rows cascade), then cache index
    /// entries, then one gc sweep of unreferenced blobs.
    async fn apply_retention(&self, own_version: &str) -> Result<u64> {
        let releases = self
            .db
            .list_update_releases(50)
            .await
            .context("list update releases for retention")?;
        if releases.is_empty() {
            return Ok(0);
        }
        let all: Vec<String> = releases.iter().map(|r| r.version.clone()).collect();
        let complete: Vec<String> = releases
            .iter()
            .filter(|r| r.assets_complete)
            .map(|r| r.version.clone())
            .collect();
        let prune = retention_prune_set(
            &all,
            &complete,
            self.config.retain_versions as usize,
            own_version,
        );
        let mut pruned: u64 = 0;
        for version in prune {
            if self.db.delete_update_release(&version).await? {
                for kind in BINARY_KINDS {
                    for platform in RELEASE_PLATFORMS {
                        let _ = self
                            .cache
                            .remove_asset(&kind_package(kind), &version, platform)
                            .await;
                    }
                }
                // Sprint 34 P2: a pruned release's blobs are gone — its open
                // rollout assignments would push clients toward bytes that no
                // longer download. Clear them with the release.
                match self
                    .db
                    .delete_update_assignments_for_version(&version)
                    .await
                {
                    Ok(cleared) if cleared > 0 => {
                        warn!(
                            version = %version,
                            cleared,
                            "update retention: cleared rollout assignments for pruned release"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => warn!(
                        version = %version,
                        error = %e,
                        "update retention: failed to clear assignments for pruned release"
                    ),
                }
                pruned += 1;
                debug!("update retention: pruned release {version}");
            }
        }
        if pruned > 0 {
            let freed = self.cache.gc(0.0).await.unwrap_or(0);
            monopsony_common::metrics::update_retention_pruned(pruned);
            debug!("update retention: gc reclaimed {freed} bytes");
        }
        Ok(pruned)
    }
}

/// Background poll loop: poll on startup, then every
/// `[update].check_interval` seconds or on an admin "check now" trigger.
/// The runtime toggle ([`UPDATE_ENABLED_SETTING`]) is honored per iteration,
/// so the dashboard can enable/disable the distributor live.
pub async fn run_poll_loop(
    distributor: Arc<UpdateDistributor>,
    check_signal: Arc<tokio::sync::Notify>,
    own_version: String,
) {
    // Clamp to >= 1s: tokio's interval panics on a zero duration, so a
    // config typo of `check_interval = 0` must not kill the poller task.
    let interval_secs = distributor.config.check_interval.max(1);
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if distributor.enabled_now().await {
            match distributor.poll_once(&own_version).await {
                Ok(PollOutcome::Fetched(v)) => info!("update distributor: stored {v}"),
                Ok(PollOutcome::AlreadyCached(v)) => {
                    debug!("update distributor: {v} already cached")
                }
                Ok(PollOutcome::NoRelease) => debug!("update distributor: no releases published"),
                Err(e) => warn!("update distributor poll failed: {e:#}"),
            }
        }
        tokio::select! {
            _ = ticker.tick() => {}
            _ = check_signal.notified() => {
                info!("update distributor: check triggered via admin API");
            }
        }
    }
}

/// Strip the leading `v` from a release tag and validate the rest as a
/// semver version (the release contract's `\d+\.\d+\.\d+(-rcN)?` shape).
/// Returns the bare version string ("v1.2.3" → "1.2.3").
pub fn parse_version_tag(tag: &str) -> Option<String> {
    let clean = tag.trim_start_matches('v');
    semver::Version::parse(clean).ok()?;
    Some(clean.to_string())
}

/// Choose the release to distribute: highest semver tag among published
/// (`!draft`) releases, honoring `include_prerelease` (default true — the
/// fleet ships rcs). Tags that are not valid semver are skipped, never
/// selected. GitHub's `/releases` ordering is not trusted for this choice.
pub fn choose_latest_release(
    releases: &[GhRelease],
    include_prerelease: bool,
) -> Option<&GhRelease> {
    let mut best: Option<(semver::Version, &GhRelease)> = None;
    for release in releases {
        if release.draft {
            continue;
        }
        if release.prerelease && !include_prerelease {
            continue;
        }
        let Ok(parsed) = semver::Version::parse(release.tag_name.trim_start_matches('v')) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| parsed > *b) {
            best = Some((parsed, release));
        }
    }
    best.map(|(_, release)| release)
}

/// Parse a `sha256sum` manifest (`<hex64>  <name>` per line, as generated by
/// the release CI's `sha256sum *.tar.gz`) into (name, lowercase-hash) pairs.
/// Hashes are normalized to lowercase; malformed lines are skipped — the
/// caller treats a missing entry as a fail-closed verification failure.
pub fn parse_sha256_sums(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (hash, name) = match (parts.next(), parts.next()) {
            (Some(h), Some(n)) if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) => {
                // sha256sum binary mode prefixes the name with '*'; text mode
                // (what release CI emits) has none — tolerate both.
                (h.to_ascii_lowercase(), n.strip_prefix('*').unwrap_or(n))
            }
            _ => {
                debug!("SHA256SUMS: skipping malformed line: {line}");
                continue;
            }
        };
        out.push((name.to_string(), hash));
    }
    out
}

/// D7 signature policy for `SHA256SUMS`:
///
/// - No pinned key → sha256-only trust: OK regardless of signature presence
///   (the caller emits the `no_key` warning metric).
/// - Pinned key + missing signature asset → fail-closed
///   ([`SumsSigError::MissingSignature`]).
/// - Pinned key + signature → armored detached verification against the
///   pinned key via the proven mirror-integrity mechanics; any failure is
///   fail-closed ([`SumsSigError::Invalid`]).
pub fn verify_sums_signature(
    sums_bytes: &[u8],
    sig_bytes: Option<&[u8]>,
    pinned_key: Option<&str>,
) -> Result<(), SumsSigError> {
    let Some(key_text) = pinned_key else {
        return Ok(());
    };
    let Some(sig_armored) = sig_bytes else {
        return Err(SumsSigError::MissingSignature);
    };
    let keys = load_trusted_keys(&[key_text.to_string()])
        .map_err(|e| SumsSigError::Invalid(e.to_string()))?;
    verify_detached_release(sums_bytes, sig_armored, &keys)
        .map_err(|e| SumsSigError::Invalid(e.to_string()))
}

/// Compute the retention prune set: everything NOT in the newest `keep`
/// complete releases and NOT the server's own running version. Only
/// semver-parseable versions are ever pruned (unparseable rows are inert
/// metadata this pipeline never created). Returned in no particular order.
pub fn retention_prune_set(
    all_versions: &[String],
    complete_versions: &[String],
    keep: usize,
    own_version: &str,
) -> Vec<String> {
    let mut newest_complete: Vec<(semver::Version, &String)> = complete_versions
        .iter()
        .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (sv, v)))
        .collect();
    newest_complete.sort_by(|a, b| b.0.cmp(&a.0));
    let mut retain: std::collections::HashSet<String> = newest_complete
        .iter()
        .take(keep)
        .map(|(_, v)| (*v).clone())
        .collect();
    retain.insert(own_version.to_string());
    all_versions
        .iter()
        .filter(|v| !retain.contains(v.as_str()))
        .filter(|v| semver::Version::parse(v).is_ok())
        .filter(|v| v.as_str() != own_version)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // SYNTHETIC fixtures only (throwaway keys, synthetic labels/hashes) per
    // house convention. No real digests, no real keys, no fixture directory.

    /// Wire-format fixture in the real GitHub `/releases` JSON shape
    /// (truncated to the fields this module deserializes).
    const RELEASES_JSON: &str = r#"[
        {
            "url": "https://api.github.com/repos/societus/monopsony/releases/1",
            "tag_name": "v1.1.0-rc.2",
            "html_url": "https://github.com/societus/monopsony/releases/tag/v1.1.0-rc.2",
            "prerelease": true,
            "draft": false,
            "assets": [
                {
                    "name": "monopsony-server-v1.1.0-rc.2-linux-x86_64.tar.gz",
                    "browser_download_url": "https://example.invalid/srv.tar.gz",
                    "size": 123456
                },
                {
                    "name": "SHA256SUMS",
                    "browser_download_url": "https://example.invalid/SHA256SUMS",
                    "size": 800
                },
                {
                    "name": "SHA256SUMS.sig",
                    "browser_download_url": "https://example.invalid/SHA256SUMS.sig",
                    "size": 566
                }
            ]
        },
        {
            "url": "https://api.github.com/repos/societus/monopsony/releases/2",
            "tag_name": "v1.0.0",
            "html_url": "https://github.com/societus/monopsony/releases/tag/v1.0.0",
            "prerelease": false,
            "draft": false,
            "assets": [
                {
                    "name": "monopsony-server-v1.0.0-linux-x86_64.tar.gz",
                    "browser_download_url": "https://example.invalid/old-srv.tar.gz",
                    "size": 111111
                }
            ]
        },
        {
            "url": "https://api.github.com/repos/societus/monopsony/releases/3",
            "tag_name": "v1.2.0",
            "html_url": "https://github.com/societus/monopsony/releases/tag/v1.2.0",
            "prerelease": false,
            "draft": true,
            "assets": []
        }
    ]"#;

    fn test_config() -> UpdateConfig {
        UpdateConfig::default()
    }

    #[test]
    fn tarball_names_match_release_contract() {
        // The 8 names exactly as .github/workflows/release.yml publishes.
        let expected: Vec<&str> = [
            "monopsony-server-v1.2.3-linux-x86_64.tar.gz",
            "monopsony-server-v1.2.3-linux-aarch64.tar.gz",
            "monopsony-server-v1.2.3-darwin-arm64.tar.gz",
            "monopsony-server-v1.2.3-darwin-x86_64.tar.gz",
            "monopsony-client-v1.2.3-linux-x86_64.tar.gz",
            "monopsony-client-v1.2.3-linux-aarch64.tar.gz",
            "monopsony-client-v1.2.3-darwin-arm64.tar.gz",
            "monopsony-client-v1.2.3-darwin-x86_64.tar.gz",
        ]
        .to_vec();
        let mut built = Vec::new();
        for kind in BINARY_KINDS {
            for platform in RELEASE_PLATFORMS {
                built.push(tarball_name(kind, "1.2.3", platform));
            }
        }
        let built: Vec<&str> = built.iter().map(|s| s.as_str()).collect();
        assert_eq!(built, expected);
    }

    #[test]
    fn github_releases_wire_format_fixture_parses() {
        let releases: Vec<GhRelease> = serde_json::from_str(RELEASES_JSON).expect("parse");
        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0].tag_name, "v1.1.0-rc.2");
        assert!(releases[0].prerelease && !releases[0].draft);
        assert_eq!(releases[0].assets.len(), 3);
        assert_eq!(
            releases[0].assets[0].name,
            "monopsony-server-v1.1.0-rc.2-linux-x86_64.tar.gz"
        );
        assert_eq!(releases[0].assets[0].size, 123456);
        assert!(!releases[1].prerelease);
        assert!(releases[2].draft);
    }

    #[test]
    fn choose_latest_honors_prerelease_and_draft() {
        let releases: Vec<GhRelease> = serde_json::from_str(RELEASES_JSON).expect("parse");
        // Default include_prerelease=true: the rc is newest semver (draft
        // v1.2.0 is never eligible).
        let latest = choose_latest_release(&releases, true).expect("some release");
        assert_eq!(latest.tag_name, "v1.1.0-rc.2");
        // Stable-only servers: v1.0.0.
        let latest = choose_latest_release(&releases, false).expect("some release");
        assert_eq!(latest.tag_name, "v1.0.0");
    }

    #[test]
    fn choose_latest_skips_unparseable_tags() {
        let releases: Vec<GhRelease> = serde_json::from_str(
            r#"[
            {"tag_name": "not-a-version", "html_url": "u", "prerelease": false, "draft": false, "assets": []},
            {"tag_name": "v0.9.1", "html_url": "u", "prerelease": false, "draft": false, "assets": []}
        ]"#,
        )
        .expect("parse");
        let latest = choose_latest_release(&releases, true).expect("some release");
        assert_eq!(latest.tag_name, "v0.9.1");
        assert!(choose_latest_release(&[], true).is_none());
    }

    #[test]
    fn parse_version_tag_shapes() {
        assert_eq!(parse_version_tag("v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parse_version_tag("1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(
            parse_version_tag("v1.2.3-rc.1").as_deref(),
            Some("1.2.3-rc.1")
        );
        assert_eq!(parse_version_tag("v1.2"), None);
        assert_eq!(parse_version_tag("release-1"), None);
    }

    #[test]
    fn parse_sha256_sums_synthetic_manifest() {
        // SYNTHETIC labels + hashes: 'a'/'b' repeats, not real digests.
        let sums = parse_sha256_sums(&format!(
            "{}  monopsony-server-v1.0.0-linux-x86_64.tar.gz\n\
             {}  monopsony-client-v1.0.0-linux-x86_64.tar.gz\n\
             {} *monopsony-server-v1.0.0-darwin-arm64.tar.gz\n\
             garbage line without hash\n\
             cccc  short-hash-name.tar.gz\n\
             \n\
             # comment line\n",
            "A".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
        ));
        assert_eq!(sums.len(), 3);
        assert_eq!(
            sums[0],
            (
                "monopsony-server-v1.0.0-linux-x86_64.tar.gz".to_string(),
                "a".repeat(64)
            )
        );
        // Uppercase hex normalized to lowercase.
        assert_eq!(sums[1].1, "b".repeat(64));
        // sha256sum binary-marker form: leading '*' stripped from the name.
        assert_eq!(
            sums[2],
            (
                "monopsony-server-v1.0.0-darwin-arm64.tar.gz".to_string(),
                "c".repeat(64)
            )
        );
    }

    #[test]
    fn verify_asset_sha256_good_mismatch_unlisted() {
        let bytes = b"synthetic asset payload";
        let good = hex::encode(Sha256::digest(bytes));
        let name = "monopsony-server-v1.0.0-linux-x86_64.tar.gz";
        let mut sums = vec![(name.to_string(), good.clone())];

        // Good hash passes.
        assert!(verify_asset_sha256(name, bytes, &sums).is_ok());

        // Wrong declared hash → Mismatch carrying both digests.
        sums[0].1 = "e".repeat(64);
        match verify_asset_sha256(name, bytes, &sums) {
            Err(AssetVerifyError::Mismatch { expected, got }) => {
                assert_eq!(expected, "e".repeat(64));
                assert_eq!(got, good);
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }

        // Unlisted asset → NotListed (fail-closed).
        assert_eq!(
            verify_asset_sha256("other.tar.gz", bytes, &sums),
            Err(AssetVerifyError::NotListed)
        );
    }

    /// Throwaway fixture keypair (SYNTHETIC — never a trust anchor).
    fn synthetic_pgp_key() -> (pgp::composed::SignedSecretKey, String) {
        use pgp::composed::{KeyType, SecretKeyParamsBuilder};
        let params = SecretKeyParamsBuilder::default()
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(true)
            .primary_user_id("Monopsony Update Fixture <fixture@invalid>".into())
            .build()
            .expect("key params");
        let secret = params.generate(rand::thread_rng()).expect("keygen");
        let public_armored = pgp::composed::SignedPublicKey::from(secret.clone())
            .to_armored_string(Default::default())
            .expect("armor public key");
        (secret, public_armored)
    }

    /// Detached armored signature over `sums`, as release CI produces.
    fn sign_sums(secret: &pgp::composed::SignedSecretKey, sums: &[u8]) -> String {
        use pgp::composed::DetachedSignature as DetachedSigType;
        use pgp::crypto::hash::HashAlgorithm;
        use pgp::types::Password;
        DetachedSigType::sign_binary_data(
            rand::thread_rng(),
            // SignedSecretKey derefs to the packet-level SecretKey, which is
            // what implements SigningKey (rpgp 0.21).
            &**secret,
            &Password::empty(),
            HashAlgorithm::Sha256,
            Cursor::new(sums),
        )
        .expect("detached sign")
        .to_armored_string(Default::default())
        .expect("armor sig")
    }

    #[test]
    fn d7_signature_policy_matrix() {
        let sums_bytes: &[u8] =
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  monopsony-server-v1.0.0-linux-x86_64.tar.gz\n";
        let (secret, public_armored) = synthetic_pgp_key();
        let sig = sign_sums(&secret, sums_bytes);

        // No pinned key: sha256-only trust, signature presence irrelevant.
        assert_eq!(verify_sums_signature(sums_bytes, None, None), Ok(()));
        assert_eq!(
            verify_sums_signature(sums_bytes, Some(sig.as_bytes()), None),
            Ok(())
        );

        // Pinned + missing signature: fail-closed (D7).
        assert_eq!(
            verify_sums_signature(sums_bytes, None, Some(&public_armored)),
            Err(SumsSigError::MissingSignature)
        );

        // Pinned + valid signature: pass.
        assert_eq!(
            verify_sums_signature(sums_bytes, Some(sig.as_bytes()), Some(&public_armored)),
            Ok(())
        );

        // Pinned + signature by a DIFFERENT key: fail-closed.
        let (_, other_public) = synthetic_pgp_key();
        assert!(matches!(
            verify_sums_signature(sums_bytes, Some(sig.as_bytes()), Some(&other_public)),
            Err(SumsSigError::Invalid(_))
        ));

        // Pinned + tampered SUMS bytes: fail-closed.
        let mut tampered = sums_bytes.to_vec();
        tampered[0] ^= 0x01;
        assert!(matches!(
            verify_sums_signature(&tampered, Some(sig.as_bytes()), Some(&public_armored)),
            Err(SumsSigError::Invalid(_))
        ));

        // Pinned + garbage signature armor: fail-closed.
        assert!(matches!(
            verify_sums_signature(sums_bytes, Some(b"not a signature"), Some(&public_armored)),
            Err(SumsSigError::Invalid(_))
        ));
    }

    #[test]
    fn retention_prune_set_keep3_and_own_version() {
        let all: Vec<String> = ["0.9.5", "1.0.0", "1.1.0", "1.2.0", "1.3.0-rc.1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let complete: Vec<String> = ["0.9.5", "1.0.0", "1.1.0", "1.2.0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Server runs 0.9.5: old (outside the keep-3 window) but retained.
        // The keep-3 window is 1.2.0 / 1.1.0 / 1.0.0; the incomplete rc fetch
        // attempt beyond the window is pruned.
        let prune = retention_prune_set(&all, &complete, 3, "0.9.5");
        assert_eq!(prune, vec!["1.3.0-rc.1".to_string()]);

        // Tighter keep-1: everything but the newest complete is pruned,
        // except the running version.
        let prune = retention_prune_set(&all, &complete, 1, "0.9.5");
        assert_eq!(prune.len(), 3);
        assert!(prune.contains(&"1.0.0".to_string()));
        assert!(prune.contains(&"1.1.0".to_string()));
        assert!(prune.contains(&"1.3.0-rc.1".to_string()));

        // Unparseable rows are never pruned by this pipeline.
        let all: Vec<String> = ["broken-tag".to_string(), "1.0.0".to_string()].to_vec();
        let complete: Vec<String> = ["1.0.0".to_string()].to_vec();
        let prune = retention_prune_set(&all, &complete, 1, "9.9.9");
        assert!(prune.is_empty());
    }

    /// Network-dependent: accepts Ok AND Err (CI may rate-limit or be
    /// offline). Ok(Vec::new()) is the documented no-releases state.
    #[tokio::test]
    async fn live_github_listing_accepts_ok_and_err() {
        let http = reqwest::Client::new();
        if let Ok(releases) = list_github_releases("societus/monopsony", None, &http).await {
            for release in &releases {
                assert!(!release.tag_name.is_empty());
            }
        }
    }

    #[test]
    fn config_defaults_are_not_zero() {
        // Serde default fns, not bare defaults: 0 = instant timeout class.
        let cfg = test_config();
        assert!(cfg.enabled);
        assert_eq!(cfg.repo, "societus/monopsony");
        assert_eq!(cfg.check_interval, 21600);
        assert!(cfg.include_prerelease);
        assert_eq!(cfg.retain_versions, 3);
        assert!(cfg.trusted_gpg_key.is_none());
    }

    #[test]
    fn config_parses_without_update_section() {
        // Existing TOMLs without [update] must still parse (serde(default)).
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[serde(default)]
            update: UpdateConfig,
        }
        let wrapper: Wrapper = toml::from_str("").expect("empty toml parses");
        assert!(wrapper.update.enabled);
        assert_eq!(wrapper.update.check_interval, 21600);
    }

    #[test]
    fn config_parses_update_section() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[serde(default)]
            update: UpdateConfig,
        }
        let wrapper: Wrapper = toml::from_str(
            "[update]\nenabled = false\nrepo = \"acme/fork\"\ncheck_interval = 60\ninclude_prerelease = false\nretain_versions = 5\n",
        )
        .expect("parse [update]");
        assert!(!wrapper.update.enabled);
        assert_eq!(wrapper.update.repo, "acme/fork");
        assert_eq!(wrapper.update.check_interval, 60);
        assert!(!wrapper.update.include_prerelease);
        assert_eq!(wrapper.update.retain_versions, 5);
    }
}
