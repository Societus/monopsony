//! Cache purging system for old/unused packages

//!
//! This module provides functionality to periodically clean up old cache entries
//! based on last access time and optionally enforce maximum cache size limits.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sqlx::Row;
use tracing::{debug, error, info, warn};

use crate::db::Database;

/// Configuration for cache purging
#[derive(Debug, Clone)]
pub struct PurgeConfig {
    /// Path to cache directory
    pub cache_dir: PathBuf,
    /// Days before purging unreferenced packages
    pub purge_after_days: u32,
    /// Maximum cache size in GB (0 = unlimited)
    pub max_size_gb: u64,
}

impl Default for PurgeConfig {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::from("/var/cache/monopsony"),
            purge_after_days: 30,
            max_size_gb: 0,
        }
    }
}

/// Start the cache purge background task
pub async fn start_purge_task(db: Arc<Database>, config: PurgeConfig) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(86400));


    loop {
        if let Err(e) = run_purge(&db, &config).await {
            error!("Cache purge failed: {}", e);
        }
        interval.tick().await;
    }
    // Purge runs forever; this code is unreachable.
    #[allow(unreachable_code)]
    Ok(())
}

/// Run a single cache purge cycle
async fn run_purge(db: &Database, config: &PurgeConfig) -> anyhow::Result<()> {
    info!("Starting cache purge cycle");

    let cutoff_timestamp = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::days(config.purge_after_days as i64))
        .unwrap_or_else(chrono::Utc::now)
        .timestamp();

    // Find old cache entries from database
    let old_entries = sqlx::query(
        "SELECT url, cache_path FROM cache_entries WHERE last_accessed < $1",
    )
    .bind(cutoff_timestamp)
    .fetch_all(db.pool())
    .await?;

    let mut purged_count = 0;
    let mut purged_bytes = 0u64;

    for row in old_entries {
        let url: String = row.get("url");
        let cache_path: String = row.get("cache_path");

        // Delete the file
        let path = PathBuf::from(&cache_path);
        if path.exists() {
            let metadata = tokio::fs::metadata(&path).await.ok();
            if let Err(e) = tokio::fs::remove_file(&path).await {
                warn!("Failed to delete cache file {}: {}", path.display(), e);
                continue;
            }
            if let Some(meta) = metadata {
                purged_bytes += meta.len();
            }
        }

        // Delete the meta file
        let meta_path = PathBuf::from(format!("{}.json", cache_path));
        let _ = tokio::fs::remove_file(&meta_path).await;

        // Delete from database
        if let Err(e) = sqlx::query("DELETE FROM cache_entries WHERE url = $1")
            .bind(&url)
            .execute(db.pool())
            .await
        {
            warn!("Failed to delete cache entry from database: {}", e);
        }

        purged_count += 1;
        debug!("Purged cache entry: {}", url);
    }

    // Check cache size limit
    if config.max_size_gb > 0 {
        let max_bytes = config.max_size_gb * 1024 * 1024 * 1024;
        let current_size = get_cache_size(&config.cache_dir).await;

        if current_size > max_bytes {
            info!(
                "Cache size {} GB exceeds limit {} GB, evicting oldest entries",
                current_size / (1024 * 1024 * 1024),
                config.max_size_gb
            );

            // Evict oldest entries until under limit
            let bytes_to_free = current_size - max_bytes;
            let evicted = evict_oldest_entries(db, bytes_to_free, &config.cache_dir).await?;
            purged_count += evicted.0;
            purged_bytes += evicted.1;
        }
    }

    info!(
        "Cache purge complete: {} entries purged, {} MB freed",
        purged_count,
        purged_bytes / (1024 * 1024)
    );

    Ok(())
}
/// Get total cache directory size
async fn get_cache_size(cache_dir: &PathBuf) -> u64 {
    let mut total_size = 0u64;

    if let Ok(mut entries) = tokio::fs::read_dir(cache_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
                if let Ok(metadata) = entry.metadata().await {
                    total_size += metadata.len();
                }
            }
        }
    }

    total_size
}
/// Evict oldest cache entries to free up space
async fn evict_oldest_entries(
    db: &Database,
    bytes_to_free: u64,
    _cache_dir: &PathBuf,
) -> anyhow::Result<(usize, u64)> {
    let old_entries = sqlx::query(
        "SELECT url, cache_path, content_length FROM cache_entries ORDER BY last_accessed ASC",
    )
    .fetch_all(db.pool())
    .await?;

    let mut freed_count = 0;
    let mut freed_bytes = 0u64;

    for row in old_entries {
        if freed_bytes >= bytes_to_free {
            break;
        }

        let url: String = row.get("url");
        let cache_path: String = row.get("cache_path");
        let content_length: i64 = row.get("content_length");

        // Delete the file
        let path = PathBuf::from(&cache_path);
        if path.exists() {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                warn!("Failed to delete cache file {}: {}", path.display(), e);
                continue;
            }
        }

        // Delete the meta file
        let meta_path = PathBuf::from(format!("{}.json", cache_path));
        let _ = tokio::fs::remove_file(&meta_path).await;

        // Delete from database
        if let Err(e) = sqlx::query("DELETE FROM cache_entries WHERE url = $1")
            .bind(&url)
            .execute(db.pool())
            .await
        {
            warn!("Failed to delete cache entry from database: {}", e);
        }

        freed_count += 1;
        freed_bytes += content_length as u64;
        debug!("Evicted cache entry: {}", url);
    }

    Ok((freed_count, freed_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_purge_config_default() {
        let config = PurgeConfig::default();
        assert_eq!(config.purge_after_days, 30);
        assert_eq!(config.max_size_gb, 0);
    }
}
