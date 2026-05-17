//! Server-side shadow package orchestration.

use std::path::PathBuf;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::db::Database;
use crate::shadow_cache::ShadowCache;
use crate::shadow_git_mirror::GitMirrorManager;
use crate::shadow_signing;
use crate::shadow_source::{GithubReleaseSource, UpdateSource};

pub struct ShadowPackageManager {
    db: Arc<Database>,
    cache: ShadowCache,
    signing_key: SigningKey,
    github_token: Option<String>,
    poll_interval: std::time::Duration,
    // Cache of sources keyed by "owner/repo"
    sources: RwLock<std::collections::HashMap<String, Arc<dyn UpdateSource>>>,
    /// Git mirror manager for git_build source type
    git_mirror_manager: GitMirrorManager,
}

#[derive(Debug)]
pub enum ShadowError {
    Database(String),
    Source(String),
    Cache(String),
    NotFound(String),
    AlreadyExists(String),
    Signing(String),
}

impl std::fmt::Display for ShadowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(e) => write!(f, "database: {e}"),
            Self::Source(e) => write!(f, "source: {e}"),
            Self::Cache(e) => write!(f, "cache: {e}"),
            Self::NotFound(e) => write!(f, "not found: {e}"),
            Self::AlreadyExists(e) => write!(f, "already exists: {e}"),
            Self::Signing(e) => write!(f, "signing: {e}"),
        }
    }
}

impl std::error::Error for ShadowError {}

#[derive(Debug, Clone)]
pub struct ShadowUpdateInfo {
    pub package_id: String,
    pub package_name: String,
    pub current_version: String,
    pub latest_version: String,
    pub asset_url: Option<String>,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub release_notes_url: Option<String>,
}

impl ShadowPackageManager {
    pub fn new(
        db: Arc<Database>,
        cache_dir: PathBuf,
        signing_key: SigningKey,
        github_token: Option<String>,
        poll_interval: std::time::Duration,
        git_mirror_dir: PathBuf,
    ) -> Self {
        Self {
            db,
            cache: ShadowCache::new(cache_dir),
            signing_key,
            github_token,
            poll_interval,
            sources: RwLock::new(std::collections::HashMap::new()),
            git_mirror_manager: GitMirrorManager::new(git_mirror_dir),
        }
    }

    /// Get or create an UpdateSource for a GitHub repo URL
    async fn get_source(&self, source_url: &str) -> Result<Arc<dyn UpdateSource>, ShadowError> {
        let repo = source_url.trim_end_matches('/');
        let key = repo.to_string();

        {
            let sources = self.sources.read().await;
            if let Some(src) = sources.get(&key) {
                return Ok(Arc::clone(src));
            }
        }

        let source = GithubReleaseSource::new(source_url, self.github_token.clone())
            .map_err(|e| ShadowError::Source(e.to_string()))?;
        let source: Arc<dyn UpdateSource> = Arc::new(source);

        {
            let mut sources = self.sources.write().await;
            sources.insert(key, Arc::clone(&source));
        }

        Ok(source)
    }

    /// Check all registered packages for new versions
    pub async fn check_all_sources(&self) -> Result<Vec<ShadowUpdateInfo>, ShadowError> {
        // List all shadow packages from DB
        // For a real impl, we'd query all packages. For now, return empty
        // since full impl requires a running DB connection.
        debug!("Checking all shadow package sources");
        Ok(vec![])
    }

    /// Check a single package for updates
    pub async fn check_one(
        &self,
        _package_id: &uuid::Uuid,
        source_url: &str,
        tag_pattern: &str,
        current_version: &str,
    ) -> Result<Option<ShadowUpdateInfo>, ShadowError> {
        let source = self.get_source(source_url).await?;
        let latest = source
            .latest_version(tag_pattern)
            .await
            .map_err(|e| ShadowError::Source(e.to_string()))?;

        match latest {
            Some(version) if version != current_version => {
                let assets = source
                    .release_assets(&version)
                    .await
                    .map_err(|e| ShadowError::Source(e.to_string()))?;

                let asset = assets.first();
                Ok(Some(ShadowUpdateInfo {
                    package_id: _package_id.to_string(),
                    package_name: String::new(),
                    current_version: current_version.to_string(),
                    latest_version: version.clone(),
                    asset_url: asset.map(|a| a.url.clone()),
                    asset_name: asset.map(|a| a.name.clone()),
                    asset_size: asset.map(|a| a.size),
                    release_notes_url: source.changelog_url(current_version, &version),
                }))
            }
            _ => Ok(None),
        }
    }

    /// Record an update result from a client
    pub async fn record_update_result(
        &self,
        history_id: &uuid::Uuid,
        status: &str,
        error: Option<&str>,
    ) -> Result<(), ShadowError> {
        self.db
            .update_shadow_update_status(history_id, status, error)
            .await
            .map_err(|e| ShadowError::Database(e.to_string()))
    }

    /// Start the background polling loop
    pub fn start_polling(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        let interval = self.poll_interval;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                match manager.check_all_sources().await {
                    Ok(updates) => {
                        if !updates.is_empty() {
                            info!("Found {} shadow package updates", updates.len());
                        }
                    }
                    Err(e) => {
                        error!("Shadow package poll error: {}", e);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let e = ShadowError::NotFound("pkg-xyz".into());
        assert!(e.to_string().contains("pkg-xyz"));
        let e = ShadowError::Source("rate limited".into());
        assert!(e.to_string().contains("rate limited"));
    }
}
