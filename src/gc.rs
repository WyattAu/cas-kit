// SPDX-License-Identifier: MIT OR Apache-2.0
//! Mark–sweep garbage collection for the blob store.
//!
//! # The reference model (read this first)
//!
//! cas-kit objects are **opaque blobs**: an object never references another
//! object. There are no tree objects and no embedded pointers; the pack
//! manifest (`.idx`) is a flat digest → offset map and expresses storage
//! location, not reachability. Reachability therefore cannot be derived
//! from store contents — the **host application owns the reference graph**
//! (its manifests, message attachments, index rows, ...).
//!
//! Garbage collection is consequently an honest **set difference**:
//!
//! 1. **Mark** ([`mark`]) — the caller supplies every hash it still
//!    considers live (the roots). There is no transitive closure to walk:
//!    the mark phase validates the root set against what is physically
//!    present (loose files ∪ packed objects) and reports roots that
//!    resolve to nothing. Roots *are* the live set.
//! 2. **Plan** ([`plan_sweep`]) — enumerate everything on disk, classify
//!    `present − live` as garbage, and decide per pack whether it can be
//!    removed outright or must be rewritten without its garbage (see
//!    [Pack handling](#pack-handling)).
//! 3. **Sweep** ([`sweep`]) — execute a plan in one of three modes:
//!    [`SweepMode::DryRun`] (change nothing), [`SweepMode::Trash`]
//!    (move garbage to `<root>/trash/`, recoverable) or
//!    [`SweepMode::Delete`] (unlink permanently).
//!
//! If your application has internal references (e.g. a manifest blob
//! listing chunk hashes), expand your roots to the full transitive closure
//! *before* calling [`mark`] — the store cannot walk your graph for you.
//!
//! # Pack handling
//!
//! Packs are immutable and have no per-blob delete, so packed garbage is
//! reclaimed at **pack granularity** using a coverage rule:
//!
//! - A pack whose objects are *all* garbage is **removed** (.idx first,
//!   then .pack).
//! - A pack with no garbage is left untouched, and its objects count as
//!   *covered* (they will survive elsewhere no matter what).
//! - A *partial* pack (some garbage) is **rewritten** with only the live
//!   objects it uniquely covers — `needed = live-in-pack − covered`, where
//!   `covered` starts as live loose objects plus all untouched packs. If
//!   `needed` is empty (every live object in the pack is also loose or in
//!   a healthy pack) the pack is removed without a rewrite; otherwise a
//!   new content-named pack is created holding exactly `needed`, and the
//!   old pair is removed. The rewrite is refcount-aware: an object that
//!   survives in some other location is not copied into a new pack.
//!
//! Rewrites are ordered by pack path and the covered set grows as they
//! proceed, so two overlapping partial packs never both copy the same
//! object. Set `SweepOptions::rewrite_partial_packs` to `false` to leave
//! all partial packs untouched (loose garbage is still reclaimed).
//!
//! # Crash safety
//!
//! Every destructive step is a single file operation, and phases are
//! ordered so a crash can lose *garbage reclamation*, never *live data*:
//!
//! 1. **Rewrites run first and are additive**: the replacement pack is
//!    fully written (`.pack` then `.idx`) before any old file is removed.
//!    A crash here leaves an extra pack — harmless duplicate storage that
//!    the next sweep reclaims (identical object sets produce identical
//!    content-derived pack names, so re-running converges). One narrow
//!    window exists: if the replacement pack's name already exists from
//!    an earlier interrupted run, its `.pack` is truncated and rewritten
//!    in place; a crash *mid-write* leaves a torn pack whose objects may
//!    transiently fail reads with `HashMismatch`. No data is lost — the
//!    old packs are only removed after the new pair is fully written —
//!    and re-running the sweep repairs the torn pack.
//! 2. **Loose deletions** unlink (or trash) one immutable file per call.
//!    A crash mid-loop leaves the remaining garbage for the next sweep.
//! 3. **Pack removals** delete the `.idx` before the `.pack`; a crash
//!    between the two leaves an orphan `.pack` that no reader will ever
//!    open (packs without an index are skipped by [`crate::PackCache`])
//!    and that the next sweep collects via the plan's orphan list.
//! 4. **Trash mode** moves files with `rename(2)` *within the store root*
//!    — atomic per file on POSIX. A crash mid-sweep leaves some garbage
//!    in `<root>/trash/` and the rest in place; either way the store is
//!    consistent and a later sweep (or a manual restore) finishes the job.
//!
//! # Concurrency
//!
//! - **Reads of live objects are safe throughout a sweep**: sweep never
//!   touches a file whose hash is in the live set. (POSIX: an in-flight
//!   `read` on an unlinked file keeps its fd; Windows: the unlink may fail
//!   with a sharing violation, surfacing as an I/O error — retry the
//!   sweep.)
//! - **Writers racing the sweep**: if a host re-puts a blob between
//!   enumeration and deletion, the sweep will delete the fresh copy (the
//!   address was classified garbage before the re-put). Quiesce writers
//!   with the same application-level lock your writers use, or re-run
//!   mark + sweep until the report is stable.
//! - **Transient read errors during pack rewrites**: a reader holding a
//!   stale in-memory pack index can hit a pack that was just removed;
//!   live objects remain readable via their other copies once the reader
//!   reloads its cache ([`crate::BlobStore::invalidate_pack_cache`] is
//!   called in-process; other processes see fresh state on next open).
//!   Retry the read, or schedule sweeps for windows where readers can
//!   tolerate a transient `BlobNotFound`.
//!
//! # Recoverability (trash mode)
//!
//! [`SweepMode::Trash`] mirrors the removed relative paths under
//! `<root>/trash/` (`trash/objects/<2-hex>/<62-hex>`,
//! `trash/objects/pack/pack-<hex>.pack|.idx`). Restoring is a reverse
//! rename of the mirrored path; a colliding trash entry (same hash
//! trashed twice) is overwritten with the newer copy. The trash directory
//! is outside `objects/`, so the store never reads from it — leftover
//! trash costs disk space only. Empty it once you are confident no
//! restore is needed.
//!
//! # Async
//!
//! The module is synchronous, matching the rest of the crate (filesystem
//! operations dominate GC and blocking is the honest model). With the
//! optional `tokio` feature, [`mark_async`] and [`sweep_async`] run the
//! same code on the blocking thread pool; they take `Arc<BlobStore>` so
//! the store can be moved into the spawned task.
//!
//! # Example
//!
//! ```no_run
//! use std::collections::HashSet;
//!
//! use cas_kit::gc::{self, SweepMode, SweepOptions};
//! use cas_kit::{BlobStore, Hash};
//!
//! # fn main() -> Result<(), cas_kit::CasError> {
//! let store = BlobStore::new("/tmp/my-store")?;
//!
//! // The host decides what is live (here: one blob, in general the whole
//! // transitive closure of your reference graph).
//! let roots: HashSet<Hash> = HashSet::from([store.put_blob(b"keep me")?]);
//! store.put_blob(b"garbage")?;
//!
//! let live = gc::mark(&store, &roots)?;
//! assert!(live.missing.is_empty());
//!
//! let live_set: HashSet<Hash> = live.live.iter().copied().collect();
//! let report = gc::sweep(&store, &live_set, SweepOptions {
//!     mode: SweepMode::Trash,
//!     ..SweepOptions::default()
//! })?;
//! assert_eq!(report.plan.garbage, 1);
//! assert_eq!(report.loose_removed, 1);
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "tokio")]
use std::sync::Arc;

