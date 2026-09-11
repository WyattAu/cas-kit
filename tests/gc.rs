// Tests assert invariants directly; unwraps keep failures loud.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Integration tests for `cas_kit::gc`: mark validation, sweep modes,
//! overlapping-pack rewriting, trash recoverability, orphan cleanup, and
//! sweeps running concurrently with reads.

use std::collections::HashSet;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cas_kit::gc::{self, SweepMode, SweepOptions};
use cas_kit::{BlobStore, Hash, PackFile};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn make_store() -> Result<(tempfile::TempDir, BlobStore), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;
    Ok((dir, store))
}

fn on_disk_path(store: &BlobStore, hash: &Hash) -> std::path::PathBuf {
    let hex = hash.to_hex();
    store.objects_dir().join(&hex[..2]).join(&hex[2..])
}

/// The roots that are physically present — exactly what [`gc::mark`]
/// computes, as a plain set.
fn live_set(store: &BlobStore, roots: &[Hash]) -> HashSet<Hash> {
    let all: HashSet<Hash> = roots.iter().copied().collect();
    let marked = gc::mark(store, &all).unwrap();
    assert!(
        marked.missing.is_empty(),
        "fixture roots must all be present"
    );
    marked.live.iter().copied().collect()
}

fn delete_options() -> SweepOptions {
    SweepOptions {
        mode: SweepMode::Delete,
        ..SweepOptions::default()
    }
}

/// Build two packs sharing one object: P1 = {a, b}, P2 = {b, c}, with no
/// loose copies. Returns the hashes; the tempdir keeps the store alive.
fn overlapping_packs() -> Result<([Hash; 3], tempfile::TempDir), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let store = BlobStore::new_uncompressed(dir.path())?;

    let datas = [b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()];
    let hashes = [
        cas_kit::hash_bytes(&datas[0]),
        cas_kit::hash_bytes(&datas[1]),
        cas_kit::hash_bytes(&datas[2]),
    ];

    PackFile::create(
        &store.pack_dir(),
        &[(hashes[0], datas[0].clone()), (hashes[1], datas[1].clone())],
    )?;
    PackFile::create(
        &store.pack_dir(),
        &[(hashes[1], datas[1].clone()), (hashes[2], datas[2].clone())],
    )?;
    Ok((hashes, dir))
}

#[test]
fn mark_validates_roots_against_presence() -> TestResult {
    let (_dir, store) = make_store()?;
    let a = store.put_blob(b"a")?;
    let missing = Hash::from_hex(&"e".repeat(64))?;

    let roots: HashSet<Hash> = HashSet::from([a, missing]);
    let live = gc::mark(&store, &roots)?;

    assert_eq!(live.live, std::collections::BTreeSet::from([a]));
    assert_eq!(live.missing, vec![missing]);
    assert_eq!(live.roots, 2);
    assert_eq!(live.scanned, 1);
    Ok(())
}

#[test]
fn sweep_removes_only_garbage_loose_objects() -> TestResult {
    let (_dir, store) = make_store()?;
    let live_h = store.put_blob(b"live blob")?;
    let garbage_h = store.put_blob(b"garbage blob")?;

    let live = live_set(&store, &[live_h]);
    let report = gc::sweep(&store, &live, delete_options())?;

    assert_eq!(report.plan.garbage, 1);
    assert_eq!(report.loose_removed, 1);
    assert!(report.bytes_reclaimed > 0);
    assert!(!store.has_blob(&garbage_h));
    assert!(store.has_blob(&live_h));
    assert_eq!(store.get_blob(&live_h)?, b"live blob".to_vec());
    assert!(
        !store.root().join(gc::TRASH_DIR).exists(),
        "delete mode must not create trash"
    );
    Ok(())
}

#[test]
fn dry_run_touches_nothing() -> TestResult {
    let (_dir, store) = make_store()?;
    let live_h = store.put_blob(b"keep")?;
    let g1 = store.put_blob(b"drop one")?;
    let g2 = store.put_blob(b"drop two")?;

    let live = live_set(&store, &[live_h]);
    let report = gc::sweep(&store, &live, SweepOptions::default())?;

    assert!(!report.executed);
    assert_eq!(report.plan.garbage, 2);
    assert_eq!(
        report.plan.bytes_reclaimable,
        report.plan.loose_garbage_bytes
    );
    assert_eq!(report.bytes_reclaimed, 0);
    assert_eq!(report.loose_removed, 0);

    // Nothing changed on disk.
    assert!(store.has_blob(&g1));
    assert!(store.has_blob(&g2));
    assert_eq!(store.blob_count()?, 3);
    assert!(!store.root().join(gc::TRASH_DIR).exists());
    Ok(())
}

