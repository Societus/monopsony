//! Unified object store cache backend.
//!
//! A single `ObjectStoreBackend` wraps any `object_store::ObjectStore` implementation,
//! providing identical key schemas and sidecar metadata for both local filesystem
//! and S3-compatible storage. This enables seamless cache migration between backends
//! (e.g. `rclone sync /cache/ s3:bucket/`) without redownloading packages.
//!
//! Backends:
//! - **Local**: `object_store::local::LocalFileSystem` — files on disk, identical layout
//! - **S3**: `object_store::aws::AmazonS3` — any S3-compatible endpoint

use std::path::PathBuf;
use std::time::SystemTime;

use async_trait::async_trait;
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use object_store::WriteMultipart;
use sha2::Digest;
use tracing::{debug, warn};

use crate::cache_storage::{ByteRange, ByteStream, CacheStorage, SpoolHandle, StorageMeta};

// ---------------------------------------------------------------------------
// ObjectStoreBackend
// ---------------------------------------------------------------------------

/// Unified object store cache backend.
///
/// Wraps any `dyn ObjectStore` (local filesystem, S3, in-memory for tests)
/// with a consistent sidecar metadata pattern. Keys are flat strings;
/// metadata lives under `.meta/<key>`.
pub struct ObjectStoreBackend {
    store: Box<dyn ObjectStore>,
    /// Human-readable label for logging ("local", "s3", "memory").
    label: &'static str,
    /// Local filesystem root path (only set for local backend).
    local_root: Option<PathBuf>,
    /// Default retention period in days (0 = no retention).
    retention_days: u32,
}

/// Internal metadata stored as a sidecar object alongside data.
///
/// Sprint 23 fields use `#[serde(default)]` so pre-Sprint-23 sidecars
/// (missing the keys) deserialize as `Distro` / never-verified — backward
/// compatible, and unverified blobs are exactly the ones the scrubber
/// should reach first.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ObjectMeta {
    checksum_sha256: String,
    size_bytes: u64,
    #[serde(default)]
    cache_class: monopsony_common::config::CacheClass,
    /// Epoch secs of the last scrubber re-hash (0 = never verified).
    #[serde(default)]
    last_verified_at: i64,
}

impl ObjectStoreBackend {
    /// Create a backend wrapping any `ObjectStore` implementation.
    pub fn new(
        store: Box<dyn ObjectStore>,
        label: &'static str,
        local_root: Option<PathBuf>,
        retention_days: u32,
    ) -> Self {
        Self {
            store,
            label,
            local_root,
            retention_days,
        }
    }

    /// Create a local filesystem backend at the given path.
    ///
    /// Uses `object_store::local::LocalFileSystem` which stores objects as files
    /// using the key as a relative path from the root. This produces an identical
    /// on-disk layout to the previous `FilesystemStorage`.
    pub fn local(root: PathBuf, retention_days: u32) -> anyhow::Result<Self> {
        let store = object_store::local::LocalFileSystem::new_with_prefix(&root)?;
        Ok(Self::new(
            Box::new(store),
            "local",
            Some(root),
            retention_days,
        ))
    }

    /// Create an S3-compatible backend from config.
    ///
    /// Supports environment variable fallback for credentials:
    /// `MONOPSONY_S3_ACCESS_KEY`, `MONOPSONY_S3_SECRET_KEY`
    pub fn s3(config: &monopsony_common::config::S3Config) -> anyhow::Result<Self> {
        use object_store::aws::AmazonS3Builder;

        let access_key = if config.access_key.is_empty() {
            std::env::var("MONOPSONY_S3_ACCESS_KEY").unwrap_or_default()
        } else {
            config.access_key.clone()
        };

        let secret_key = if config.secret_key.is_empty() {
            std::env::var("MONOPSONY_S3_SECRET_KEY").unwrap_or_default()
        } else {
            config.secret_key.clone()
        };

        let mut builder = AmazonS3Builder::new()
            .with_endpoint(&config.endpoint)
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&access_key)
            .with_secret_access_key(&secret_key)
            .with_allow_http(true); // Allow HTTP for local SeaweedFS/Garage

        if config.force_path_style {
            builder = builder.with_virtual_hosted_style_request(false);
        }

        let store = builder.build()?;