use crate::error::CasError;
use crate::hash::Hash;
use crate::pack::{PackFile, PackIndex};
use crate::store::BlobStore;

/// Name of the trash directory created under the store root when a sweep
/// runs in [`SweepMode::Trash`] mode. Mirrored paths under this directory
/// restore by renaming back into `objects/`.
pub const TRASH_DIR: &str = "trash";

/// Result of the mark phase: the validated live set.
///
/// Objects are opaque (see the [module docs](self)), so "live" means
/// exactly "a root the host supplied that is physically present in the
/// store".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSet {
    /// Roots present in the store — the live set to sweep against.
    pub live: BTreeSet<Hash>,
    /// Roots absent from the store (neither loose nor packed), sorted.
    /// Missing roots are reported, never fatal: they may indicate a
    /// stale host index or a blob that was already swept.
    pub missing: Vec<Hash>,
    /// Number of roots supplied by the host.
    pub roots: usize,
    /// Distinct objects physically present (loose ∪ packed).
    pub scanned: usize,
}

/// Validate the host's live set against what the store physically holds.
///
/// See the [module docs](self) for why the roots *are* the live set and
/// no reference walk happens here.
pub fn mark(store: &BlobStore, roots: &HashSet<Hash>) -> Result<LiveSet, CasError> {
    let mut present: HashSet<Hash> = store.list_blobs()?.into_iter().collect();
    present.extend(store.list_blobs_packed()?);

    let mut live = BTreeSet::new();
    let mut missing = Vec::new();
    for hash in roots {
        if present.contains(hash) {
            live.insert(*hash);
        } else {
            missing.push(*hash);
        }
    }
    missing.sort();

    Ok(LiveSet {
        live,
        missing,
        roots: roots.len(),
        scanned: present.len(),
    })
}

