// SPDX-License-Identifier: MIT OR Apache-2.0
//! Blob Store — the primary CAS interface for storing and retrieving blobs.
//!
//! Blobs are stored on disk using a content-addressed scheme:
//! - Hash is split into a 2-char prefix directory and 62-char filename
//! - This creates 256 buckets, avoiding any single directory having too many files
//! - Blobs are optionally Zstd-compressed
//!
//! # Thread Safety
//!
//! [`BlobStore`] is `Send + Sync` and can be shared across threads via `Arc`.
//! File operations are the primary bottleneck; the store itself holds no mutable
//! state beyond the root path. Interior caches use `std::sync::Mutex`; a poisoned
//! lock is surfaced as [`CasError::LockPoisoned`] where recovery is meaningful,
//! and ignored (best-effort) where the operation is advisory.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::compressor;
use crate::error::CasError;
use crate::hash::Hash;
use crate::hasher;
use crate::pack::{PackCache, PackFile, PackIndex};

/// Default maximum number of entries in the in-memory blob cache.
const BLOB_CACHE_CAPACITY: usize = 1024;

/// Check whether data starts with the Zstd frame magic (`0x28 0xB5 0x2F 0xFD`).
#[cfg(feature = "zstd")]
#[must_use]
pub fn is_zstd_compressed(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == [0x28, 0xB5, 0x2F, 0xFD]
}

/// Feature-less build: the store never writes Zstd frames, so every blob
/// is treated as raw. Reading a store written by a zstd-enabled build
/// fails hash verification on such blobs instead of silently returning
/// still-compressed bytes.
#[cfg(not(feature = "zstd"))]
#[must_use]
pub fn is_zstd_compressed(_data: &[u8]) -> bool {
    false
}

/// The content-addressable-storage blob store.
///
/// Stores blobs indexed by BLAKE3 hash on the local filesystem.
/// Provides deduplication, optional compression, and integrity verification.
///
/// # Thread Safety
///
/// `BlobStore` is `Send + Sync` and can be shared across threads via `Arc`.
/// The pack index cache uses `Mutex` for interior mutability.
pub struct BlobStore {
    /// Root directory containing the `objects/` subdirectory.
    root: PathBuf,
    /// Whether to compress blobs with Zstd.
    compress: bool,
    /// Zstd compression level (1-22).
    #[cfg_attr(not(feature = "zstd"), allow(dead_code))]
    compression_level: i32,
    /// Whether to verify blob hashes on read. Default: true.
    /// Set to false for hot paths where performance matters more than
    /// per-read integrity verification (content addressing already
    /// provides correctness by construction).
    verify_on_read: bool,
    /// Cached pack indices, loaded lazily on first pack access.
    /// Invalidated when `repack()` creates new pack files.
    pack_cache: Mutex<Option<PackCache>>,
    /// In-memory LRU-like blob cache. Uses a simple ordered Vec as a ring buffer
    /// to bound memory usage without external dependencies. Most-recently-accessed
    /// entries are promoted to the front on cache hit.
    blob_cache: Mutex<Vec<(Hash, Vec<u8>)>>,
    /// Cache of known blob prefix directories (2-hex-char buckets).
    /// Avoids redundant `fs::create_dir_all` syscalls when many blobs share
    /// the same prefix. At most 256 entries (00–ff).
    known_dirs: Mutex<HashSet<PathBuf>>,
}