#[test]
fn trash_mode_is_recoverable() -> TestResult {
    let (_dir, store) = make_store()?;
    let data = b"recoverable payload";
    let live_h = store.put_blob(b"stays")?;
    let garbage_h = store.put_blob(data)?;

    let live = live_set(&store, &[live_h]);
    let report = gc::sweep(
        &store,
        &live,
        SweepOptions {
            mode: SweepMode::Trash,
            ..SweepOptions::default()
        },
    )?;
    assert!(!store.has_blob(&garbage_h));

    // The blob's bytes are recoverable from the trash mirror.
    let trash = report.trash.as_ref().ok_or("trash dir must be reported")?;
    let hex = garbage_h.to_hex();
    let trashed = trash.join("objects").join(&hex[..2]).join(&hex[2..]);
    assert!(
        trashed.exists(),
        "trashed blob must sit at the mirrored path"
    );
    assert_eq!(fs::read(&trashed)?, data);

    // Restoring = renaming back into objects/; the blob reads again.
    let original = on_disk_path(&store, &garbage_h);
    fs::create_dir_all(original.parent().ok_or("no parent")?)?;
    fs::rename(&trashed, &original)?;
    assert_eq!(store.get_blob(&garbage_h)?, data.to_vec());
    Ok(())
}

#[test]
fn sweep_with_overlapping_packs_rewrites_and_dedups() -> TestResult {
    let (hashes, dir) = overlapping_packs()?;
    let store = BlobStore::new_uncompressed(dir.path())?;
    let [a, b, c] = hashes;
    assert!(store.has_blob_packed(&b), "b must be packed in both packs");

    // b is garbage; a and c are live.
    let live = HashSet::from([a, c]);
    let report = gc::sweep(&store, &live, delete_options())?;

    // Both packs were partial; both rewritten, both old pairs removed.
    assert_eq!(report.plan.packs_with_garbage, 2);
    assert_eq!(report.packs_rewritten, 2);
    assert_eq!(report.packs_removed, 2);
    // Each rewrite kept exactly its unique live object: b is garbage
    // and a/c were covered by nothing else.
    let keeps: Vec<Vec<Hash>> = report
        .plan
        .packs_to_rewrite
        .iter()
        .map(|r| r.keep.clone())
        .collect();
    assert!(
        keeps.contains(&vec![a]) && keeps.contains(&vec![c]),
        "keeps: {keeps:?}"
    );

    assert!(!store.has_blob(&b), "garbage must be gone");
    assert_eq!(store.get_blob(&a)?, b"alpha".to_vec());
    assert_eq!(store.get_blob(&c)?, b"gamma".to_vec());

    // A second sweep converges to a no-op.
    let again = gc::sweep(&store, &live, delete_options())?;
    assert_eq!(again.plan.garbage, 0);
    assert_eq!(again.packs_rewritten, 0);
    Ok(())
}

#[test]
fn fully_garbage_pack_is_removed_not_rewritten() -> TestResult {
    let (_dir, store) = make_store()?;
    let x = store.put_blob(b"dead one")?;
    let y = store.put_blob(b"dead two")?;
    store.repack(0)?;
    let live_h = store.put_blob(b"live loose")?;

    let live = live_set(&store, &[live_h]);
    let report = gc::sweep(&store, &live, delete_options())?;

    assert_eq!(report.plan.packs_with_garbage, 1);
    assert_eq!(
        report.plan.packs_to_rewrite.len(),
        0,
        "nothing live in pack"
    );
    assert_eq!(report.packs_removed, 1);
    assert!(
        report.plan.bytes_reclaimable > 0,
        "full pack bytes count as reclaimable"
    );
    assert!(!store.has_blob(&x) && !store.has_blob(&y));
    assert_eq!(store.get_blob(&live_h)?, b"live loose".to_vec());
    Ok(())
}