/// What to do with garbage during [`sweep`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SweepMode {
    /// Enumerate and classify only; change nothing on disk.
    #[default]
    DryRun,
    /// Move garbage to `<root>/trash/` (atomic rename within the store
    /// root), preserving relative paths for recovery.
    Trash,
    /// Unlink garbage permanently.
    Delete,
}

/// Options for [`sweep`].
#[derive(Clone, Copy, Debug)]
pub struct SweepOptions {
    /// What to do with garbage. Default: [`SweepMode::DryRun`].
    pub mode: SweepMode,
    /// Rewrite partial packs to drop their garbage (see the
    /// [module docs](self#pack-handling)). When `false`, garbage inside
    /// otherwise-live packs is left in place. Default: `true`.
    pub rewrite_partial_packs: bool,
}

impl Default for SweepOptions {
    fn default() -> Self {
        Self {
            mode: SweepMode::DryRun,
            rewrite_partial_packs: true,
        }
    }
}

/// A pack scheduled to be rewritten without some of its objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackRewrite {
    /// Path of the pack being rewritten.
    pub pack: PathBuf,
    /// Objects the replacement pack will contain: the pack's live
    /// objects that are not covered by a loose copy or a healthy pack,
    /// sorted.
    pub keep: Vec<Hash>,
}

/// The result of enumerating the store and classifying garbage.
///
/// Produced by [`plan_sweep`]; embedded in [`SweepReport`]. Byte counts
/// are on-disk sizes (compressed bytes for zstd builds).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// Distinct objects physically present (loose ∪ packed).
    pub scanned: usize,
    /// Objects with a loose copy.
    pub loose_present: usize,
    /// Distinct objects with at least one packed copy.
    pub packed_present: usize,
    /// Live objects actually present (live ∩ scanned).
    pub live: usize,
    /// Garbage objects: `scanned − live`, the sweep target.
    pub garbage: usize,
    /// Garbage objects with a loose copy, sorted. Each entry is one
    /// removable file.
    pub loose_garbage: Vec<Hash>,
    /// On-disk bytes of [`Self::loose_garbage`] files.
    pub loose_garbage_bytes: u64,
    /// Packs containing at least one garbage object (before the
    /// coverage rule decides remove-vs-rewrite).
    pub packs_with_garbage: usize,
    /// Garbage objects with at least one packed copy, counted once per
    /// pack that holds them (a hash packed twice counts twice, and a
    /// hash with both loose and packed garbage copies is counted here
    /// *and* in [`Self::loose_garbage`]). [`Self::garbage`] remains the
    /// distinct total.
    pub packed_garbage_objects: usize,
    /// Packs removed outright (all garbage, or live contents fully
    /// covered elsewhere), sorted.
    pub packs_to_remove: Vec<PathBuf>,
    /// Packs to be rewritten without their garbage.
    pub packs_to_rewrite: Vec<PackRewrite>,
    /// Files in the pack directory that cannot ever be read: a `.pack`
    /// without `.idx` or an `.idx` without a `.pack`. Swept like garbage.
    pub orphan_files: Vec<PathBuf>,
    /// Packs whose `.idx` failed to load. Their contents are unknown, so
    /// they are **never touched** — deleting one could destroy live data.
    /// Investigate manually.
    pub unreadable_packs: Vec<PathBuf>,
    /// Exact on-disk bytes reclaimable by deleting loose garbage and
    /// removed packs. Pack *rewrites* are excluded: their savings depend
    /// on recompression and are only known after execution (see
    /// [`SweepReport::bytes_reclaimed`]).
    pub bytes_reclaimable: u64,
}

