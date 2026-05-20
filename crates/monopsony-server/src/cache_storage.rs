//! Abstract cache storage backend.
//!
//! Defines the `CacheStorage` trait that decouples cache I/O from the filesystem.
//! Two implementations: `FilesystemStorage` (default, always available) and
//! `S3Storage` (future sprint, requires `object_store` crate).

use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{debug, warn};

/// Metadata returned after a successful write.
#[derive(Debug, Clone)]
pub struct StorageMeta {
    pub checksum_sha256: String,
    pub size_bytes: u64,
    pub etag: Option<String>,
    pub storage_class: Option<String>,
}

/// Abstraction over cache storage I/O.
///
/// The proxy, prefetch, and purge modules use this trait instead of direct
/// filesystem calls. This allows swapping in S3-compatible backends with
/// encryption and Object Lock without changing consumer code.
#[async_trait::async_trait]
pub trait CacheStorage: Send + Sync {
    /// Write data to storage under the given key. Returns metadata including checksum.
    async fn write(&self, key: &str, data: &[u8]) -> anyhow::Result<StorageMeta>;

    /// Read data from storage by key.
    async fn read(&self, key: &str) -> anyhow::Result<Vec<u8>>;

    /// Check if a key exists.
    async fn exists(&self, key: &str) -> anyhow::Result<bool>;

    /// Delete a key.
    async fn delete(&self, key: &str) -> anyhow::Result<()>;

    /// Get stored SHA256 checksum for tamper verification.
    async fn get_checksum(&self, key: &str) -> anyhow::Result<Option<String>>;

    /// Set immutability retention on a key until the given time.
    /// No-op on filesystem backend.
    async fn set_retention(&self, key: &str, until: SystemTime) -> anyhow::Result<()>;

    /// Get storage backend type identifier.
    fn backend_type(&self) -> &'static str;

    /// Get the total size of all stored data in bytes.
    async fn total_size(&self) -> u64;

    /// List all keys in storage (for stats/purge).
    async fn list_keys(&self) -> anyhow::Result<Vec<String>>;
}

// ---------------------------------------------------------------------------
// FilesystemStorage
// ---------------------------------------------------------------------------

/// Filesystem-backed cache storage.
///
/// Stores cache files and JSON metadata side-by-side on disk.
/// This is the default backend and preserves the exact behavior that existed
/// before the CacheStorage trait was introduced.
#[derive(Debug)]
pub struct FilesystemStorage {
    base_dir: PathBuf,
    meta_dir: PathBuf,
}

impl FilesystemStorage {
    pub fn new(base_dir: PathBuf) -> Self {
        let meta_dir = base_dir.join("meta");
        Self { base_dir, meta_dir }
    }

    /// The base cache directory (for consumers that need the raw path,
    /// e.g. path traversal validation).
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Convert a storage key to a filesystem path.
    pub fn key_to_path(&self, key: &str) -> PathBuf {
        self.base_dir.join(key)
    }

    /// Convert a filesystem path back to a storage key (relative to base_dir).
    pub fn path_to_key(&self, path: &Path) -> Option<String> {
        path.strip_prefix(&self.base_dir)
            .ok()
            .map(|rel| rel.to_string_lossy().to_string())
    }

    /// Path to the JSON metadata file for a given cache key.
    fn meta_path(&self, key: &str) -> PathBuf {
        let mut p = self.meta_dir.join(key);
        p.set_extension("json");
        p
    }

    /// Ensure parent directories exist for a given path.
    async fn ensure_parent(path: &Path) {
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                warn!("Failed to create directory {:?}: {}", parent, e);
            }
        }
    }
}

#[async_trait::async_trait]
impl CacheStorage for FilesystemStorage {
    async fn write(&self, key: &str, data: &[u8]) -> anyhow::Result<StorageMeta> {
        let path = self.key_to_path(key);
        Self::ensure_parent(&path).await;

        tokio::fs::write(&path, data).await?;

        // Compute SHA256 checksum
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(data));

        debug!("Wrote {} bytes to cache key {}", data.len(), key);