#[test]
fn redundant_partial_pack_is_removed_without_rewrite() -> TestResult {
    // Pack P = {a, b} where a also exists loose and b is garbage: the
    // live contents are covered by the loose copy, so the pack goes
    // away without a rewrite.
    let (_dir, store) = make_store()?;
    let a = store.put_blob(b"covered loosely")?;
    let b = store.put_blob(b"packed garbage")?;
    let a_data = store.get_blob(&a)?;
    PackFile::create(
        &store.pack_dir(),
        &[(a, a_data.clone()), (b, b"packed garbage".to_vec())],
    )?;

    let live = live_set(&store, &[a]);
    let report = gc::sweep(&store, &live, delete_options())?;

    assert_eq!(report.plan.packs_to_rewrite.len(), 0);
    assert_eq!(report.packs_removed, 1);
    assert!(!store.has_blob_packed(&a), "redundant pack must be gone");
    assert!(
        store.has_blob(&a),
        "live object survives via its loose copy"
    );
    assert_eq!(store.get_blob(&a)?, a_data);
    assert!(!store.has_blob(&b));
    Ok(())
}

#[test]
fn pack_rewrite_preserves_live_objects_byte_for_byte() -> TestResult {
    // A partial pack mixing live and garbage objects: the rewrite must
    // keep every live object readable and drop every garbage one.
    let (_dir, store) = make_store()?;
    let mut live_roots: Vec<Hash> = Vec::new();
    let mut expected: Vec<(Hash, Vec<u8>)> = Vec::new();
    let mut garbage = Vec::new();
    for i in 0..6 {
        let data = format!("rewrite victim {i}").into_bytes();
        let hash = store.put_blob(&data)?;
        if i % 2 == 0 {
            live_roots.push(hash);
            expected.push((hash, data));
        } else {
            garbage.push(hash);
        }
    }
    store.repack(0)?;

    let live = live_set(&store, &live_roots);
    let report = gc::sweep(
        &store,
        &live,
        SweepOptions {
            mode: SweepMode::Trash,
            ..SweepOptions::default()
        },
    )?;

    assert_eq!(report.packs_rewritten, 1);
    assert_eq!(report.plan.packed_garbage_objects, 3);
    for (hash, data) in &expected {
        assert_eq!(
            &store.get_blob(hash)?,
            data,
            "live object must survive the rewrite"
        );
    }
    for hash in &garbage {
        assert!(!store.has_blob(hash));
    }
    Ok(())
}

#[test]
fn orphan_pack_files_are_swept() -> TestResult {
    let (_dir, store) = make_store()?;
    let live_h = store.put_blob(b"anchor")?;
    fs::create_dir_all(store.pack_dir())?;

    let stray_pack = store.pack_dir().join("pack-deadbeef.pack");
    let stray_pack_bytes = b"junk bytes without an index".to_vec();
    fs::write(&stray_pack, &stray_pack_bytes)?;
    let stray_idx = store.pack_dir().join("pack-feedface.idx");
    let stray_idx_bytes = b"index without a pack".to_vec();
    fs::write(&stray_idx, &stray_idx_bytes)?;

    let live = live_set(&store, &[live_h]);
    let plan = gc::plan_sweep(&store, &live)?;
    assert_eq!(plan.orphan_files.len(), 2);
    assert_eq!(plan.scanned, 1, "orphans are not objects");

    let report = gc::sweep(&store, &live, delete_options())?;
    assert!(!stray_pack.exists() && !stray_idx.exists());
    let expected: u64 = (stray_pack_bytes.len() + stray_idx_bytes.len()) as u64;
    assert_eq!(report.bytes_reclaimed, expected);
    assert_eq!(store.get_blob(&live_h)?, b"anchor".to_vec());
    Ok(())
}

#[test]
fn unreadable_pack_is_never_touched() -> TestResult {
    let (_dir, store) = make_store()?;
    let _a = store.put_blob(b"inside corrupt pack")?;
    store.repack(0)?;
    let live_h = store.put_blob(b"loose live")?;

    // Corrupt the pack's index: contents unknown, so sweep must skip
    // the pack entirely instead of gambling on its hashes.
    let mut packs = PackFile::list_packs(&store.pack_dir())?;
    let pack_path = packs.pop().ok_or("expected one pack")?;
    let idx_path = pack_path.with_extension("idx");
    let mut raw = fs::read(&idx_path)?;
    raw[0..4].copy_from_slice(b"XXXX");
    fs::write(&idx_path, &raw)?;

    // The live set here is just the loose blob: gc::mark correctly
    // surfaces the corrupt index as an error (list_blobs_packed goes
    // through the pack cache), which callers must resolve before
    // sweeping. plan_sweep/sweep take the conservative path instead.
    let live = HashSet::from([live_h]);
    let report = gc::sweep(&store, &live, delete_options())?;

    assert_eq!(report.plan.unreadable_packs, vec![pack_path.clone()]);
    assert!(
        pack_path.exists() && idx_path.exists(),
        "unreadable pack must be preserved"
    );
    assert_eq!(store.get_blob(&live_h)?, b"loose live".to_vec());
    Ok(())
}