/// Outcome of an executed (or dry-run) sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepReport {
    /// The plan that was (or would be) executed.
    pub plan: SweepPlan,
    /// The mode the sweep ran in.
    pub mode: SweepMode,
    /// Whether any filesystem change was made (`false` for dry runs).
    pub executed: bool,
    /// Loose garbage files removed.
    pub loose_removed: usize,
    /// Packs removed entirely (full-garbage, coverage-redundant, and the
    /// old pairs of rewritten packs).
    pub packs_removed: usize,
    /// Packs rewritten without their garbage.
    pub packs_rewritten: usize,
    /// On-disk bytes no longer under `objects/`: unlinked in delete
    /// mode, moved to trash in trash mode. For rewrites this is the old
    /// pair size minus the new pair size.
    pub bytes_reclaimed: u64,
    /// Trash directory when mode was [`SweepMode::Trash`], else `None`.
    pub trash: Option<PathBuf>,
}

/// A pack file on disk with its parsed index and classification.
struct PackState {
    pack_path: PathBuf,
    hashes: BTreeSet<Hash>,
    garbage: BTreeSet<Hash>,
    bytes: u64,
}

/// Enumerate the store, classify garbage, and decide pack handling
/// without touching the filesystem.
///
/// Packs whose index fails to load are reported in
/// [`SweepPlan::unreadable_packs`] and never deleted; everything else
/// proceeds. See the [module docs](self) for the coverage rule.
pub fn plan_sweep(store: &BlobStore, live: &HashSet<Hash>) -> Result<SweepPlan, CasError> {
    let loose = store.list_blobs()?;
    let loose_set: BTreeSet<Hash> = loose.iter().copied().collect();

    // Fresh inventory, not the store's lazy pack cache: the cache may be
    // stale relative to disk, and a sweep must classify what is actually
    // present.
    let pack_dir = store.pack_dir();
    let mut packs: Vec<PackState> = Vec::new();
    let mut orphan_files: Vec<PathBuf> = Vec::new();
    let mut unreadable_packs: Vec<PathBuf> = Vec::new();

    for pack_path in PackFile::list_packs(&pack_dir)? {
        let idx_path = pack_path.with_extension("idx");
        if !idx_path.exists() {
            orphan_files.push(pack_path);
            continue;
        }
        match PackIndex::load(&idx_path) {
            Ok(index) => {
                let hashes: BTreeSet<Hash> = index.hashes().into_iter().collect();
                let garbage: BTreeSet<Hash> = hashes
                    .iter()
                    .copied()
                    .filter(|h| !live.contains(h))
                    .collect();
                let bytes = file_len(&pack_path)? + file_len(&idx_path)?;
                packs.push(PackState {
                    pack_path,
                    hashes,
                    garbage,
                    bytes,
                });
            }
            Err(_) => unreadable_packs.push(pack_path),
        }
    }
    // An .idx without its .pack can never be read either.
    let pack_names: Vec<PathBuf> = packs.iter().map(|p| p.pack_path.clone()).collect();
    if pack_dir.exists() {
        for entry in fs::read_dir(&pack_dir)? {
            let entry = entry?;
            let path = entry.path();
            let is_idx = entry.file_name().to_string_lossy().ends_with(".idx");
            if is_idx {
                let pack = path.with_extension("pack");
                if !pack_names.contains(&pack) && !pack.exists() {
                    orphan_files.push(path);
                }
            }
        }
    }
    orphan_files.sort();

    // Distinct present set and garbage classification.
    let packed_hashes: HashSet<Hash> = packs
        .iter()
        .flat_map(|p| p.hashes.iter().copied())
        .collect();
    let packed_present = packed_hashes.len();
    let scanned_set: HashSet<Hash> = loose_set.iter().copied().chain(packed_hashes).collect();
    let garbage_set: BTreeSet<Hash> = scanned_set
        .iter()
        .copied()
        .filter(|h| !live.contains(h))
        .collect();

    let loose_garbage: Vec<Hash> = loose_set
        .iter()
        .copied()
        .filter(|h| garbage_set.contains(h))
        .collect();
    let mut loose_garbage_bytes = 0u64;
    for hash in &loose_garbage {
        loose_garbage_bytes += file_len(&loose_path(store, hash))?;
    }

    let packed_garbage_objects = packs.iter().map(|p| p.garbage.len()).sum::<usize>();

    // Pack decisions under the coverage rule (see module docs).
    let mut covered: HashSet<Hash> = loose_set
        .iter()
        .copied()
        .filter(|h| !garbage_set.contains(h))
        .collect();
    let mut packs_to_remove = Vec::new();
    let mut packs_to_rewrite = Vec::new();
    let mut remove_bytes = 0u64;
    let mut packs_with_garbage = 0usize;

    for pack in &packs {
        if pack.garbage.is_empty() {
            covered.extend(pack.hashes.iter().copied());
        }
    }
    for pack in &packs {
        if pack.garbage.is_empty() {
            continue;
        }
        packs_with_garbage += 1;
        let needed: Vec<Hash> = pack
            .hashes
            .iter()
            .copied()
            .filter(|h| !garbage_set.contains(h) && !covered.contains(h))
            .collect();
        if needed.is_empty() {
            packs_to_remove.push(pack.pack_path.clone());
            remove_bytes += pack.bytes;
        } else {
            // The replacement pack will hold exactly `needed`; those
            // objects are covered for later packs in this run.
            covered.extend(needed.iter().copied());
            packs_to_rewrite.push(PackRewrite {
                pack: pack.pack_path.clone(),
                keep: needed,
            });
        }
    }

    let live_present = scanned_set.iter().filter(|h| live.contains(h)).count();

    Ok(SweepPlan {
        scanned: scanned_set.len(),
        loose_present: loose_set.len(),
        packed_present,
        live: live_present,
        garbage: garbage_set.len(),
        loose_garbage,
        loose_garbage_bytes,
        packs_with_garbage,
        packed_garbage_objects,
        packs_to_remove,
        packs_to_rewrite,
        orphan_files,
        unreadable_packs,
        bytes_reclaimable: loose_garbage_bytes + remove_bytes,
    })
}

