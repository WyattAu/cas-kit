// Zero-allocation gate for the dedup-hit ingest path: a counting global
// allocator proves a re-ingest of already-stored content never allocates
// (or writes) anything proportional to the blob size.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! Allocation counter tests for `BlobStore::put_blob` dedup hits.
//!
//! The README claims a second `put_blob` of identical bytes "sees the file
//! already exists and returns immediately" — a no-op re-store. This binary
//! is the verification (a counting allocator cannot lie about code
//! reading): the hit path must stay within a small constant allocation
//! budget (the hex-path `PathBuf`/`String` plumbing), *independent of the
//! blob size*. If the hit path ever grew a compression buffer or re-wrote
//! the blob, the byte counter would scale with the payload and fail here.
//!
//! The iai-callgrind instruction-count gate (CI-only; requires valgrind,
//! `benches/iai_cas.rs::put_hit`) pins the cycle cost; this file pins the
//! heap behavior on every `cargo test` run.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use cas_kit::BlobStore;
use tempfile::TempDir;

static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // realloc grows shrink the buffer: track the signed delta.
        if new_size >= layout.size() {
            ALLOCATED_BYTES.fetch_add(new_size - layout.size(), Ordering::Relaxed);
        } else {
            ALLOCATED_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocated_bytes() -> usize {
    ALLOCATED_BYTES.load(Ordering::Relaxed)
}

fn allocations() -> usize {
    ALLOCATIONS.load(Ordering::Relaxed)
}

/// Deterministic splitmix64 fill (same generator as the benches), so the
/// payload is incompressible: a re-store would allocate a compression
/// buffer at least as large as the input.
fn gen_data(size: usize, seed: u64) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    let mut state = seed;
    for chunk in buf.chunks_mut(8) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut word = state;
        word = (word ^ (word >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        word = (word ^ (word >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        word ^= word >> 31;
        for (i, byte) in chunk.iter_mut().enumerate() {
            *byte = (word >> (i * 8)) as u8;
        }
    }
    buf
}

/// One sequential test: the allocation counter is process-global, so
/// parallel test threads would pollute each other's counts.
#[test]
fn dedup_hit_allocation_claims() {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();

    // --- Counter sanity guard. With `zstd` (default), a cold put of
    // incompressible content allocates content-proportionally (compression
    // buffer + write path); without it, the cold put streams raw and stays
    // in the constant budget. Either way the guard fails loudly if the
    // counter is broken, so the assertions below prove something. ---
    let blob_1mib = gen_data(1024 * 1024, 1);
    let before = allocated_bytes();
    store.put_blob(&blob_1mib).unwrap();
    let cold_bytes = allocated_bytes() - before;
    #[cfg(feature = "zstd")]
    assert!(
        cold_bytes >= 1024 * 1024,
        "cold put of incompressible 1 MiB must allocate at least the payload \
         (allocated {cold_bytes} bytes — counter sanity check)"
    );
    #[cfg(not(feature = "zstd"))]
    assert!(
        cold_bytes < 4096,
        "raw cold put (no zstd) must not allocate content-proportionally \
         (allocated {cold_bytes} bytes)"
    );

    // The counting allocator is live: a fresh 1 MiB test-side buffer must
    // move the byte counter by at least its size.
    let before = allocated_bytes();
    let probe = vec![0u8; 1024 * 1024];
    assert!(
        allocated_bytes() - before >= 1024 * 1024,
        "counting allocator must count allocations (counter sanity check)"
    );
    drop(probe);

    // --- Dedup hit: a bounded handful of small allocations (hex path
    // plumbing), never a content-sized buffer. ---
    let before = allocated_bytes();
    let hits_before = allocations();
    assert_eq!(
        store.put_blob(&blob_1mib).unwrap(),
        cas_kit::hash_bytes(&blob_1mib)
    );
    let hit_bytes = allocated_bytes() - before;
    let hit_count = allocations() - hits_before;
    assert!(
        hit_bytes < 4096,
        "dedup hit must not allocate content-proportionally \
         (1 MiB blob hit allocated {hit_bytes} bytes)"
    );
    assert!(
        hit_count < 32,
        "dedup hit allocation count must be a small constant (got {hit_count})"
    );

    // --- Size independence: the hit path for a 1 KiB blob and a 1 MiB blob
    // costs the same bounded budget. ---
    let blob_1kib = gen_data(1024, 2);
    store.put_blob(&blob_1kib).unwrap();
    let before = allocated_bytes();
    store.put_blob(&blob_1mib).unwrap();
    let hit_1mib = allocated_bytes() - before;
    let before = allocated_bytes();
    store.put_blob(&blob_1kib).unwrap();
    let hit_1kib = allocated_bytes() - before;
    assert!(
        hit_1mib < 4096 && hit_1kib < 4096,
        "hit-path allocation must be size-independent \
         (1 KiB: {hit_1kib} B, 1 MiB: {hit_1mib} B)"
    );

    // --- Steady state: re-ingesting a stored corpus (100 hits) allocates
    // the same small constant per hit — no accumulation, no re-store. ---
    let corpus: Vec<Vec<u8>> = (0..100u64).map(|i| gen_data(16 * 1024, 100 + i)).collect();
    for blob in &corpus {
        store.put_blob(blob).unwrap();
    }
    let before = allocated_bytes();
    for blob in &corpus {
        store.put_blob(blob).unwrap();
    }
    let reingest_bytes = allocated_bytes() - before;
    assert!(
        reingest_bytes < 100 * 4096,
        "full-corpus re-ingest (all hits) must stay in the constant-per-hit \
         budget (allocated {reingest_bytes} bytes for 100 x 16 KiB)"
    );
}
