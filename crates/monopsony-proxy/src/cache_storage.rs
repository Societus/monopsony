//! Abstract cache storage backend.
//!
//! Defines the `CacheStorage` trait that decouples cache I/O from any single
//! implementation. The single production backend is `ObjectStoreBackend`
//! (object_store crate: local filesystem or S3), which implements:
//!
//! - **Streaming writes** (`spool`) — atomic commit, O(1) memory for GB-scale objects
//! - **Range reads** (`read_range`) — serves partial content / resume requests
//! - **Metadata sidecars** — `.cache-meta/<key>` JSON per entry
//!
//! Legacy whole-blob `write`/`read` remain for small objects (docker blobs,
//! prefetch metadata) where simplicity beats streaming.

use std::path::PathBuf;
use std::time::SystemTime;

use object_store::WriteMultipart;
use sha2::Digest;

/// Metadata returned after a successful write.
#[derive(Debug, Clone)]
pub struct StorageMeta {
    pub checksum_sha256: String,
    pub size_bytes: u64,
    pub etag: Option<String>,
    pub storage_class: Option<String>,
}

impl StorageMeta {
    /// Fake meta for tests / no-op storage.
    #[cfg(test)]
    pub fn synthetic(size: u64) -> Self {
        Self {
            checksum_sha256: String::new(),
            size_bytes: size,
            etag: None,
            storage_class: None,
        }
    }
}

/// Byte range for partial reads (`start..end`, end exclusive).
pub type ByteRange = std::ops::Range<u64>;

/// A streaming body read from storage. Yields `Bytes` chunks; total object
/// size is returned alongside for Content-Length headers.
pub type ByteStream = futures::stream::BoxStream<'static, anyhow::Result<bytes::Bytes>>;

/// Streaming spool for writing large objects incrementally with atomic commit.
///
/// Created by [`CacheStorage::spool`]. Feed chunks with [`SpoolHandle::write`],
/// then [`SpoolHandle::commit`] to publish atomically or [`SpoolHandle::abort`]
/// to discard. Nothing is visible to readers until commit succeeds.
///
/// Memory is O(chunk size): data flows upstream-chunk → spool buffer → store.
/// The spooled object becomes visible atomically at commit — partial objects
/// are never observable, so readers never need retry/repair logic.
pub struct SpoolHandle {
    key: String,
    inner: Option<WriteMultipart>,
    hasher: sha2::Sha256,
    size: u64,
}

impl SpoolHandle {
    pub(crate) fn new(key: impl Into<String>, inner: WriteMultipart) -> Self {
        Self {
            key: key.into(),
            inner: Some(inner),
            hasher: sha2::Sha256::new(),
            size: 0,
        }
    }

    /// Append a chunk. Data is invisible to readers until commit.
    ///
    /// Buffering: `WriteMultipart` flushes parts of ≥5 MiB; smaller writes
    /// accumulate in its internal buffer (≤5 MiB per in-flight spool).
    pub fn write(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.size += chunk.len() as u64;
        if let Some(inner) = self.inner.as_mut() {
            inner.write(chunk);
        }
    }

    /// Bytes written so far.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The cache key this spool will publish to.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Publish the object atomically. Returns final size and SHA-256.
    pub async fn commit(mut self) -> anyhow::Result<StorageMeta> {
        let inner = self
            .inner
            .take()
            .ok_or_else(|| anyhow::anyhow!("spool already finished"))?;
        let put = inner.finish().await?;
        Ok(StorageMeta {
            checksum_sha256: format!("{:x}", self.hasher.finalize()),
            size_bytes: self.size,
            etag: put.e_tag,
            storage_class: None,
        })
    }

    /// Discard the spooled object. Consumes self; idempotent cleanup.
    pub async fn abort(mut self) {
        if let Some(inner) = self.inner.take() {
            if let Err(e) = inner.abort().await {
                tracing::debug!("Spool abort cleanup failed (janitor will sweep): {}", e);
            }
        }
    }
}

/// Abstraction over cache storage I/O.
///
/// The proxy, prefetch, and purge modules use this trait instead of direct
/// filesystem calls. This allows swapping in S3-compatible backends with
/// encryption and Object Lock without changing consumer code.
#[async_trait::async_trait]
pub trait CacheStorage: Send + Sync {
    /// Begin a streaming spool for `key`. The object appears atomically on
    /// [`SpoolHandle::commit`]. Use for large or unknown-size bodies.
    async fn spool(&self, key: &str) -> anyhow::Result<SpoolHandle>;