/// Sweep the store: remove everything not in `live`, according to
/// [`SweepOptions::mode`].
///
/// A dry run is exactly [`plan_sweep`] plus a zeroed report. Execution
/// order is crash-safe (see the [module docs](self)): pack rewrites
/// first (additive), then loose deletions, then orphan and pack removals.
/// The store's pack cache is invalidated afterwards so in-process reads
/// observe the new pack layout.
pub fn sweep(
    store: &BlobStore,
    live: &HashSet<Hash>,
    options: SweepOptions,
) -> Result<SweepReport, CasError> {
    let plan = plan_sweep(store, live)?;

    if options.mode == SweepMode::DryRun {
        return Ok(SweepReport {
            plan,
            mode: options.mode,
            executed: false,
            loose_removed: 0,
            packs_removed: 0,
            packs_rewritten: 0,
            bytes_reclaimed: 0,
            trash: None,
        });
    }

    let trash_root = store.root().join(TRASH_DIR);
    let mut loose_removed = 0usize;
    let mut packs_removed = 0usize;
    let mut packs_rewritten = 0usize;
    let mut bytes_reclaimed = 0u64;

    // Phase 1: pack rewrites — purely additive. New packs are fully
    // written before any old file is removed (phase 4), so a crash here
    // only leaves duplicate storage that the next sweep reclaims.
    if options.rewrite_partial_packs {
        for rewrite in &plan.packs_to_rewrite {
            let idx_path = rewrite.pack.with_extension("idx");
            let index = PackIndex::load(&idx_path)?;
            let mut objects = Vec::with_capacity(rewrite.keep.len());
            for hash in &rewrite.keep {
                let data = PackFile::read_blob(&rewrite.pack, &index, hash)?;
                objects.push((*hash, data));
            }
            // Deterministic content-derived name: identical keep-sets
            // converge to the same pack across sweeps.
            let (new_pack, new_idx) = PackFile::create(&store.pack_dir(), &objects)?;
            let old_bytes = file_len(&rewrite.pack)? + file_len(&idx_path)?;
            let new_bytes = file_len(&new_pack)? + file_len(&new_idx)?;
            bytes_reclaimed += old_bytes.saturating_sub(new_bytes);
            packs_rewritten += 1;
        }
    }

    // Phase 2: loose garbage — one immutable file per call.
    for hash in &plan.loose_garbage {
        bytes_reclaimed +=
            remove_or_trash(store, options.mode, &trash_root, &loose_path(store, hash))?;
        loose_removed += 1;
    }

    // Phase 3: orphaned pack-directory files.
    for path in &plan.orphan_files {
        bytes_reclaimed += remove_or_trash(store, options.mode, &trash_root, path)?;
    }

    // Phase 4: pack removals — full-garbage packs, coverage-redundant
    // packs, and the old pairs of rewritten packs. The .idx goes first:
    // a crash between the two leaves an unreadable orphan .pack that the
    // next sweep collects.
    let mut removals: Vec<PathBuf> = plan.packs_to_remove.clone();
    if options.rewrite_partial_packs {
        removals.extend(plan.packs_to_rewrite.iter().map(|r| r.pack.clone()));
    }
    for pack_path in &removals {
        let idx_path = pack_path.with_extension("idx");
        for path in [idx_path, pack_path.clone()] {
            if path.exists() {
                bytes_reclaimed += remove_or_trash(store, options.mode, &trash_root, &path)?;
            }
        }
        packs_removed += 1;
    }

    // In-process readers must observe the new pack layout.
    store.invalidate_pack_cache();

    Ok(SweepReport {
        plan,
        mode: options.mode,
        executed: true,
        loose_removed,
        packs_removed,
        packs_rewritten,
        bytes_reclaimed,
        trash: match options.mode {
            SweepMode::Trash => Some(trash_root),
            _ => None,
        },
    })
}

