//! Content-addressed blob store for shadow package assets.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetIndexEntry {
    pub blob_hash: String,
    pub asset_name: String,
    pub size: u64,
    pub indexed_at: i64,
}

#[derive(Debug)]
pub enum CacheError {
    Io(std::io::Error),
    DiskBudgetExceeded,
    HashMismatch { expected: String, got: String },
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::DiskBudgetExceeded => write!(f, "disk budget exceeded"),
            Self::HashMismatch { expected, got } => {
                write!(f, "hash mismatch: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for CacheError {}

pub struct ShadowCache {
    base: PathBuf,
}

impl ShadowCache {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    /// Ensure blob directory structure exists
    async fn ensure_dirs(&self) -> Result<(), CacheError> {
        fs::create_dir_all(self.base.join("blobs"))
            .await
            .map_err(CacheError::Io)?;
        fs::create_dir_all(self.base.join("index"))
            .await
            .map_err(CacheError::Io)?;
        Ok(())
    }

    /// Blob file path: blobs/<first2>/<rest38>
    fn blob_path(&self, hash: &str) -> PathBuf {
        self.base.join("blobs").join(&hash[..2]).join(&hash[2..])
    }

    /// Store raw bytes, return SHA-256 hex hash (blob ID)
    pub async fn store_blob(&self, data: &[u8]) -> Result<String, CacheError> {
        self.ensure_dirs().await?;
        let hash = hex::encode(Sha256::digest(data));
        let path = self.blob_path(&hash);

        // Idempotent: if already stored, just return hash
        if path.exists() {
            debug!("Blob {hash} already cached");
            return Ok(hash);
        }

        // Create parent dir
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.map_err(CacheError::Io)?;
        }

        fs::write(&path, data).await.map_err(CacheError::Io)?;
        debug!("Stored blob {hash} ({} bytes)", data.len());
        Ok(hash)
    }

    /// Read a blob by its SHA-256 hash
    pub async fn read_blob(&self, hash: &str) -> Result<Vec<u8>, CacheError> {
        let path = self.blob_path(hash);
        fs::read(&path).await.map_err(CacheError::Io)
    }

    /// Check if a blob exists
    pub async fn blob_exists(&self, hash: &str) -> bool {
        self.blob_path(hash).exists()
    }

    /// Index path: index/<package>/<version>/<platform>.json
    fn index_path(&self, package: &str, version: &str, platform: &str) -> PathBuf {
        self.base
            .join("index")
            .join(package)
            .join(version)
            .join(format!("{platform}.json"))
    }

    /// Create a platform index entry: (package, version, platform) -> blob hash
    pub async fn index_asset(
        &self,
        package: &str,
        version: &str,
        platform: &str,
        blob_hash: &str,
        asset_name: &str,
        size: u64,
    ) -> Result<(), CacheError> {
        let path = self.index_path(package, version, platform);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.map_err(CacheError::Io)?;
        }
        let entry = AssetIndexEntry {
            blob_hash: blob_hash.to_string(),
            asset_name: asset_name.to_string(),
            size,
            indexed_at: chrono::Utc::now().timestamp(),
        };
        let json = serde_json::to_string_pretty(&entry)
            .map_err(|e| CacheError::Io(std::io::Error::other(e)))?;
        fs::write(&path, json).await.map_err(CacheError::Io)?;
        debug!("Indexed {package}/{version}/{platform} -> {blob_hash}");
        Ok(())
    }

    /// Look up the blob hash for a platform-indexed asset
    pub async fn lookup_asset(
        &self,
        package: &str,
        version: &str,
        platform: &str,
    ) -> Option<AssetIndexEntry> {
        let path = self.index_path(package, version, platform);
        let data = fs::read_to_string(&path).await.ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Garbage collect: remove blobs with zero references, respecting disk budget.
    /// Returns number of bytes freed.
    pub async fn gc(&self, _disk_budget_pct: f64) -> Result<u64, CacheError> {
        self.ensure_dirs().await?;

        // Collect all indexed blob hashes
        let blobs_dir = self.base.join("blobs");
        let index_dir = self.base.join("index");

        let mut referenced_hashes = std::collections::HashSet::new();
        if index_dir.exists() {
            self.collect_indexed_hashes(&index_dir, &mut referenced_hashes)
                .await?;
        }

        // Find and remove orphaned blobs
        let mut freed: u64 = 0;
        if blobs_dir.exists() {
            let mut prefix_dirs = fs::read_dir(&blobs_dir).await.map_err(CacheError::Io)?;
            while let Some(entry) = prefix_dirs.next_entry().await.map_err(CacheError::Io)? {
                if entry.file_type().await.map_err(CacheError::Io)?.is_dir() {
                    let mut blob_files =
                        fs::read_dir(entry.path()).await.map_err(CacheError::Io)?;
                    while let Some(blob_entry) =
                        blob_files.next_entry().await.map_err(CacheError::Io)?
                    {
                        let name = blob_entry.file_name();
                        let name_str = name.to_string_lossy().into_owned();
                        let prefix_name = entry.file_name();
                        let prefix_str = prefix_name.to_string_lossy().into_owned();
                        let full_hash = format!("{prefix_str}{name_str}");

                        if !referenced_hashes.contains(&full_hash) {
                            let meta = blob_entry.metadata().await.map_err(CacheError::Io)?;
                            freed += meta.len();
                            fs::remove_file(blob_entry.path())
                                .await
                                .map_err(CacheError::Io)?;
                            debug!("GC removed orphan blob: {full_hash}");
                        }
                    }
                }
            }
        }

        Ok(freed)
    }

    /// Recursively collect all blob hashes referenced by index files
    async fn collect_indexed_hashes(
        &self,
        dir: &Path,
        hashes: &mut std::collections::HashSet<String>,
    ) -> Result<(), CacheError> {
        let mut entries = fs::read_dir(dir).await.map_err(CacheError::Io)?;
        while let Some(entry) = entries.next_entry().await.map_err(CacheError::Io)? {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(data) = fs::read_to_string(&path).await {
                    if let Ok(idx) = serde_json::from_str::<AssetIndexEntry>(&data) {
                        hashes.insert(idx.blob_hash);
                    }
                }
            } else if entry.file_type().await.map_err(CacheError::Io)?.is_dir() {
                Box::pin(self.collect_indexed_hashes(&path, hashes)).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_cache() -> (tempfile::TempDir, ShadowCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = ShadowCache::new(dir.path().to_path_buf());
        cache.ensure_dirs().await.unwrap();
        (dir, cache)
    }

    #[tokio::test]
    async fn store_and_read_blob() {
        let (_dir, cache) = setup_cache().await;
        let data = b"hello world";
        let hash = cache.store_blob(data).await.unwrap();
        assert_eq!(hash.len(), 64);
        let read = cache.read_blob(&hash).await.unwrap();
        assert_eq!(read, data);
    }

    #[tokio::test]
    async fn store_blob_idempotent() {
        let (_dir, cache) = setup_cache().await;
        let hash1 = cache.store_blob(b"data").await.unwrap();
        let hash2 = cache.store_blob(b"data").await.unwrap();
        assert_eq!(hash1, hash2);
    }

    #[tokio::test]
    async fn read_nonexistent_blob() {
        let (_dir, cache) = setup_cache().await;
        let result = cache.read_blob("abcdef1234567890".repeat(4).as_str()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn index_and_lookup_asset() {
        let (_dir, cache) = setup_cache().await;
        let hash = cache.store_blob(b"asset-data").await.unwrap();
        cache
            .index_asset(
                "sonarr",
                "4.0.7",
                "linux-x86_64-glibc",
                &hash,
                "sonarr.tar.gz",
                1024,
            )
            .await
            .unwrap();
        let entry = cache
            .lookup_asset("sonarr", "4.0.7", "linux-x86_64-glibc")
            .await
            .unwrap();
        assert_eq!(entry.blob_hash, hash);
        assert_eq!(entry.asset_name, "sonarr.tar.gz");
        assert_eq!(entry.size, 1024);
    }

    #[tokio::test]
    async fn gc_removes_orphans() {
        let (_dir, cache) = setup_cache().await;
        let referenced = cache.store_blob(b"keep-me").await.unwrap();
        let orphan = cache.store_blob(b"delete-me").await.unwrap();

        // Index only one
        cache
            .index_asset("pkg", "1.0", "linux-x86_64", &referenced, "pkg.tar.gz", 7)
            .await
            .unwrap();

        let freed = cache.gc(1.0).await.unwrap();
        assert!(freed > 0);

        // Referenced still exists
        assert!(cache.read_blob(&referenced).await.is_ok());
        // Orphan gone
        assert!(cache.read_blob(&orphan).await.is_err());
    }
}
