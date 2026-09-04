// SPDX-License-Identifier: MIT OR Apache-2.0
//! Pack file support for the content-addressed store.
//!
//! Pack files bundle multiple blobs into a single file, reducing
//! filesystem overhead for repositories with many small objects.
//!
//! # Pack Format (v1)
//!
//! ```text
//! .pack file:
//!   "SPCK"                    magic
//!   u32 LE                    version (1)
//!   u32 LE                    object count
//!   per object:
//!     u8                      object type (1 = blob)
//!     u32 LE                  uncompressed length
//!     u32 LE                  compressed length
//!     [32]                    BLAKE3 digest
//!     [compressed length]     Zstd frame
//!
//! .idx file:
//!   "SIDX"                    magic
//!   u32 LE                    version (1)
//!   u32 LE                    entry count
//!   per entry:
//!     [32]                    BLAKE3 digest
//!     u64 LE                  offset into the .pack file
//! ```
//!
//! The index is sorted by digest on load so lookups binary-search.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use thiserror::Error;

#[cfg(feature = "zstd")]
use crate::compressor;
use crate::hash::Hash;
use crate::hasher;

/// Errors that can occur while reading or writing pack files.
#[derive(Error, Debug)]
pub enum PackError {
    /// A `.pack` file did not start with the `SPCK` magic.
    #[error("invalid pack magic: {0}")]
    InvalidMagic(String),
    /// The pack version is newer than this build supports.
    #[error("unsupported pack version: {0}")]
    UnsupportedVersion(u32),
    /// An `.idx` file did not start with the `SIDX` magic.
    #[error("invalid index magic: {0}")]
    InvalidIndexMagic(String),
    /// The digest is not present in this pack.
    #[error("blob not found in pack: {0}")]
    BlobNotFound(String),
    /// An underlying filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// Zstd compression failed while writing the pack.
    #[error("compression error: {0}")]
    CompressionError(String),
    /// Zstd decompression failed while reading the pack.
    #[error("decompression error: {0}")]
    DecompressionError(String),
    /// At least one object is required to create a pack.
    #[error("cannot create empty pack")]
    EmptyPack,
    /// An object record used a type byte this build does not know.
    #[error("unexpected object type: {0}")]
    UnexpectedObjectType(u8),
    /// The blob read from the pack did not match its recorded digest.
    #[error("hash mismatch in pack: expected {expected}, got {actual}")]
    HashMismatch {
        /// The digest the object was addressed by.
        expected: String,
        /// The digest of what was actually read.
        actual: String,
    },
}

const PACK_MAGIC: &[u8; 4] = b"SPCK";
const INDEX_MAGIC: &[u8; 4] = b"SIDX";
const PACK_VERSION: u32 = 1;
const TYPE_BLOB: u8 = 1;

#[derive(Clone, Debug)]
struct PackIndexEntry {
    hash: Hash,
    offset: u64,
}

/// A parsed, sorted `.idx` file.
#[derive(Clone, Debug)]
pub struct PackIndex {
    entries: Vec<PackIndexEntry>,
}

impl PackIndex {
    /// Load and validate an index from disk.
    pub fn load(path: &Path) -> Result<Self, PackError> {
        let file = fs::File::open(path)?;
        let mut reader = BufReader::new(file);

        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;
        if &magic != INDEX_MAGIC {
            return Err(PackError::InvalidIndexMagic(
                String::from_utf8_lossy(&magic).to_string(),
            ));
        }

        let mut version = [0u8; 4];
        reader.read_exact(&mut version)?;
        let version = u32::from_le_bytes(version);
        if version != PACK_VERSION {
            return Err(PackError::UnsupportedVersion(version));
        }

        let mut count = [0u8; 4];
        reader.read_exact(&mut count)?;
        let count = u32::from_le_bytes(count) as usize;

        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let mut hash_bytes = [0u8; 32];
            reader.read_exact(&mut hash_bytes)?;
            let mut offset_bytes = [0u8; 8];
            reader.read_exact(&mut offset_bytes)?;
            entries.push(PackIndexEntry {
                hash: Hash::from(hash_bytes),
                offset: u64::from_le_bytes(offset_bytes),
            });
        }

        entries.sort_by_key(|e| e.hash);