        Ok(Self::new(
            Box::new(store),
            "s3",
            None,
            config.retention_days,
        ))
    }

    /// Create an in-memory backend for testing.
    #[cfg(test)]
    pub fn in_memory(label: &'static str) -> Self {
        let store = object_store::memory::InMemory::new();
        Self::new(Box::new(store), label, None, 0)
    }

    // -- Internal helpers --

    fn obj_path(key: &str) -> ObjectPath {
        ObjectPath::from(key)
    }

    fn meta_obj_path(key: &str) -> ObjectPath {
        ObjectPath::from(format!(".meta/{}", key))
    }

    async fn save_sidecar(&self, key: &str, meta: &ObjectMeta) -> anyhow::Result<()> {
        let path = Self::meta_obj_path(key);
        let json = serde_json::to_vec(meta)?;
        self.store.put(&path, json.into()).await?;
        Ok(())
    }

    async fn load_sidecar(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
        let path = Self::meta_obj_path(key);
        match self.store.get(&path).await {
            Ok(result) => {
                let bytes = result.bytes().await?;
                let meta: ObjectMeta = serde_json::from_slice(&bytes)?;
                Ok(Some(meta))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get the local filesystem root, if this is a local backend.
    pub fn local_root(&self) -> Option<&std::path::Path> {
        self.local_root.as_deref()
    }
}

impl std::fmt::Debug for ObjectStoreBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStoreBackend")
            .field("label", &self.label)
            .field("local_root", &self.local_root)
            .field("retention_days", &self.retention_days)
            .finish()
    }
}

#[async_trait]
impl CacheStorage for ObjectStoreBackend {
    async fn spool(&self, key: &str) -> anyhow::Result<SpoolHandle> {
        let path = Self::obj_path(key);
        let upload = self.store.put_multipart(&path).await?;
        Ok(SpoolHandle::new(key, WriteMultipart::new(upload)))
    }

    async fn read_stream(&self, key: &str, range: ByteRange) -> anyhow::Result<(ByteStream, u64)> {
        use futures::StreamExt;

        let path = Self::obj_path(key);
        // Head first for total size (returns NotFound early).
        let meta = self.store.head(&path).await?;
        let total = meta.size;

        if range.start >= total {
            anyhow::bail!("range start {} beyond object size {}", range.start, total);
        }
        let end = range.end.min(total);
        let get_opts = object_store::GetOptions {
            range: Some(object_store::GetRange::Bounded(range.start..end)),
            ..Default::default()
        };
        let result = self.store.get_opts(&path, get_opts).await?;
        let stream = result.into_stream().map(|r| r.map_err(|e| e.into()));
        Ok((stream.boxed(), total))
    }

    async fn write(&self, key: &str, data: &[u8]) -> anyhow::Result<StorageMeta> {
        let path = Self::obj_path(key);

        // Compute SHA256 checksum
        let hash = hex::encode(sha2::Sha256::digest(data));

        // Write data object
        let put_result = self.store.put(&path, data.to_vec().into()).await?;

        // Write metadata sidecar
        let sidecar = ObjectMeta {
            checksum_sha256: hash.clone(),
            size_bytes: data.len() as u64,
            cache_class: monopsony_common::config::CacheClass::Distro,
            last_verified_at: 0,
        };
        self.save_sidecar(key, &sidecar).await?;

        debug!("Wrote {} bytes to {} key {}", data.len(), self.label, key);

        Ok(StorageMeta {
            checksum_sha256: hash,
            size_bytes: data.len() as u64,
            etag: put_result.e_tag,
            storage_class: None,
            cache_class: monopsony_common::config::CacheClass::Distro,
        })
    }

    async fn read(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let path = Self::obj_path(key);
        let result = self.store.get(&path).await?;
        let bytes = result.bytes().await?;
        Ok(bytes.to_vec())
    }

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        let path = Self::obj_path(key);
        match self.store.head(&path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = Self::obj_path(key);
        self.store.delete(&path).await?;

        // Also delete sidecar metadata
        let meta_path = Self::meta_obj_path(key);
        let _ = self.store.delete(&meta_path).await; // ignore NotFound

        debug!("Deleted {} key {}", self.label, key);
        Ok(())
    }

    async fn get_checksum(&self, key: &str) -> anyhow::Result<Option<String>> {
        let meta = self.load_sidecar(key).await?;
        Ok(meta.map(|m| m.checksum_sha256))
    }

    async fn record_spool_meta(&self, key: &str, meta: &StorageMeta) -> anyhow::Result<()> {
        // Overwrite semantics match `write`: new bytes = never verified
        // (last_verified_at resets so the scrubber reaches them first).
        let sidecar = ObjectMeta {
            checksum_sha256: meta.checksum_sha256.clone(),
            size_bytes: meta.size_bytes,
            cache_class: meta.cache_class.clone(),
            last_verified_at: 0,
        };
        self.save_sidecar(key, &sidecar).await
    }