#[test]
fn sweep_concurrent_with_reads_of_live_objects() -> TestResult {
    let dir = tempfile::tempdir()?;
    let store = Arc::new(BlobStore::new_uncompressed(dir.path())?);

    let mut live = Vec::new();
    let mut garbage = Vec::new();
    for i in 0..8 {
        let data = format!("concurrent blob {i}").into_bytes();
        let hash = store.put_blob(&data)?;
        if i % 2 == 0 {
            live.push((hash, data));
        } else {
            garbage.push(hash);
        }
    }

    // Readers hammer live blobs while the sweep runs; they must never
    // observe a failure — sweep never touches a live object.
    type ReadResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
    let mut handles: Vec<std::thread::JoinHandle<ReadResult>> = Vec::new();
    for (hash, data) in live.clone() {
        let store = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                let got = store.get_blob(&hash)?;
                if got != data {
                    return Err(format!("read returned wrong bytes for {hash:?}").into());
                }
            }
            Ok(())
        }));
    }

    let sweeper = {
        let store = Arc::clone(&store);
        let live_set: HashSet<Hash> = live.iter().map(|(h, _)| *h).collect();
        std::thread::spawn(move || gc::sweep(&store, &live_set, delete_options()))
    };

    let report = sweeper
        .join()
        .map_err(|e| format!("sweeper panicked: {e:?}"))??;
    assert_eq!(report.loose_removed, 4);

    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(format!("reader failed: {e}").into()),
            Err(e) => return Err(format!("reader panicked: {e:?}").into()),
        }
    }
    for hash in &garbage {
        assert!(!store.has_blob(hash));
    }
    for (hash, data) in &live {
        assert_eq!(&store.get_blob(hash)?, data);
    }
    Ok(())
}

#[test]
fn mark_then_sweep_full_cycle_leaves_exactly_the_live_set() -> TestResult {
    let (_dir, store) = make_store()?;
    let mut hashes = Vec::new();
    for i in 0..10 {
        hashes.push(store.put_blob(format!("cycle {i}").as_bytes())?);
    }
    let roots: HashSet<Hash> = hashes[..6].iter().copied().collect();

    let live = gc::mark(&store, &roots)?;
    assert!(live.missing.is_empty());
    let live_std: HashSet<Hash> = live.live.iter().copied().collect();
    gc::sweep(&store, &live_std, delete_options())?;

    let remaining: HashSet<Hash> = store.list_blobs()?.into_iter().collect();
    assert_eq!(remaining, live_std, "exactly the live set must remain");
    assert_eq!(store.blob_count()?, 6);
    Ok(())
}

#[cfg(feature = "tokio")]
#[test]
fn async_wrappers_match_sync_behavior() -> TestResult {
    let (_dir, store) = make_store()?;
    let live_h = store.put_blob(b"async live")?;
    let garbage_h = store.put_blob(b"async garbage")?;
    let store = Arc::new(store);

    let roots: HashSet<Hash> = HashSet::from([live_h]);
    let rt = tokio::runtime::Builder::new_current_thread().build()?;
    // Errors are stringified at the async boundary: `Box<dyn Error +
    // Send + Sync>` does not convert into this test's `Box<dyn Error>`.
    rt.block_on(async {
        let live = gc::mark_async(Arc::clone(&store), roots)
            .await
            .map_err(|e| e.to_string())?;
        assert_eq!(live.live.len(), 1);
        let live_set: HashSet<Hash> = live.live.iter().copied().collect();
        let report = gc::sweep_async(
            Arc::clone(&store),
            live_set,
            SweepOptions {
                mode: SweepMode::Delete,
                ..SweepOptions::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        assert_eq!(report.loose_removed, 1);
        Ok::<(), String>(())
    })?;

    assert!(!store.has_blob(&garbage_h));
    assert!(store.has_blob(&live_h));
    Ok(())
}