        Ok(Self { entries })
    }

    /// Look up the pack offset for a digest, if present.
    #[must_use]
    pub fn find(&self, hash: &Hash) -> Option<u64> {
        self.entries
            .binary_search_by_key(hash, |e| e.hash)
            .ok()
            .map(|idx| self.entries[idx].offset)
    }

    /// All digests in the index (sorted).
    #[must_use]
    pub fn hashes(&self) -> Vec<Hash> {
        self.entries.iter().map(|e| e.hash).collect()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Writer/reader for `.pack` files.
pub struct PackFile;

impl PackFile {
    /// Write a new pack file (and its index) containing `objects`.
    ///
    /// The pack name is derived from the BLAKE3 hash of the index, so
    /// identical packs are naturally deduplicated on disk.
    pub fn create(
        pack_dir: &Path,
        objects: &[(Hash, Vec<u8>)],
    ) -> Result<(PathBuf, PathBuf), PackError> {
        if objects.is_empty() {
            return Err(PackError::EmptyPack);
        }

        fs::create_dir_all(pack_dir)?;

        let mut pack_data = Vec::new();
        let mut index_entries = Vec::new();

        pack_data.extend_from_slice(PACK_MAGIC);
        pack_data.extend_from_slice(&PACK_VERSION.to_le_bytes());
        pack_data.extend_from_slice(&(objects.len() as u32).to_le_bytes());

        for (hash, data) in objects {
            let offset = pack_data.len() as u64;

            // Without the `zstd` feature the object payload is stored raw;
            // packs remain self-consistent within a single build.
            #[cfg(feature = "zstd")]
            let compressed = compressor::compress_default(data)
                .map_err(|e| PackError::CompressionError(e.to_string()))?;
            #[cfg(not(feature = "zstd"))]
            let compressed = data.clone();

            pack_data.push(TYPE_BLOB);
            pack_data.extend_from_slice(&(data.len() as u32).to_le_bytes());
            pack_data.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
            pack_data.extend_from_slice(&hash.0);
            pack_data.extend_from_slice(&compressed);

            index_entries.push(PackIndexEntry {
                hash: *hash,
                offset,
            });
        }

        let index_data = Self::serialize_index(&index_entries);
        let index_hash = hasher::hash_bytes(&index_data);
        let name = format!("pack-{}", index_hash.to_hex());

        let pack_path = pack_dir.join(format!("{name}.pack"));
        let idx_path = pack_dir.join(format!("{name}.idx"));

        fs::write(&pack_path, &pack_data)?;
        fs::write(&idx_path, &index_data)?;

        Ok((pack_path, idx_path))
    }

    fn serialize_index(entries: &[PackIndexEntry]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(INDEX_MAGIC);
        data.extend_from_slice(&PACK_VERSION.to_le_bytes());
        data.extend_from_slice(&(entries.len() as u32).to_le_bytes());

        for entry in entries {
            data.extend_from_slice(&entry.hash.0);
            data.extend_from_slice(&entry.offset.to_le_bytes());
        }

        data
    }

    /// Read a single blob out of a pack file, verifying its digest.
    pub fn read_blob(
        pack_path: &Path,
        index: &PackIndex,
        hash: &Hash,
    ) -> Result<Vec<u8>, PackError> {
        let offset = index
            .find(hash)
            .ok_or_else(|| PackError::BlobNotFound(hash.to_hex()))?;

        let file = fs::File::open(pack_path)?;
        let mut reader = BufReader::new(file);

        reader.seek(SeekFrom::Start(offset))?;

        let mut type_byte = [0u8; 1];
        reader.read_exact(&mut type_byte)?;
        if type_byte[0] != TYPE_BLOB {
            return Err(PackError::UnexpectedObjectType(type_byte[0]));
        }

        let mut uncomp_size = [0u8; 4];
        reader.read_exact(&mut uncomp_size)?;
        let _uncomp_size = u32::from_le_bytes(uncomp_size) as usize;

        let mut comp_size = [0u8; 4];
        reader.read_exact(&mut comp_size)?;
        let comp_size = u32::from_le_bytes(comp_size) as usize;

        let mut stored_hash = [0u8; 32];
        reader.read_exact(&mut stored_hash)?;

        let mut compressed = vec![0u8; comp_size];
        reader.read_exact(&mut compressed)?;

        // Without the `zstd` feature the payload is expected raw; a frame
        // written by a zstd-enabled build fails hash verification below
        // instead of silently yielding its compressed bytes.
        #[cfg(feature = "zstd")]
        let data = compressor::decompress(&compressed)
            .map_err(|e| PackError::DecompressionError(e.to_string()))?;
        #[cfg(not(feature = "zstd"))]
        let data = compressed;

        let actual_hash = hasher::hash_bytes(&data);
        if actual_hash != *hash {
            return Err(PackError::HashMismatch {
                expected: hash.to_hex(),
                actual: actual_hash.to_hex(),
            });
        }

        Ok(data)
    }

    /// List all `.pack` files in `pack_dir` (sorted). An absent directory
    /// yields an empty list.
    pub fn list_packs(pack_dir: &Path) -> io::Result<Vec<PathBuf>> {
        if !pack_dir.exists() {
            return Ok(Vec::new());
        }
        let mut packs = Vec::new();
        for entry in fs::read_dir(pack_dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".pack") {
                    packs.push(entry.path());
                }
            }
        }
        packs.sort();
        Ok(packs)
    }
}