impl BlobStore {
    /// Create a new BlobStore rooted at the given directory.
    ///
    /// Creates the `objects/` subdirectory if it doesn't exist.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, CasError> {
        let root = root.into();
        let objects_dir = root.join("objects");
        fs::create_dir_all(&objects_dir)?;
        Ok(Self {
            root,
            compress: true,
            compression_level: compressor::DEFAULT_COMPRESSION_LEVEL,
            verify_on_read: true,
            pack_cache: Mutex::new(None),
            blob_cache: Mutex::new(Vec::with_capacity(BLOB_CACHE_CAPACITY)),
            known_dirs: Mutex::new(HashSet::new()),
        })
    }

    /// Create a BlobStore backed by a temporary directory.
    ///
    /// Useful for testing and in-memory repository usage. The temporary
    /// directory is cleaned up when the returned `TempDir` is dropped.
    pub fn open_in_memory() -> Result<(tempfile::TempDir, Self), CasError> {
        let root = tempfile::tempdir()?;
        let objects_dir = root.path().join("objects");
        fs::create_dir_all(&objects_dir)?;
        let store = Self {
            root: root.path().to_path_buf(),
            compress: true,
            compression_level: compressor::DEFAULT_COMPRESSION_LEVEL,
            verify_on_read: true,
            pack_cache: Mutex::new(None),
            blob_cache: Mutex::new(Vec::with_capacity(BLOB_CACHE_CAPACITY)),
            known_dirs: Mutex::new(HashSet::new()),
        };
        Ok((root, store))
    }

    /// Create a BlobStore with compression disabled (for testing).
    pub fn new_uncompressed(root: impl Into<PathBuf>) -> Result<Self, CasError> {
        let mut store = Self::new(root)?;
        store.compress = false;
        Ok(store)
    }

    /// Set whether to verify blob hashes on read.
    ///
    /// When disabled, `get_blob()` skips the BLAKE3 hash verification
    /// step, saving O(n) computation per read. The content-addressed
    /// storage scheme already provides correctness by construction
    /// (the filename is the hash), so this is safe for performance-critical
    /// paths which may read many blobs in sequence.
    pub fn set_verify_on_read(&mut self, verify: bool) {
        self.verify_on_read = verify;
    }

    /// Check whether hash verification is enabled on read.
    pub fn verify_on_read(&self) -> bool {
        self.verify_on_read
    }

    fn ensure_parent_dir(&self, parent: &std::path::Path) -> Result<(), CasError> {
        {
            let known = self
                .known_dirs
                .lock()
                .map_err(|e| CasError::LockPoisoned(e.to_string()))?;
            if known.contains(parent) {
                return Ok(());
            }
        }
        fs::create_dir_all(parent)?;
        self.known_dirs
            .lock()
            .map_err(|e| CasError::LockPoisoned(e.to_string()))?
            .insert(parent.to_path_buf());
        Ok(())
    }

    /// Store a blob, returning its BLAKE3 hash.
    ///
    /// If a blob with the same hash already exists, this is a no-op
    /// (deduplication). Returns the hash either way.
    pub fn put_blob(&self, data: &[u8]) -> Result<Hash, CasError> {
        let hash = hasher::hash_bytes(data);
        let blob_path = self.blob_path(&hash);

        // Deduplication: if blob already exists, return immediately.
        if blob_path.exists() {
            return Ok(hash);
        }

        // Ensure the prefix directory exists.
        if let Some(parent) = blob_path.parent() {
            self.ensure_parent_dir(parent)?;
        }

        // Write blob (optionally compressed).
        #[cfg(feature = "zstd")]
        if self.compress {
            let compressed = compressor::compress(data, self.compression_level)?;
            fs::write(&blob_path, &compressed)?;
        } else {
            fs::write(&blob_path, data)?;
        }
        #[cfg(not(feature = "zstd"))]
        fs::write(&blob_path, data)?;

        Ok(hash)
    }

    /// Store a blob, returning an error if it already exists.
    pub fn put_blob_new(&self, data: &[u8]) -> Result<Hash, CasError> {
        let hash = hasher::hash_bytes(data);
        let blob_path = self.blob_path(&hash);

        if blob_path.exists() {
            return Err(CasError::AlreadyExists(hash.to_hex()));
        }

        if let Some(parent) = blob_path.parent() {
            self.ensure_parent_dir(parent)?;
        }

        #[cfg(feature = "zstd")]
        if self.compress {
            let compressed = compressor::compress(data, self.compression_level)?;
            fs::write(&blob_path, &compressed)?;
        } else {
            fs::write(&blob_path, data)?;
        }
        #[cfg(not(feature = "zstd"))]
        fs::write(&blob_path, data)?;

        Ok(hash)
    }

    /// Store a blob with an explicit hash (used when receiving blobs from a remote).
    ///
    /// Verifies the data matches the expected hash before storing.
    pub fn put_blob_with_hash(&self, data: &[u8], expected_hash: &Hash) -> Result<(), CasError> {
        let blob_path = self.blob_path(expected_hash);

        if blob_path.exists() {
            return Ok(());
        }

        hasher::verify_hash(data, expected_hash)?;

        if let Some(parent) = blob_path.parent() {
            self.ensure_parent_dir(parent)?;
        }

        #[cfg(feature = "zstd")]
        if self.compress {
            let compressed = compressor::compress(data, self.compression_level)?;
            fs::write(&blob_path, &compressed)?;
        } else {
            fs::write(&blob_path, data)?;
        }
        #[cfg(not(feature = "zstd"))]
        fs::write(&blob_path, data)?;

        Ok(())
    }

    /// Retrieve a blob by its BLAKE3 hash.
    ///
    /// Tries the in-memory cache, then loose objects, then pack files.
    /// Decompresses if necessary and verifies the hash of the result
    /// (unless verification was disabled via `set_verify_on_read(false)`).
    pub fn get_blob(&self, hash: &Hash) -> Result<Vec<u8>, CasError> {
        {
            let mut cache = self
                .blob_cache
                .lock()
                .map_err(|e| CasError::LockPoisoned(e.to_string()))?;
            if let Some(pos) = cache.iter().position(|(h, _)| h == hash) {
                let (_, data) = cache.remove(pos);
                cache.insert(0, (*hash, data.clone()));
                return Ok(data);
            }
        }

        let data = if self.blob_path(hash).exists() {
            let raw = fs::read(self.blob_path(hash))?;
            #[cfg(feature = "zstd")]
            let result = if is_zstd_compressed(&raw) {
                compressor::decompress(&raw)?
            } else {
                raw
            };
            #[cfg(not(feature = "zstd"))]
            let result = raw;
            if self.verify_on_read {
                hasher::verify_hash(&result, hash)?;
            }
            result
        } else {
            match self.get_blob_packed(hash) {
                Ok(data) => data,
                // A genuinely absent blob is the expected miss path; any
                // other error (I/O, corrupt pack, poisoned lock) is real
                // corruption or resource failure and must not masquerade
                // as "not found".
                Err(CasError::BlobNotFound(_)) => {
                    return Err(CasError::BlobNotFound(hash.to_hex()))
                }
                Err(e) => return Err(e),
            }
        };

        self.cache_blob(*hash, data.clone());
        Ok(data)
    }

    /// Insert a blob into the in-memory cache with LRU eviction.
    fn cache_blob(&self, hash: Hash, data: Vec<u8>) {
        // Best-effort caching: if the lock is poisoned the blob is still
        // on disk, so a failed cache insert only costs a future disk read.
        let Ok(mut cache) = self.blob_cache.lock() else {
            return;
        };
        // Evict oldest entry if at capacity.
        if cache.len() >= BLOB_CACHE_CAPACITY {
            cache.pop();
        }
        cache.insert(0, (hash, data));
    }

    /// Check if a blob exists in the store.
    ///
    /// Checks loose objects first, then pack files.
    /// This does NOT verify the blob's integrity — it only checks for existence.
    pub fn has_blob(&self, hash: &Hash) -> bool {
        self.blob_path(hash).exists() || self.has_blob_packed(hash)
    }

    /// Delete a blob from the store.
    ///
    /// The caller is responsible for ensuring no patches reference this blob.
    pub fn delete_blob(&self, hash: &Hash) -> Result<(), CasError> {
        let blob_path = self.blob_path(hash);
        fs::remove_file(&blob_path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                CasError::BlobNotFound(hash.to_hex())
            } else {
                CasError::Io(e)
            }
        })
    }

    /// Get the total number of loose blobs in the store.
    pub fn blob_count(&self) -> Result<u64, CasError> {
        let objects_dir = self.root.join("objects");
        let mut count = 0u64;
        if objects_dir.exists() {
            for entry in fs::read_dir(&objects_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let dir_name = entry.file_name();
                    if dir_name == "pack" {
                        continue;
                    }
                    for sub_entry in fs::read_dir(entry.path())? {
                        let sub_entry = sub_entry?;
                        if sub_entry.file_type()?.is_file() {
                            count += 1;
                        }
                    }
                }
            }
        }
        Ok(count)
    }

    /// Get the total size of all loose blobs in the store (on-disk size).
    pub fn total_size(&self) -> Result<u64, CasError> {
        let objects_dir = self.root.join("objects");
        let mut total = 0u64;
        if objects_dir.exists() {
            for entry in fs::read_dir(&objects_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let dir_name = entry.file_name();
                    if dir_name == "pack" {
                        continue;
                    }
                    for sub_entry in fs::read_dir(entry.path())? {
                        let sub_entry = sub_entry?;
                        if sub_entry.file_type()?.is_file() {
                            total += sub_entry.metadata()?.len();
                        }
                    }
                }
            }
        }
        Ok(total)
    }

    /// List all loose blob hashes in the store.
    ///
    /// Entries whose names do not form valid 64-char hex are skipped
    /// (they are not valid content addresses).
    pub fn list_blobs(&self) -> Result<Vec<Hash>, CasError> {
        let objects_dir = self.root.join("objects");
        let mut hashes = Vec::new();
        if !objects_dir.exists() {
            return Ok(hashes);
        }
        for entry in fs::read_dir(&objects_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let dir_name = entry.file_name();
                if dir_name == "pack" {
                    continue;
                }
                let prefix = dir_name.to_string_lossy().to_string();
                for sub_entry in fs::read_dir(entry.path())? {
                    let sub_entry = sub_entry?;
                    if sub_entry.file_type()?.is_file() {
                        let suffix = sub_entry.file_name().to_string_lossy().to_string();
                        let hex = format!("{prefix}{suffix}");
                        if let Ok(hash) = Hash::from_hex(&hex) {
                            hashes.push(hash);
                        }
                    }
                }
            }
        }
        hashes.sort();
        Ok(hashes)
    }

    /// Get the path to the objects directory.
    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    /// Get the store root directory (the parent of `objects/`).
    ///
    /// GC sweeps in trash mode place recoverable copies under
    /// `<root>/trash/` (see [`crate::gc`]).
    #[must_use]
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Get the path to the pack directory.
    pub fn pack_dir(&self) -> PathBuf {
        self.root.join("objects").join("pack")
    }

    /// Ensure pack cache is loaded, then call `f` with a reference to it.
    ///
    /// On first access, reads all `.idx` files from the pack directory.
    /// Subsequent calls return the cached data without disk I/O.
    /// Call `invalidate_pack_cache()` after `repack()` to force a reload.
    fn with_pack_cache<F, R>(&self, f: F) -> Result<R, CasError>
    where
        F: FnOnce(&PackCache) -> R,
    {
        let mut guard = self
            .pack_cache
            .lock()
            .map_err(|e| CasError::LockPoisoned(e.to_string()))?;
        if guard.is_none() {
            let cache = PackCache::load_all(&self.pack_dir())?;
            *guard = Some(cache);
        }
        // INVARIANT (documented-infallible): `guard` was `Some` on entry or
        // was just assigned `Some(cache)` directly above. The mutex guard
        // is exclusive, so no other thread can have set it back to `None`
        // between those two statements. The `expect` can therefore never
        // fire; it exists to convert `Option` -> `&PackCache` without
        // cloning.
        // Justified: see invariant comment above.
        #[allow(clippy::expect_used)]
        let cache = guard
            .as_ref()
            .expect("pack cache was populated two statements above under an exclusive lock");
        Ok(f(cache))
    }

    /// Invalidate the pack cache (call after repack or external pack changes).
    ///
    /// Best-effort: if the lock is poisoned the cache is left as-is; the
    /// next successful lock still sees whatever entries exist, and pack
    /// lookups remain correct because pack content is immutable once
    /// written.
    pub fn invalidate_pack_cache(&self) {
        if let Ok(mut guard) = self.pack_cache.lock() {
            *guard = None;
        }
    }

    /// Retrieve a blob from pack files only (not loose objects).
    pub fn get_blob_packed(&self, hash: &Hash) -> Result<Vec<u8>, CasError> {
        // Find which pack file contains this blob.
        let pack_path = self.with_pack_cache(|cache| cache.find(hash).map(|(p, _)| p.clone()))?;
        let pack_path = pack_path.ok_or_else(|| CasError::BlobNotFound(hash.to_hex()))?;

        let idx_path = pack_path.with_extension("idx");
        let index = PackIndex::load(&idx_path)?;
        let data = PackFile::read_blob(&pack_path, &index, hash)?;
        Ok(data)
    }

    /// Check if a blob exists in any pack file.
    ///
    /// Best-effort: a poisoned pack-cache lock reads as "not present"
    /// rather than panicking; callers doing integrity-critical work should
    /// use `get_blob` (which propagates lock poisoning) instead.
    pub fn has_blob_packed(&self, hash: &Hash) -> bool {
        self.with_pack_cache(|cache| cache.find(hash).is_some())
            .unwrap_or(false)
    }

    /// List all blob hashes stored in pack files.
    pub fn list_blobs_packed(&self) -> Result<Vec<Hash>, CasError> {
        self.with_pack_cache(PackCache::all_hashes)
    }

    /// Repack loose blobs into a pack file if the count exceeds the threshold.
    ///
    /// Returns the number of blobs that were packed. If the loose blob count
    /// is at or below the threshold, no packing occurs and 0 is returned.
    /// After successful packing, the loose blobs are removed.
    ///
    /// Loose-blob deletion after the pack write is best-effort: if a delete
    /// fails, the blob simply remains loose (and takes read priority), so
    /// no data is lost and no error is raised.
    pub fn repack(&self, threshold: usize) -> Result<usize, CasError> {
        let loose_hashes = self.list_blobs()?;
        if loose_hashes.len() <= threshold {
            return Ok(0);
        }

        let mut objects = Vec::with_capacity(loose_hashes.len());
        for hash in &loose_hashes {
            let data = self.get_blob(hash)?;
            objects.push((*hash, data));
        }

        let (pack_path, _idx_path) = PackFile::create(&self.pack_dir(), &objects)?;
        debug_assert!(pack_path.exists());

        for hash in &loose_hashes {
            // Best-effort delete (see doc comment): failure leaves the
            // blob loose, which is safe because loose reads take priority.
            let _ = self.delete_blob(hash);
        }

        // Invalidate pack cache since we created new pack files.
        self.invalidate_pack_cache();

        Ok(loose_hashes.len())
    }

    /// Get the on-disk path for a given hash.
    fn blob_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        // INVARIANT (Kani-verified in tests/kani.rs): `to_hex` always emits
        // exactly 64 lowercase hex chars whose first two encode
        // `hash.bucket()`, so the 2/62 split below can never go out of
        // bounds and always yields a 2-char bucket + 62-char filename.
        let prefix = &hex[..2];
        let suffix = &hex[2..];
        self.root.join("objects").join(prefix).join(suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// All fallible test steps use `?` into `Box<dyn Error>`; there are no
    /// bare `unwrap()`s in this crate (enforced by the unwrap sweep gate).
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn make_store() -> Result<(TempDir, BlobStore), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new_uncompressed(dir.path())?;
        Ok((dir, store))
    }

    /// Whether the pack cache is populated, mapping lock poisoning to a
    /// test failure instead of unwrapping.
    fn pack_cache_loaded(store: &BlobStore) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(store
            .pack_cache
            .lock()
            .map_err(|_| "pack cache lock poisoned")?
            .is_some())
    }

    #[test]
    fn test_put_and_get_blob() -> TestResult {
        let (_dir, store) = make_store()?;
        let data = b"hello, suture!";
        let hash = store.put_blob(data)?;

        let retrieved = store.get_blob(&hash)?;
        assert_eq!(data.as_slice(), retrieved.as_slice());
        Ok(())
    }

    #[test]
    fn test_deduplication() -> TestResult {
        let (_dir, store) = make_store()?;
        let data = b"deduplicate me";

        let h1 = store.put_blob(data)?;
        let h2 = store.put_blob(data)?;
        assert_eq!(h1, h2);

        assert_eq!(store.blob_count()?, 1, "Only one copy should exist");
        Ok(())
    }

    #[test]
    fn test_has_blob() -> TestResult {
        let (_dir, store) = make_store()?;
        let hash = store.put_blob(b"exists")?;

        assert!(store.has_blob(&hash));
        let missing = Hash::from_hex(&"f".repeat(64))?;
        assert!(!store.has_blob(&missing));
        Ok(())
    }

    #[test]
    fn test_get_nonexistent_blob() -> TestResult {
        let (_dir, store) = make_store()?;
        let missing = Hash::from_hex(&"a".repeat(64))?;
        let result = store.get_blob(&missing);
        assert!(matches!(result, Err(CasError::BlobNotFound(_))));
        Ok(())
    }

    #[test]
    fn test_delete_blob() -> TestResult {
        let (_dir, store) = make_store()?;
        let hash = store.put_blob(b"delete me")?;
        assert!(store.has_blob(&hash));

        store.delete_blob(&hash)?;
        assert!(!store.has_blob(&hash));
        Ok(())
    }

    #[test]
    fn test_delete_nonexistent_blob() -> TestResult {
        let (_dir, store) = make_store()?;
        let missing = Hash::from_hex(&"b".repeat(64))?;
        let result = store.delete_blob(&missing);
        assert!(matches!(result, Err(CasError::BlobNotFound(_))));
        Ok(())
    }

    #[test]
    fn test_put_blob_new_rejects_duplicate() -> TestResult {
        let (_dir, store) = make_store()?;
        let data = b"duplicate";
        store.put_blob(data)?;
        let result = store.put_blob_new(data);
        assert!(matches!(result, Err(CasError::AlreadyExists(_))));
        Ok(())
    }

    #[test]
    fn test_put_blob_with_hash_verifies() -> TestResult {
        let (_dir, store) = make_store()?;
        let data = b"verified content";
        let hash = hasher::hash_bytes(data);
        store.put_blob_with_hash(data, &hash)?;

        // Wrong content for a claimed (not-yet-present) hash must be rejected.
        let wrong = hasher::hash_bytes(b"other content");
        let result = store.put_blob_with_hash(b"original", &wrong);
        assert!(matches!(result, Err(CasError::HashMismatch { .. })));
        Ok(())
    }

    #[test]
    fn test_blob_count_and_list() -> TestResult {
        let (_dir, store) = make_store()?;
        store.put_blob(b"one")?;
        store.put_blob(b"two")?;
        store.put_blob(b"three")?;

        assert_eq!(store.blob_count()?, 3);
        assert_eq!(store.list_blobs()?.len(), 3);
        Ok(())
    }

    #[test]
    fn test_large_blob() -> TestResult {
        let (_dir, store) = make_store()?;
        // 10 MB blob.
        let data: Vec<u8> = (0..10_000_000).map(|i| (i % 256) as u8).collect();
        let hash = store.put_blob(&data)?;

        let retrieved = store.get_blob(&hash)?;
        assert_eq!(data.len(), retrieved.len());
        assert_eq!(data, retrieved);
        Ok(())
    }

    #[test]
    fn test_hash_integrity() -> TestResult {
        let (_dir, store) = make_store()?;
        let data = b"integrity check";
        let hash = store.put_blob(data)?;

        // Manually corrupt the stored blob.
        let blob_path = store.blob_path(&hash);
        let mut corrupted = fs::read(&blob_path)?;
        corrupted[0] = corrupted[0].wrapping_add(1);
        fs::write(&blob_path, &corrupted)?;

        // Getting the corrupted blob should fail integrity check.
        let result = store.get_blob(&hash);
        assert!(matches!(result, Err(CasError::HashMismatch { .. })));
        Ok(())
    }

    #[test]
    fn test_verify_on_read_disabled_skips_check() -> TestResult {
        let (_dir, store) = make_store()?;
        let data = b"trust me";
        let hash = store.put_blob(data)?;

        let mut store = store;
        store.set_verify_on_read(false);
        assert!(!store.verify_on_read());

        // Corrupt the loose blob; with verification off the corrupted
        // bytes come back without an error.
        let blob_path = store.blob_path(&hash);
        let mut corrupted = fs::read(&blob_path)?;
        corrupted[0] = corrupted[0].wrapping_add(1);
        fs::write(&blob_path, &corrupted)?;

        let result = store.get_blob(&hash)?;
        assert_eq!(result, corrupted);
        Ok(())
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn test_compressed_store() -> TestResult {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;

        let data = b"this will be compressed";
        let hash = store.put_blob(data)?;

        // Verify the stored file is actually compressed.
        let blob_path = store.blob_path(&hash);
        let raw = fs::read(&blob_path)?;
        assert!(is_zstd_compressed(&raw), "Blob should be Zstd-compressed");

        // Verify round-trip.
        let retrieved = store.get_blob(&hash)?;
        assert_eq!(data.as_slice(), retrieved.as_slice());
        Ok(())
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn test_decompress_rejects_zip_bomb() -> TestResult {
        use std::io::Write;

        let dir = tempfile::tempdir()?;
        let store = BlobStore::new(dir.path())?;

        // Craft a Zstd frame that expands past MAX_DECOMPRESSED_SIZE
        // without ever materializing the full payload in memory.
        let mut encoder = zstd::Encoder::new(Vec::new(), 3)?;
        let chunk = [0u8; 65536];
        let total = compressor::MAX_DECOMPRESSED_SIZE + 1;
        let mut written = 0usize;
        while written < total {
            encoder.write_all(&chunk)?;
            written += chunk.len();
        }
        let frame = encoder.finish()?;

        // Deposit the frame directly under an arbitrary address; get_blob
        // decompresses before hashing, so the cap must trip first.
        let addr = Hash::from_hex(&"7".repeat(64))?;
        let path = store.blob_path(&addr);
        fs::create_dir_all(path.parent().ok_or("address path has no parent")?)?;
        fs::write(&path, &frame)?;

        let result = store.get_blob(&addr);
        assert!(
            matches!(result, Err(CasError::DecompressionTooLarge { .. })),
            "zip bomb must be rejected"
        );
        Ok(())
    }

    #[test]
    fn test_blob_path_layout() -> TestResult {
        let (_dir, store) = make_store()?;
        let hash = hasher::hash_bytes(b"layout");
        let path = store.blob_path(&hash);
        let expected = store
            .objects_dir()
            .join(&hash.to_hex()[..2])
            .join(&hash.to_hex()[2..]);
        assert_eq!(path, expected);
        Ok(())
    }

    #[test]
    fn test_in_memory_store() -> TestResult {
        let (_root, store) = BlobStore::open_in_memory()?;
        let hash = store.put_blob(b"in-memory")?;
        assert_eq!(store.get_blob(&hash)?, b"in-memory".to_vec());
        Ok(())
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// Convert any fallible step into a proptest failure. Used instead
        /// of `unwrap` to keep the zero-unwrap gate absolute.
        fn soft<T, E: std::fmt::Display>(r: Result<T, E>) -> Result<T, TestCaseError> {
            r.map_err(|e| TestCaseError::fail(e.to_string()))
        }

        fn arb_bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
            proptest::collection::vec(proptest::num::u8::ANY, 0..max)
        }

        proptest! {
            #[test]
            fn put_get_roundtrip(data in arb_bytes(1024)) {
                let dir = soft(tempfile::tempdir())?;
                let store = soft(BlobStore::new_uncompressed(dir.path()))?;
                let hash = soft(store.put_blob(&data))?;
                let retrieved = soft(store.get_blob(&hash))?;
                prop_assert_eq!(data, retrieved);
            }

            #[test]
            fn content_addressing(data1 in arb_bytes(512), data2 in arb_bytes(512)) {
                let dir = soft(tempfile::tempdir())?;
                let store = soft(BlobStore::new_uncompressed(dir.path()))?;

                let hash1 = soft(store.put_blob(&data1))?;
                let hash2 = soft(store.put_blob(&data2))?;

                if data1 == data2 {
                    prop_assert_eq!(hash1, hash2, "same data must produce same hash");
                } else {
                    prop_assert_ne!(hash1, hash2, "different data must produce different hashes");
                }
            }

            #[test]
            fn put_twice_idempotent(data in arb_bytes(1024)) {
                let dir = soft(tempfile::tempdir())?;
                let store = soft(BlobStore::new_uncompressed(dir.path()))?;

                let hash1 = soft(store.put_blob(&data))?;
                let hash2 = soft(store.put_blob(&data))?;
                prop_assert_eq!(hash1, hash2);
                prop_assert_eq!(soft(store.blob_count())?, 1);
            }
        }
    }

    mod pack_tests {
        use super::*;

        #[test]
        fn test_get_blob_from_pack() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            let hash1 = store.put_blob(b"packed blob one")?;
            let hash2 = store.put_blob(b"packed blob two")?;

            let packed = store.repack(0)?;
            assert_eq!(packed, 2);

            assert_eq!(store.blob_count()?, 0);

            let data1 = store.get_blob(&hash1)?;
            assert_eq!(data1, b"packed blob one".to_vec());

            let data2 = store.get_blob(&hash2)?;
            assert_eq!(data2, b"packed blob two".to_vec());
            Ok(())
        }

        #[test]
        fn test_has_blob_checks_packs() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            let hash = store.put_blob(b"check me in packs")?;
            store.repack(0)?;

            assert!(store.has_blob(&hash));
            let missing = Hash::from_hex(&"c".repeat(64))?;
            assert!(!store.has_blob(&missing));
            Ok(())
        }

        #[test]
        fn test_get_blob_packed_not_found() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            let missing = Hash::from_hex(&"d".repeat(64))?;
            let result = store.get_blob_packed(&missing);
            assert!(matches!(result, Err(CasError::BlobNotFound(_))));
            Ok(())
        }

        #[test]
        fn test_list_blobs_packed() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            store.put_blob(b"alpha")?;
            store.put_blob(b"beta")?;
            store.repack(0)?;

            let packed = store.list_blobs_packed()?;
            assert_eq!(packed.len(), 2);
            Ok(())
        }

        #[test]
        fn test_repack_below_threshold() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            store.put_blob(b"only one")?;

            let packed = store.repack(10)?;
            assert_eq!(packed, 0);
            assert_eq!(store.blob_count()?, 1);
            Ok(())
        }

        #[test]
        fn test_repack_at_threshold() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            store.put_blob(b"one")?;
            store.put_blob(b"two")?;

            let packed = store.repack(2)?;
            assert_eq!(packed, 0);
            assert_eq!(store.blob_count()?, 2);

            let packed = store.repack(1)?;
            assert_eq!(packed, 2);
            assert_eq!(store.blob_count()?, 0);
            Ok(())
        }

        #[test]
        fn test_loose_priority_over_packed() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            let hash = store.put_blob(b"original data")?;
            store.repack(0)?;

            // Re-store the same hash as a loose blob.
            let blob_path = store.blob_path(&hash);
            if let Some(parent) = blob_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&blob_path, b"original data")?;

            let data = store.get_blob(&hash)?;
            assert_eq!(data, b"original data".to_vec());

            // Delete the loose blob; should still find in pack.
            store.delete_blob(&hash)?;
            let data = store.get_blob(&hash)?;
            assert_eq!(data, b"original data".to_vec());
            Ok(())
        }

        #[test]
        fn test_has_blob_packed() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            let hash = store.put_blob(b"packed check")?;
            assert!(!store.has_blob_packed(&hash));

            store.repack(0)?;
            assert!(store.has_blob_packed(&hash));
            Ok(())
        }

        #[test]
        fn test_repack_multiple_times() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            store.put_blob(b"first batch one")?;
            store.put_blob(b"first batch two")?;
            store.repack(0)?;

            store.put_blob(b"second batch")?;
            store.repack(0)?;

            let all = store.list_blobs_packed()?;
            assert_eq!(all.len(), 3);
            Ok(())
        }

        #[test]
        fn test_pack_cache_avoids_repeated_disk_reads() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            let hash = store.put_blob(b"cache me")?;
            store.repack(0)?;

            // First access: loads cache from disk.
            assert!(store.has_blob_packed(&hash));
            // Cache should now be populated.
            assert!(
                pack_cache_loaded(&store)?,
                "pack cache should be populated after first access"
            );

            // Second access: uses cached data (no disk I/O).
            assert!(store.has_blob_packed(&hash));

            // Third access: also cached.
            let data = store.get_blob_packed(&hash)?;
            assert_eq!(data, b"cache me".to_vec());
            Ok(())
        }

        #[test]
        fn test_invalidate_pack_cache() -> TestResult {
            let dir = tempfile::tempdir()?;
            let store = BlobStore::new_uncompressed(dir.path())?;

            let hash = store.put_blob(b"invalidate test")?;
            store.repack(0)?;

            // Populate cache.
            assert!(store.has_blob_packed(&hash));
            assert!(pack_cache_loaded(&store)?);

            // Invalidate.
            store.invalidate_pack_cache();
            assert!(!pack_cache_loaded(&store)?);

            // Next access reloads from disk.
            assert!(store.has_blob_packed(&hash));
            assert!(pack_cache_loaded(&store)?);
            Ok(())
        }
    }
}
