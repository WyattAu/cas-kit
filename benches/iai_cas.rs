// iai-callgrind benchmarks run once under Valgrind on fixed inputs; the
// harness measures instruction counts, so there is no "expected failure"
// recovery path — a panic aborts the run visibly, which is what we want.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! Deterministic regression gate for the BlobStore hot paths.
//!
//! Unlike criterion (wall-clock, noisy, human-readable trend —
//! `benches/cas_bench.rs` / `benches/dedup.rs`), iai-callgrind counts CPU
//! instructions under Valgrind and is reproducible for a given binary —
//! fit for a CI gate. Criterion stays the source of the wall-clock trend;
//! this file is the pass/fail gate.
//!
//! Paths pinned (all on a 16 KiB blob, zstd default level):
//!
//! - `put_miss` — cold put: BLAKE3 hash → zstd compress → write
//! - `put_hit` — the dedup-hit path: hash → existence check → return
//!   (the README claims this is a no-op re-store; the instruction count
//!   pins its cost and `tests/zero_alloc_dedup_hit.rs` pins its heap
//!   behavior)
//! - `get_cold_verified` — disk read + decompress + verify-on-read
//! - `get_cache_hit` — the in-memory ring cache serves the blob

use cas_kit::{BlobStore, Hash};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use tempfile::TempDir;

const BLOB_SIZE: usize = 16 * 1024;

/// Deterministic splitmix64 fill (same generator as `benches/cas_bench.rs`),
/// so the payload is incompressible and every run hashes identical bytes.
fn blob() -> Vec<u8> {
    let mut buf = vec![0u8; BLOB_SIZE];
    let mut state = 42u64;
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

fn setup_fresh_store() -> (TempDir, BlobStore, Vec<u8>) {
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    let data = blob();
    (dir, store, data)
}

fn setup_populated_store() -> (TempDir, BlobStore, Vec<u8>, Hash) {
    let (dir, store, data) = setup_fresh_store();
    let hash = store.put_blob(&data).unwrap();
    (dir, store, data, hash)
}

// Cold put on a fresh store: hash → zstd compress → bucketed write.
#[library_benchmark]
#[bench::cold_put(setup = setup_fresh_store)]
fn put_miss(env: (TempDir, BlobStore, Vec<u8>)) -> Hash {
    let (_dir, store, data) = env;
    store.put_blob(&data).unwrap()
}

// Dedup-hit re-put: the blob already exists, so this must be the cheap
// hash + existence-check path (no compression, no write).
#[library_benchmark]
#[bench::dedup_hit(setup = setup_populated_store)]
fn put_hit(env: (TempDir, BlobStore, Vec<u8>, Hash)) -> Hash {
    let (_dir, store, data, _hash) = env;
    store.put_blob(&data).unwrap()
}

// Cold verified read: disk read + zstd decompress + BLAKE3 verify.
// The store is freshly opened in setup, so the ring cache is empty.
#[library_benchmark]
#[bench::cold_verified(setup = setup_populated_store)]
fn get_cold_verified(env: (TempDir, BlobStore, Vec<u8>, Hash)) -> Vec<u8> {
    let (_dir, store, _data, hash) = env;
    store.get_blob(&hash).unwrap()
}

// Ring-cache hit: a warm-up get in setup primes the in-memory cache; the
// measured get is served from it (MRU-promoted, no filesystem access).
#[library_benchmark]
#[bench::cache_hit(setup = setup_cache_hit)]
fn get_cache_hit(env: (TempDir, BlobStore, Vec<u8>, Hash)) -> Vec<u8> {
    let (_dir, store, _data, hash) = env;
    store.get_blob(&hash).unwrap()
}

fn setup_cache_hit() -> (TempDir, BlobStore, Vec<u8>, Hash) {
    let env = setup_populated_store();
    env.1.get_blob(&env.3).unwrap();
    env
}

library_benchmark_group!(
    name = iai_cas_hot_path;
    benchmarks = put_miss, put_hit, get_cold_verified, get_cache_hit
);

main!(library_benchmark_groups = iai_cas_hot_path);