/// Cache of loaded pack indices for efficient lookup.
#[derive(Debug)]
pub struct PackCache {
    indices: HashMap<PathBuf, PackIndex>,
}

impl PackCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            indices: HashMap::new(),
        }
    }

    /// Load all pack indices from the pack directory.
    pub fn load_all(pack_dir: &Path) -> Result<Self, PackError> {
        let mut cache = Self::new();
        let pack_files = PackFile::list_packs(pack_dir)?;

        for pack_path in &pack_files {
            let idx_path = pack_path.with_extension("idx");
            if idx_path.exists() {
                let index = PackIndex::load(&idx_path)?;
                cache.indices.insert(pack_path.clone(), index);
            }
        }

        Ok(cache)
    }

    /// Find a hash across all loaded pack indices, returning the pack
    /// path and offset.
    #[must_use]
    pub fn find(&self, hash: &Hash) -> Option<(&PathBuf, u64)> {
        for (pack_path, index) in &self.indices {
            if let Some(offset) = index.find(hash) {
                return Some((pack_path, offset));
            }
        }
        None
    }

    /// List all hashes across all loaded pack indices (sorted, deduped).
    #[must_use]
    pub fn all_hashes(&self) -> Vec<Hash> {
        let mut hashes = Vec::new();
        for index in self.indices.values() {
            hashes.extend(index.hashes());
        }
        hashes.sort();
        hashes.dedup();
        hashes
    }

    /// Number of pack files loaded.
    #[must_use]
    pub fn pack_count(&self) -> usize {
        self.indices.len()
    }

    /// Total number of objects across all packs.
    #[must_use]
    pub fn object_count(&self) -> usize {
        self.indices.values().map(PackIndex::len).sum()
    }
}

