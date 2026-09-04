// SPDX-License-Identifier: MIT OR Apache-2.0
//! Integration tests for `cas-kit`: end-to-end put/get/verify roundtrips,
//! on-disk bucketing layout, pack write/read/repack, and cache behavior.

use std::fs;

use cas_kit::{BlobStore, CasError, Hash};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn read_on_disk(store: &BlobStore, hash: &Hash) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let hex = hash.to_hex();
    let path = store.objects_dir().join(&hex[..2]).join(&hex[2..]);
    Ok(fs::read(path)?)
}

fn write_on_disk(store: &BlobStore, hash: &Hash, bytes: &[u8]) -> TestResult {
    let hex = hash.to_hex();
    let path = store.objects_dir().join(&hex[..2]).join(&hex[2..]);
    fs::create_dir_all(path.parent().ok_or("no parent")?)?;
    fs::write(path, bytes)?;
    Ok(())
}

#[test]
fn put_get_verify_roundtrip() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new(dir.path())?;

    let data: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
    let hash = store.put_blob(&data)?;

    assert!(store.has_blob(&hash));
    let out = store.get_blob(&hash)?;
    assert_eq!(out, data);
    Ok(())
}

#[test]
fn verify_on_read_detects_bitrot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;

    let hash = store.put_blob(b"payload to corrupt")?;
    let mut on_disk = read_on_disk(&store, &hash)?;
    on_disk[0] ^= 0xFF;
    write_on_disk(&store, &hash, &on_disk)?;

    match store.get_blob(&hash) {
        Err(CasError::HashMismatch { expected, actual }) => {
            assert_eq!(expected, hash.to_hex());
            assert_ne!(actual, hash.to_hex());
        }
        other => panic!("expected HashMismatch, got {other:?}"),
    }
    Ok(())
}

#[test]
fn bucketing_layout_uses_256_prefix_dirs() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;

    // Fill every 2-hex bucket: force blobs whose hash starts with each byte.
    // Instead of mining hashes, place files directly for all 256 prefixes.
    let objects = store.objects_dir();
    for i in 0..=255u8 {
        let prefix = format!("{i:02x}");
        let full = format!("{prefix}{}", "a".repeat(62));
        let bucket = objects.join(&prefix);
        fs::create_dir_all(&bucket)?;
        fs::write(bucket.join(&full[2..]), b"bucket filler")?;
    }

    let buckets: Vec<_> = fs::read_dir(&objects)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy() != "pack")
        .collect();
    assert_eq!(buckets.len(), 256, "layout must use 256 2-hex buckets");

    // Every file lands in a 2-char directory named by its hash prefix.
    let hashes = store.list_blobs()?;
    assert_eq!(hashes.len(), 256);
    for h in &hashes {
        let hex = h.to_hex();
        let parent = store.objects_dir().join(&hex[..2]);
        assert!(parent.is_dir(), "bucket {} must exist", &hex[..2]);
        assert!(store.has_blob(h));
    }
    Ok(())
}

#[test]
fn bucketing_layout_real_blob_lands_in_prefix_dir() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;

    let hash = store.put_blob(b"layout check")?;
    let hex = hash.to_hex();
    let expected_dir = store.objects_dir().join(&hex[..2]);
    let expected_file = expected_dir.join(&hex[2..]);

    assert!(expected_dir.is_dir());
    assert!(expected_file.is_file());
    Ok(())
}

#[test]
fn pack_write_read_repack_roundtrip() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;

    let blobs: Vec<Vec<u8>> = (0..8)
        .map(|i| format!("repack payload {i}").into_bytes())
        .collect();
    let mut hashes = Vec::new();
    for b in &blobs {
        hashes.push(store.put_blob(b)?);
    }
    assert_eq!(store.blob_count()?, 8);

    // Pack everything: threshold 0 packs all loose blobs.
    let packed = store.repack(0)?;
    assert_eq!(packed, 8);
    assert_eq!(store.blob_count()?, 0, "loose blobs must be gone");

    // Pack dir now contains at least one .pack/.idx pair.
    let pack_dir = store.pack_dir();
    assert!(pack_dir.is_dir());
    let packs = cas_kit::PackFile::list_packs(&pack_dir)?;
    assert!(!packs.is_empty());

    // All blobs still readable through the pack path.
    for (hash, blob) in hashes.iter().zip(&blobs) {
        assert_eq!(store.get_blob(hash)?, *blob);
        assert!(store.has_blob_packed(hash));
    }

    // list_blobs_packed agrees.
    let packed_hashes = store.list_blobs_packed()?;
    assert_eq!(packed_hashes.len(), 8);
    for h in &hashes {
        assert!(packed_hashes.contains(h));
    }
    Ok(())
}

#[test]
fn blob_cache_serves_second_read() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut store = BlobStore::new_uncompressed(dir.path())?;

    let data = b"cache me please";
    let hash = store.put_blob(data)?;

    // First read populates the in-memory ring cache.
    assert_eq!(store.get_blob(&hash)?, data.to_vec());

    // Corrupt the on-disk copy. A disk read would fail verification.
    write_on_disk(&store, &hash, b"corrupted bytes on disk")?;

    // Second read is served from the cache and returns the original.
    assert_eq!(store.get_blob(&hash)?, data.to_vec());

    // With the cache bypassed via a fresh store over the same root, the
    // corruption is detected.
    let store2 = BlobStore::new_uncompressed(dir.path())?;
    assert!(matches!(
        store2.get_blob(&hash),
        Err(CasError::HashMismatch { .. })
    ));

    // Verification can be disabled for hot paths.
    store.set_verify_on_read(false);
    assert!(!store.verify_on_read());
    Ok(())
}

#[test]
fn dedup_and_delete_lifecycle() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;

    let h1 = store.put_blob(b"lifecycle")?;
    let h2 = store.put_blob(b"lifecycle")?;
    assert_eq!(h1, h2);
    assert!(matches!(
        store.put_blob_new(b"lifecycle"),
        Err(CasError::AlreadyExists(_))
    ));

    store.delete_blob(&h1)?;
    assert!(!store.has_blob(&h1));
    assert_eq!(store.blob_count()?, 0);
    assert_eq!(store.total_size()?, 0);
    Ok(())
}

#[test]
fn shared_across_threads_via_arc() -> TestResult {
    use std::sync::Arc;
    use std::thread;

    let dir = tempfile::tempdir()?;
    let store = Arc::new(BlobStore::new_uncompressed(dir.path())?);

    let hash = store.put_blob(b"threaded")?;

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let store = Arc::clone(&store);
            move || -> Result<(), CasError> {
                let got = store.get_blob(&hash)?;
                assert_eq!(got, b"threaded".to_vec());
                let extra = format!("blob {i}").into_bytes();
                let h = store.put_blob(&extra)?;
                assert_eq!(store.get_blob(&h)?, extra);
                Ok(())
            }
        })
        .map(thread::spawn)
        .collect();

    for h in handles {
        h.join().map_err(|e| format!("worker panicked: {e:?}"))??;
    }
    Ok(())
}