    async fn set_retention(&self, key: &str, until: SystemTime) -> anyhow::Result<()> {
        // Object Lock retention is set via S3 API extension.
        // The object_store crate doesn't directly support PutObjectRetention.
        // This is a placeholder that records the intent.
        debug!(
            "Retention requested for {} key {} until {:?} — API not yet wired",
            self.label, key, until
        );
        Ok(())
    }

    fn backend_type(&self) -> &'static str {
        self.label
    }

    async fn total_size(&self) -> u64 {
        let mut total = 0u64;
        let object_stream = self.store.list(None);
        use futures::StreamExt;
        let mut stream = Box::pin(object_stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(meta) => {
                    // Skip metadata sidecar objects
                    if !meta.location.as_ref().starts_with(".meta/") {
                        total += meta.size;
                    }
                }
                Err(e) => {
                    warn!("Error listing {} objects: {}", self.label, e);
                }
            }
        }
        total
    }

    async fn list_keys(&self) -> anyhow::Result<Vec<String>> {
        let mut keys = Vec::new();
        let object_stream = self.store.list(None);
        use futures::StreamExt;
        let mut stream = Box::pin(object_stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(meta) => {
                    let key = meta.location.to_string();
                    // Skip metadata sidecar objects
                    if !key.starts_with(".meta/") {
                        keys.push(key);
                    }
                }
                Err(e) => {
                    warn!("Error listing {} objects: {}", self.label, e);
                }
            }
        }
        Ok(keys)
    }

    async fn load_meta_json(&self, key: &str) -> Option<Vec<u8>> {
        // The CacheEntry JSON metadata stored by proxy.rs.
        // Stored under .cache-meta/<key> to distinguish from our internal .meta/ sidecars.
        let path = ObjectPath::from(format!(".cache-meta/{}", key));
        match self.store.get(&path).await {
            Ok(result) => result.bytes().await.ok().map(|b| b.to_vec()),
            Err(_) => None,
        }
    }

    async fn save_meta_json(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        let path = ObjectPath::from(format!(".cache-meta/{}", key));
        self.store.put(&path, data.to_vec().into()).await?;
        Ok(())
    }

    async fn init_dirs(&self) -> anyhow::Result<()> {
        // For local backend, object_store::local::LocalFileSystem creates
        // parent directories on write. No explicit init needed.
        Ok(())
    }

    fn base_dir(&self) -> Option<PathBuf> {
        self.local_root.clone()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> ObjectStoreBackend {
        ObjectStoreBackend::in_memory("memory")
    }

    #[tokio::test]
    async fn test_write_read_roundtrip() {
        let storage = make_store();
        let data = b"hello cache";

        let meta = storage.write("test/key.deb", data).await.unwrap();
        assert_eq!(meta.size_bytes, 11);
        assert_eq!(meta.checksum_sha256.len(), 64);

        let read_data = storage.read("test/key.deb").await.unwrap();
        assert_eq!(read_data, data);
    }

    #[tokio::test]
    async fn test_write_returns_etag() {
        let storage = make_store();
        let meta = storage.write("key", b"data").await.unwrap();
        assert!(meta.etag.is_some());
    }

    #[tokio::test]
    async fn test_exists() {
        let storage = make_store();
        assert!(!storage.exists("missing").await.unwrap());
        storage.write("present", b"data").await.unwrap();
        assert!(storage.exists("present").await.unwrap());
    }

    #[tokio::test]
    async fn test_delete() {
        let storage = make_store();
        storage.write("to-delete", b"data").await.unwrap();
        assert!(storage.exists("to-delete").await.unwrap());

        storage.delete("to-delete").await.unwrap();
        assert!(!storage.exists("to-delete").await.unwrap());
    }

    #[tokio::test]
    async fn test_delete_cleans_sidecar() {
        let storage = make_store();
        storage.write("key", b"data").await.unwrap();

        let checksum_before = storage.get_checksum("key").await.unwrap();
        assert!(checksum_before.is_some());

        storage.delete("key").await.unwrap();

        let checksum_after = storage.get_checksum("key").await.unwrap();
        assert!(checksum_after.is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_ok() {
        let storage = make_store();
        let _ = storage.delete("ghost").await;
    }

    #[tokio::test]
    async fn test_checksum_stored_and_retrieved() {
        let storage = make_store();
        let data = b"known data for checksum";
        let meta = storage.write("checksum-test", data).await.unwrap();

        let stored = storage.get_checksum("checksum-test").await.unwrap();
        let expected = hex::encode(sha2::Sha256::digest(data));
        assert_eq!(stored, Some(expected));
        assert_eq!(stored, Some(meta.checksum_sha256));
    }

    #[tokio::test]
    async fn test_checksum_missing_key() {
        let storage = make_store();
        let result = storage.get_checksum("nonexistent").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_retention_noop() {
        let storage = make_store();
        storage
            .set_retention("key", SystemTime::now())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_backend_type() {
        let storage = make_store();
        assert_eq!(storage.backend_type(), "memory");
    }

    #[tokio::test]
    async fn test_list_keys() {
        let storage = make_store();
        storage.write("key1", b"a").await.unwrap();
        storage.write("key2", b"b").await.unwrap();
        storage.write("key3", b"c").await.unwrap();

        let keys = storage.list_keys().await.unwrap();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"key1".to_string()));
        assert!(keys.contains(&"key2".to_string()));
        assert!(keys.contains(&"key3".to_string()));
    }

    #[tokio::test]
    async fn test_list_keys_excludes_meta() {
        let storage = make_store();
        storage.write("data-key", b"data").await.unwrap();

        let keys = storage.list_keys().await.unwrap();
        assert!(!keys.iter().any(|k| k.starts_with(".meta/")));
    }

    #[tokio::test]
    async fn test_total_size() {
        let storage = make_store();
        storage.write("small", b"12345").await.unwrap();
        storage
            .write("large", b"12345678901234567890")
            .await
            .unwrap();

        let size = storage.total_size().await;
        assert_eq!(size, 25);
    }

    #[tokio::test]
    async fn test_overwrite_key() {
        let storage = make_store();
        storage.write("key", b"original").await.unwrap();
        storage.write("key", b"replacement").await.unwrap();

        let data = storage.read("key").await.unwrap();
        assert_eq!(data, b"replacement");
    }

    #[tokio::test]
    async fn test_meta_json_roundtrip() {
        let storage = make_store();

        let json = br#"{"url":"http://example.com/pkg.deb"}"#;
        storage.save_meta_json("pkg.deb", json).await.unwrap();

        let loaded = storage.load_meta_json("pkg.deb").await.unwrap();
        assert_eq!(loaded, json);
    }

    #[tokio::test]
    async fn test_meta_json_separate_from_sidecar() {
        // CacheEntry JSON (.cache-meta/) is distinct from checksum sidecar (.meta/)
        let storage = make_store();
        storage.write("key", b"data").await.unwrap();

        let cache_meta = br#"{"url":"http://example.com/pkg.deb"}"#;
        storage.save_meta_json("key", cache_meta).await.unwrap();

        // Both should coexist
        let checksum = storage.get_checksum("key").await.unwrap();
        assert!(checksum.is_some());

        let loaded = storage.load_meta_json("key").await.unwrap();
        assert_eq!(loaded, cache_meta);
    }

    #[tokio::test]
    async fn test_init_dirs_noop() {
        let storage = make_store();
        storage.init_dirs().await.unwrap();
    }

    #[tokio::test]
    async fn test_base_dir_none_for_memory() {
        let storage = make_store();
        assert!(storage.base_dir().is_none());
    }

    #[tokio::test]
    async fn test_debug_format() {
        let storage = make_store();
        let debug_str = format!("{:?}", storage);
        assert!(debug_str.contains("memory"));
        assert!(debug_str.contains("ObjectStoreBackend"));
    }

    #[tokio::test]
    async fn test_local_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = ObjectStoreBackend::local(tmp.path().to_path_buf(), 0).unwrap();

        // base_dir should point to the temp dir
        assert_eq!(storage.base_dir().unwrap(), tmp.path());

        // Write
        let meta = storage
            .write("test/local-key.deb", b"local data")
            .await
            .unwrap();
        assert_eq!(meta.size_bytes, 10);

        // Read back
        let buf = storage.read("test/local-key.deb").await.unwrap();
        assert_eq!(buf.as_slice(), b"local data");

        // Exists
        assert!(storage.exists("test/local-key.deb").await.unwrap());
        assert!(!storage.exists("test/nonexistent.deb").await.unwrap());

        // Checksum
        let cs = storage
            .get_checksum("test/local-key.deb")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cs.len(), 64);

        // Delete
        storage.delete("test/local-key.deb").await.unwrap();
        assert!(!storage.exists("test/local-key.deb").await.unwrap());
    }

    #[test]
    fn test_s3_constructor_compiles() {
        // Verifies the s3() constructor compiles. No live S3 connection.
        let config = monopsony_common::config::S3Config {
            endpoint: "http://localhost:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "test-bucket".to_string(),
            access_key: "minioadmin".to_string(),
            secret_key: "minioadmin".to_string(),
            encryption_key_path: None,
            retention_days: 7,
            force_path_style: true,
        };
        // Constructor should succeed (only builds the client, doesn't connect)
        let result = ObjectStoreBackend::s3(&config);
        assert!(
            result.is_ok(),
            "s3() constructor failed: {:?}",
            result.err()
        );
    }
}
