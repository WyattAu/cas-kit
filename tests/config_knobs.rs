// Tests assert invariants directly; unwraps keep failures loud.
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Config-knob behavior matrix: every public configuration surface on
//! `BlobStore` / `SweepOptions` gets a default-vs-configured differential
//! test. Each test fails if the knob stops changing observable behavior
//! (i.e., becomes a dead knob).
//!
//! | Knob | Differential test |
//! |---|---|
//! | `BlobStore::new` vs `new_uncompressed` | `compression_mode_changes_on_disk_bytes` |
//! | `set_verify_on_read(bool)` | `verify_on_read_default_rejects_corruption_disabled_returns_it` |
//! | `SweepOptions::rewrite_partial_packs` | `rewrite_partial_packs_false_leaves_partial_pack_untouched` |
//! | `BlobStore::repack(threshold)` | `repack_threshold_gates_packing` |

use std::collections::HashSet;
use std::fs;

use cas_kit::gc::{self, SweepMode, SweepOptions};
use cas_kit::store::is_zstd_compressed;
use cas_kit::{BlobStore, Hash, PackFile};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn on_disk_path(store: &BlobStore, hash: &Hash) -> std::path::PathBuf {
    let hex = hash.to_hex();
    store.objects_dir().join(&hex[..2]).join(&hex[2..])
}

fn write_on_disk(
    store: &BlobStore,
    hash: &Hash,
    bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(on_disk_path(store, hash), bytes)?;
    Ok(())
}

fn live_set(store: &BlobStore, roots: &[Hash]) -> HashSet<Hash> {
    let all: HashSet<Hash> = roots.iter().copied().collect();
    gc::mark(store, &all)
        .unwrap()
        .live
        .iter()
        .copied()
        .collect()
}

fn delete_options(rewrite_partial_packs: bool) -> SweepOptions {
    SweepOptions {
        mode: SweepMode::Delete,
        rewrite_partial_packs,
    }
}

/// Knob: `BlobStore::new` (zstd) vs `BlobStore::new_uncompressed`.
/// The compression choice must be observable in the bytes written to
/// disk, not just in memory.
#[test]
fn compression_mode_changes_on_disk_bytes() -> TestResult {
    let data = b"compression knob probe".repeat(64);

    let dir_c = tempfile::tempdir()?;
    let store_c = BlobStore::new(dir_c.path())?;
    let hash_c = store_c.put_blob(&data)?;
    let raw_c = fs::read(on_disk_path(&store_c, &hash_c))?;

    let dir_u = tempfile::tempdir()?;
    let store_u = BlobStore::new_uncompressed(dir_u.path())?;
    let hash_u = store_u.put_blob(&data)?;
    let raw_u = fs::read(on_disk_path(&store_u, &hash_u))?;

    #[cfg(feature = "zstd")]
    {
        assert!(
            is_zstd_compressed(&raw_c),
            "default store must write zstd-framed objects"
        );
        assert_ne!(raw_c, data, "compressed bytes must differ from plaintext");
    }
    #[cfg(not(feature = "zstd"))]
    {
        assert!(
            !is_zstd_compressed(&raw_c),
            "without the zstd feature the default store writes raw objects"
        );
    }
    assert!(
        !is_zstd_compressed(&raw_u),
        "new_uncompressed store must write raw objects"
    );
    assert_eq!(raw_u, data, "uncompressed bytes must be stored verbatim");

    // Both modes read back the same logical content.
    assert_eq!(store_c.get_blob(&hash_c)?, data);
    assert_eq!(store_u.get_blob(&hash_u)?, data);
    Ok(())
}

/// Knob: `set_verify_on_read(bool)`. Default (`true`) must reject a
/// corrupted on-disk object; configured `false` must return the (corrupt)
/// bytes without error.
#[test]
fn verify_on_read_default_rejects_corruption_disabled_returns_it() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut store = BlobStore::new_uncompressed(dir.path())?;

    let data = b"verify knob probe";
    let hash = store.put_blob(data)?;

    // Corrupt the loose object on disk (no prior read: cache is cold).
    write_on_disk(&store, &hash, b"corrupted bytes on disk")?;

    // Default behavior (fresh store, verify_on_read = true): detected.
    let strict = BlobStore::new_uncompressed(dir.path())?;
    assert!(
        matches!(
            strict.get_blob(&hash),
            Err(cas_kit::CasError::HashMismatch { .. })
        ),
        "default verify_on_read must surface HashMismatch on corruption"
    );

    // Configured behavior (verify_on_read = false): bytes returned as-is.
    assert!(store.verify_on_read(), "default must be verify-enabled");
    store.set_verify_on_read(false);
    assert!(!store.verify_on_read());
    let got = store.get_blob(&hash)?;
    assert_eq!(
        got, b"corrupted bytes on disk",
        "disabled verification must return the on-disk (corrupt) bytes"
    );
    Ok(())
}