    /// Open a streaming read of `range` bytes (end clamped to object size).
    /// Returns the stream and total object size. `range.end == u64::MAX`
    /// means "to EOF".
    async fn read_stream(&self, key: &str, range: ByteRange) -> anyhow::Result<(ByteStream, u64)>;

    /// Write data to storage under the given key. Returns metadata including checksum.
    /// For small objects; use [`CacheStorage::spool`] for large/unknown sizes.
    async fn write(&self, key: &str, data: &[u8]) -> anyhow::Result<StorageMeta>;

    /// Read data from storage by key. Whole-object read; use `read_range` for large objects.
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

    /// Load JSON metadata sidecar for a key (backend-specific, may be no-op).
    async fn load_meta_json(&self, key: &str) -> Option<Vec<u8>> {
        let _ = key;
        None
    }

    /// Save JSON metadata sidecar for a key (backend-specific, may be no-op).
    async fn save_meta_json(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        let _ = (key, data);
        Ok(())
    }

    /// Initialize storage directories (filesystem only).
    async fn init_dirs(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Get filesystem base dir if applicable (owned copy).
    fn base_dir(&self) -> Option<PathBuf> {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests — trait conformance against the production ObjectStoreBackend
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store_backend::ObjectStoreBackend;
    use futures::StreamExt;
    use std::sync::Arc;

    async fn storage() -> Arc<dyn CacheStorage> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(ObjectStoreBackend::local(dir.path().to_path_buf(), 0).unwrap())
    }

    #[tokio::test]
    async fn test_spool_commit_visible_atomically() {
        let storage = storage().await;
        let mut spool = storage.spool("spool/key.deb").await.unwrap();
        assert!(!storage.exists("spool/key.deb").await.unwrap());

        spool.write(b"hello ");
        spool.write(b"world");
        // Still invisible mid-spool
        assert!(!storage.exists("spool/key.deb").await.unwrap());

        let meta = spool.commit().await.unwrap();
        assert_eq!(meta.size_bytes, 11);
        assert_eq!(meta.checksum_sha256.len(), 64);

        assert!(storage.exists("spool/key.deb").await.unwrap());
        assert_eq!(storage.read("spool/key.deb").await.unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn test_spool_drop_without_commit_discards() {
        // Dropping without commit/abort must not publish the object
        // (object_store's Drop impl removes the staged file).
        let storage = storage().await;
        let mut spool = storage.spool("spool/dropped.deb").await.unwrap();
        spool.write(b"never committed");
        drop(spool);

        assert!(!storage.exists("spool/dropped.deb").await.unwrap());
    }

    #[tokio::test]
    async fn test_spool_abort_discards() {
        let storage = storage().await;
        let mut spool = storage.spool("spool/abort.deb").await.unwrap();
        spool.write(b"partial data");
        spool.abort().await;

        assert!(!storage.exists("spool/abort.deb").await.unwrap());
    }

    #[tokio::test]
    async fn test_spool_large_stream_chunked_writes() {
        // Multiple parts so WriteMultipart flushes to the store more than once.
        let storage = storage().await;
        let mut spool = storage.spool("spool/large.bin").await.unwrap();

        let chunk = vec![7u8; 64 * 1024];
        let mut expected_hash = sha2::Sha256::new();
        for _ in 0..200 {
            spool.write(&chunk);
            expected_hash.update(&chunk);
        }
        let meta = spool.commit().await.unwrap();
        assert_eq!(meta.size_bytes, 12_800 * 1024);
        assert_eq!(
            meta.checksum_sha256,
            format!("{:x}", expected_hash.finalize())
        );
    }

    #[tokio::test]
    async fn test_read_range_basic() {
        let storage = storage().await;
        storage.write("range/key", b"0123456789").await.unwrap();

        let (mut stream, total) = storage.read_stream("range/key", 2..5).await.unwrap();
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, b"234");
        assert_eq!(total, 10);
    }

    #[tokio::test]
    async fn test_read_range_start_beyond_eof_is_error() {
        let storage = storage().await;
        storage.write("range/short", b"abc").await.unwrap();
        // object_store returns an error for ranges starting past EOF
        assert!(storage.read_stream("range/short", 10..15).await.is_err());
    }

    #[tokio::test]
    async fn test_read_stream_to_eof() {
        let storage = storage().await;
        storage.write("range/eof", b"abcdef").await.unwrap();

        let (mut stream, total) = storage.read_stream("range/eof", 0..u64::MAX).await.unwrap();
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, b"abcdef");
        assert_eq!(total, 6);
    }
}