impl Default for PackCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Context type used to convert pack errors into `Box<dyn Error>` so
    /// tests can use `?` instead of `unwrap`.
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn make_test_objects() -> Vec<(Hash, Vec<u8>)> {
        vec![
            {
                let data = b"hello, world!".to_vec();
                let hash = hasher::hash_bytes(&data);
                (hash, data)
            },
            {
                let data = b"second blob content".to_vec();
                let hash = hasher::hash_bytes(&data);
                (hash, data)
            },
            {
                let data = vec![0u8; 1024];
                let hash = hasher::hash_bytes(&data);
                (hash, data)
            },
        ]
    }

    #[test]
    fn test_pack_create_and_read() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");
        let objects = make_test_objects();

        let (pack_path, idx_path) = PackFile::create(&pack_dir, &objects)?;
        assert!(pack_path.exists());
        assert!(idx_path.exists());
        assert!(pack_path
            .to_str()
            .ok_or("non-utf8 pack path")?
            .ends_with(".pack"));
        assert!(idx_path
            .to_str()
            .ok_or("non-utf8 idx path")?
            .ends_with(".idx"));

        let index = PackIndex::load(&idx_path)?;
        assert_eq!(index.len(), 3);

        for (hash, data) in &objects {
            let retrieved = PackFile::read_blob(&pack_path, &index, hash)?;
            assert_eq!(*data, retrieved);
        }
        Ok(())
    }

    #[test]
    fn test_pack_index_sorted() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");
        let objects = make_test_objects();

        let (_, idx_path) = PackFile::create(&pack_dir, &objects)?;
        let index = PackIndex::load(&idx_path)?;

        let hashes = index.hashes();
        let mut sorted = hashes.clone();
        sorted.sort();
        assert_eq!(hashes, sorted);
        Ok(())
    }

    #[test]
    fn test_pack_index_find_missing() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");
        let objects = make_test_objects();

        let (_, idx_path) = PackFile::create(&pack_dir, &objects)?;
        let index = PackIndex::load(&idx_path)?;

        let missing = Hash::from_hex(&"f".repeat(64))?;
        assert!(index.find(&missing).is_none());
        Ok(())
    }

    #[test]
    fn test_pack_create_empty_fails() {
        let dir = tempfile::tempdir().expect("tempdir for empty-pack test");
        let pack_dir = dir.path().join("pack");
        let result = PackFile::create(&pack_dir, &[]);
        assert!(matches!(result, Err(PackError::EmptyPack)));
    }

    #[test]
    fn test_pack_list_packs() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");

        assert_eq!(PackFile::list_packs(&pack_dir)?.len(), 0);

        let objects = make_test_objects();
        PackFile::create(&pack_dir, &objects)?;

        let packs = PackFile::list_packs(&pack_dir)?;
        assert_eq!(packs.len(), 1);
        assert!(packs[0]
            .to_str()
            .ok_or("non-utf8 pack path")?
            .ends_with(".pack"));
        Ok(())
    }

    #[test]
    fn test_pack_cache() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");
        let objects = make_test_objects();

        PackFile::create(&pack_dir, &objects)?;

        let cache = PackCache::load_all(&pack_dir)?;
        assert_eq!(cache.pack_count(), 1);
        assert_eq!(cache.object_count(), 3);

        let all_hashes = cache.all_hashes();
        assert_eq!(all_hashes.len(), 3);

        for (hash, _data) in &objects {
            let (pack_path, offset) = cache.find(hash).ok_or("expected hash in cache")?;
            assert!(pack_path.exists());
            assert!(offset > 0);
        }
        Ok(())
    }

    #[test]
    fn test_pack_cache_missing() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");
        let objects = make_test_objects();

        PackFile::create(&pack_dir, &objects)?;

        let cache = PackCache::load_all(&pack_dir)?;
        let missing = Hash::from_hex(&"a".repeat(64))?;
        assert!(cache.find(&missing).is_none());
        Ok(())
    }

    #[test]
    fn test_pack_invalid_magic() -> TestResult {
        let dir = tempfile::tempdir()?;
        let bad_idx = dir.path().join("bad.idx");
        fs::write(&bad_idx, b"XXXX")?;

        let result = PackIndex::load(&bad_idx);
        assert!(matches!(result, Err(PackError::InvalidIndexMagic(_))));
        Ok(())
    }

    #[test]
    fn test_pack_single_object() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");
        let data = b"single object".to_vec();
        let hash = hasher::hash_bytes(&data);

        let (pack_path, idx_path) = PackFile::create(&pack_dir, &[(hash, data.clone())])?;

        let index = PackIndex::load(&idx_path)?;
        assert_eq!(index.len(), 1);

        let retrieved = PackFile::read_blob(&pack_path, &index, &hash)?;
        assert_eq!(data, retrieved);
        Ok(())
    }

    #[test]
    fn test_pack_index_corrupt_version() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");
        let objects = make_test_objects();
        let (_, idx_path) = PackFile::create(&pack_dir, &objects)?;

        // Overwrite the version field (bytes 4..8) with 2.
        let mut raw = fs::read(&idx_path)?;
        raw[4..8].copy_from_slice(&2u32.to_le_bytes());
        fs::write(&idx_path, &raw)?;

        let result = PackIndex::load(&idx_path);
        assert!(matches!(result, Err(PackError::UnsupportedVersion(2))));
        Ok(())
    }

    #[test]
    fn test_index_len_is_empty_consistency() -> TestResult {
        let dir = tempfile::tempdir()?;
        let pack_dir = dir.path().join("pack");
        let objects = make_test_objects();
        let (_, idx_path) = PackFile::create(&pack_dir, &objects)?;
        let index = PackIndex::load(&idx_path)?;
        assert!(!index.is_empty());

        let empty = PackIndex {
            entries: Vec::new(),
        };
        assert!(empty.is_empty());
        assert_eq!(empty.hashes().len(), 0);
        Ok(())
    }
}