        Ok(StorageMeta {
            checksum_sha256: hash,
            size_bytes: data.len() as u64,
            etag: None,
            storage_class: None,
        })
    }

    async fn read(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let path = self.key_to_path(key);
        Ok(tokio::fs::read(&path).await?)
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        let path = self.key_to_path(key);
        Ok(path.exists())
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = self.key_to_path(key);
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        // Also delete metadata file
        let meta_path = self.meta_path(key);
        let _ = tokio::fs::remove_file(&meta_path).await;
        Ok(())
    }

    async fn get_checksum(&self, key: &str) -> anyhow::Result<Option<String>> {
        // FilesystemStorage doesn't store checksums separately.
        // Future: compute on read or store in meta JSON.
        let _ = key;
        Ok(None)
    }

    async fn set_retention(&self, _key: &str, _until: SystemTime) -> anyhow::Result<()> {
        // No-op: filesystem has no Object Lock equivalent.
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        "filesystem"
    }

    async fn total_size(&self) -> u64 {
        let mut total = 0u64;
        if let Ok(mut entries) = tokio::fs::read_dir(&self.base_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
                    if let Ok(meta) = entry.metadata().await {
                        total += meta.len();
                    }
                }
            }
        }
        total
    }

    async fn list_keys(&self) -> anyhow::Result<Vec<String>> {
        let mut keys = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.base_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_file() {
                    if let Some(key) = self.path_to_key(&path) {
                        keys.push(key);
                    }
                }
            }
        }
        Ok(keys)
    }
}

// ---------------------------------------------------------------------------
// Metadata helpers (used by proxy.rs for CacheEntry JSON)
// ---------------------------------------------------------------------------

impl FilesystemStorage {
    /// Load a JSON metadata file for a cache key.
    pub async fn load_meta_json(&self, key: &str) -> Option<Vec<u8>> {
        let meta_path = self.meta_path(key);
        tokio::fs::read(&meta_path).await.ok()
    }

    /// Save a JSON metadata file for a cache key.
    pub async fn save_meta_json(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        let meta_path = self.meta_path(key);
        Self::ensure_parent(&meta_path).await;
        tokio::fs::write(&meta_path, data).await?;
        Ok(())
    }

    /// Initialize directory structure.
    pub async fn init_dirs(&self) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.base_dir).await?;
        tokio::fs::create_dir_all(&self.meta_dir).await?;
        tokio::fs::create_dir_all(self.base_dir.join("other")).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_filesystem_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path().to_path_buf());

        let data = b"hello cache";
        let meta = storage.write("test/key.deb", data).await.unwrap();

        assert_eq!(meta.size_bytes, 11);
        assert_eq!(meta.checksum_sha256.len(), 64);

        let read_data = storage.read("test/key.deb").await.unwrap();
        assert_eq!(read_data, data);
    }

    #[tokio::test]
    async fn test_filesystem_exists() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path().to_path_buf());

        assert!(!storage.exists("missing").await.unwrap());
        storage.write("present", b"data").await.unwrap();
        assert!(storage.exists("present").await.unwrap());
    }

    #[tokio::test]
    async fn test_filesystem_delete() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path().to_path_buf());

        storage.write("to-delete", b"data").await.unwrap();
        assert!(storage.exists("to-delete").await.unwrap());

        storage.delete("to-delete").await.unwrap();
        assert!(!storage.exists("to-delete").await.unwrap());
    }

    #[tokio::test]
    async fn test_filesystem_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path().to_path_buf());

        let meta = storage.write("hash-test", b"known data").await.unwrap();
        // SHA256 of "known data"
        use sha2::Digest;
        let expected = hex::encode(sha2::Sha256::digest(b"known data"));
        assert_eq!(meta.checksum_sha256, expected);
    }

    #[tokio::test]
    async fn test_filesystem_retention_noop() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path().to_path_buf());
        // Should not error
        storage
            .set_retention("key", SystemTime::now())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_filesystem_backend_type() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path().to_path_buf());
        assert_eq!(storage.backend_type(), "filesystem");
    }

    #[tokio::test]
    async fn test_filesystem_meta_json_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path().to_path_buf());

        let json = br#"{"url":"http://example.com/pkg.deb"}"#;
        storage.save_meta_json("pkg.deb", json).await.unwrap();

        let loaded = storage.load_meta_json("pkg.deb").await.unwrap();
        assert_eq!(loaded, json);
    }

    #[tokio::test]
    async fn test_key_to_path_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path().to_path_buf());

        let key = "apt/archive.ubuntu.com/pool/main/l/linux/linux-image.deb";
        let path = storage.key_to_path(key);
        assert!(path.starts_with(dir.path()));
        assert!(path.to_string_lossy().contains("apt/archive.ubuntu.com"));

        let back = storage.path_to_key(&path).unwrap();
        assert_eq!(back, key);
    }

    #[tokio::test]
    async fn test_init_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("cache");
        let storage = FilesystemStorage::new(sub.clone());
        storage.init_dirs().await.unwrap();

        assert!(sub.exists());
        assert!(sub.join("meta").exists());
        assert!(sub.join("other").exists());
    }
}