/// [`mark`] on the blocking thread pool (requires the `tokio` feature).
///
/// The store is moved into the spawned task, hence the `Arc`.
#[cfg(feature = "tokio")]
pub async fn mark_async(store: Arc<BlobStore>, roots: HashSet<Hash>) -> Result<LiveSet, CasError> {
    tokio::task::spawn_blocking(move || mark(&store, &roots))
        .await
        .map_err(|e| CasError::TaskJoin(e.to_string()))?
}

/// [`sweep`] on the blocking thread pool (requires the `tokio` feature).
///
/// The store is moved into the spawned task, hence the `Arc`.
#[cfg(feature = "tokio")]
pub async fn sweep_async(
    store: Arc<BlobStore>,
    live: HashSet<Hash>,
    options: SweepOptions,
) -> Result<SweepReport, CasError> {
    tokio::task::spawn_blocking(move || sweep(&store, &live, options))
        .await
        .map_err(|e| CasError::TaskJoin(e.to_string()))?
}

/// Path of the loose file backing `hash` (mirrors `BlobStore::blob_path`).
fn loose_path(store: &BlobStore, hash: &Hash) -> PathBuf {
    let hex = hash.to_hex();
    store.objects_dir().join(&hex[..2]).join(&hex[2..])
}

/// On-disk length of a regular file.
fn file_len(path: &Path) -> Result<u64, CasError> {
    Ok(fs::metadata(path)?.len())
}

/// Delete `path`, or move it into the trash mirror. Returns the on-disk
/// bytes no longer under `objects/`.
///
/// Never called in dry-run mode (sweep returns early).
fn remove_or_trash(
    store: &BlobStore,
    mode: SweepMode,
    trash_root: &Path,
    path: &Path,
) -> Result<u64, CasError> {
    let len = file_len(path)?;
    match mode {
        SweepMode::Delete => fs::remove_file(path)?,
        SweepMode::Trash => {
            let relative = path
                .strip_prefix(store.root())
                .map_err(|_| CasError::GcPathEscape(path.display().to_string()))?;
            let destination = trash_root.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            // A previous sweep may already have trashed this address;
            // the newer copy wins (both were garbage).
            if destination.exists() {
                let _ = fs::remove_file(&destination);
            }
            fs::rename(path, &destination)?;
        }
        SweepMode::DryRun => unreachable!("dry-run sweeps never reach deletion"),
    }
    Ok(len)
}