/// Knob: `SweepOptions::rewrite_partial_packs`. Default (`true`) rewrites
/// a partial pack (live + garbage) down to its live objects; `false`
/// must leave that pack byte-identical, keeping the garbage packed.
#[test]
fn rewrite_partial_packs_false_leaves_partial_pack_untouched() -> TestResult {
    // Two identical fixtures: partial pack P = {live, garbage}, no loose
    // copies, so the only way to drop the garbage is a pack rewrite.
    fn build() -> Result<(tempfile::TempDir, BlobStore, Hash, Hash), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = BlobStore::new_uncompressed(dir.path())?;
        let live = store.put_blob(b"partial-pack live object")?;
        let garbage = store.put_blob(b"partial-pack garbage object")?;
        PackFile::create(
            &store.pack_dir(),
            &[
                (live, b"partial-pack live object".to_vec()),
                (garbage, b"partial-pack garbage object".to_vec()),
            ],
        )?;
        // Drop the loose copies so both objects exist only inside the pack.
        store.delete_blob(&live)?;
        store.delete_blob(&garbage)?;
        assert!(store.has_blob_packed(&live) && store.has_blob_packed(&garbage));
        Ok((dir, store, live, garbage))
    }

    let live_roots = |store: &BlobStore, live: &Hash| live_set(store, std::slice::from_ref(live));

    // Default (rewrite_partial_packs = true): pack is rewritten to {live}.
    let (_d1, store1, live1, garbage1) = build()?;
    let report1 = gc::sweep(&store1, &live_roots(&store1, &live1), delete_options(true))?;
    assert_eq!(
        report1.plan.packs_to_rewrite.len(),
        1,
        "default must schedule a rewrite of the partial pack"
    );
    assert_eq!(report1.packs_rewritten, 1);
    assert_eq!(
        report1.packs_removed, 1,
        "the old pack pair must be removed after the rewrite"
    );
    assert_eq!(report1.loose_removed, 0);
    assert!(
        !store1.has_blob(&garbage1),
        "garbage must be gone after the default rewrite"
    );
    assert_eq!(store1.get_blob(&live1)?, b"partial-pack live object");

    // Configured (rewrite_partial_packs = false): the plan still *lists*
    // the rewrite (planning is option-independent), but the sweep must
    // neither execute it nor remove the old pack.
    let (_d2, store2, live2, garbage2) = build()?;
    let report2 = gc::sweep(&store2, &live_roots(&store2, &live2), delete_options(false))?;
    assert_eq!(
        report2.plan.packs_to_rewrite.len(),
        1,
        "planning is option-independent: the partial pack is still listed"
    );
    assert_eq!(
        report2.packs_rewritten, 0,
        "rewrite_partial_packs = false must not execute any rewrite"
    );
    assert_eq!(report2.packs_removed, 0, "the old pack must stay in place");
    assert!(
        store2.has_blob_packed(&garbage2),
        "garbage must remain packed when rewrites are disabled"
    );
    assert_eq!(store2.get_blob(&live2)?, b"partial-pack live object");
    Ok(())
}

/// Knob: `BlobStore::repack(threshold)`. A threshold above the loose-blob
/// count must be a no-op; a threshold below it must pack every loose
/// blob and empty the loose store.
#[test]
fn repack_threshold_gates_packing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;

    let hashes: Vec<Hash> = ["repack a", "repack b", "repack c"]
        .iter()
        .map(|b| store.put_blob(b.as_bytes()))
        .collect::<Result<_, _>>()?;
    assert_eq!(store.list_blobs()?.len(), 3);

    // High threshold: no-op.
    let packed = store.repack(5)?;
    assert_eq!(packed, 0, "repack above the loose count must pack nothing");
    assert_eq!(store.list_blobs()?.len(), 3, "blobs must remain loose");
    for h in &hashes {
        assert!(!store.has_blob_packed(h));
    }

    // Low threshold: packs all three loose blobs.
    let packed = store.repack(2)?;
    assert_eq!(
        packed, 3,
        "repack below the loose count must pack everything loose"
    );
    assert_eq!(
        store.list_blobs()?.len(),
        0,
        "loose store must be empty after repack"
    );
    assert!(
        store.pack_dir().read_dir()?.next().is_some(),
        "a pack file must exist"
    );
    for (h, expected) in hashes.iter().zip(["repack a", "repack b", "repack c"]) {
        assert!(store.has_blob_packed(h));
        assert_eq!(store.get_blob(h)?, expected.as_bytes().to_vec());
    }
    Ok(())
}
