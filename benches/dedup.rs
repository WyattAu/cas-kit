//! Deduplication benchmarks over a synthetic corpus.
//!
//! The corpus is 1000 files of 16 KiB deterministic pseudo-random bytes:
//!
//! - 50% unique — random bases seen once
//! - 20% near-duplicates — a base with one byte flipped: *new* content
//!   that differs from an existing blob by exactly 1 byte
//! - 30% exact duplicates — byte-identical copies of an earlier file
//!
//! cas-kit has no chunking: dedup granularity is the whole object. The
//! benchmarks therefore measure whole-object dedup — exact duplicates
//! collapse, near-duplicates do not. The `corpus report` printed at
//! startup quantifies both effects (the near-duplicate share is what a
//! future chunking layer could reclaim).
//!
//! Groups:
//!
//! - `dedup/ingest_cold` — put all 1000 files into a fresh store per
//!   iteration (hash → dedup check → compress → write for misses).
//! - `naive/ingest_cold` — write the same files into a plain directory
//!   per iteration; the dedup-overhead baseline.
//! - `dedup/put_hit` — re-put the corpus into a store that already
//!   holds it: the pure dedup-hit path (hash + existence check, no
//!   write I/O).
//!
//! Run: `cargo bench --bench dedup`

// Benchmarks exercise only fixed happy-path inputs; unwraps here surface
// as benchmark failures, never as production behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::hint::black_box;
use std::time::Instant;

use cas_kit::{hash_bytes, BlobStore, Hash};
use criterion::{criterion_group, BenchmarkId, Criterion, Throughput};
use tempfile::TempDir;

/// File count in the corpus.
const FILE_COUNT: usize = 1000;
/// Size of every file.
const FILE_SIZE: usize = 16 * 1024;
/// Exact-duplicate files (byte-identical copies of earlier files).
const EXACT_DUPES: usize = 300;
/// Near-duplicate files (one flipped byte off an earlier base).
const NEAR_DUPES: usize = 200;
/// Files stored as unique bases: 50% uniques + 20% near-dupe bases.
const BASE_COUNT: usize = FILE_COUNT - EXACT_DUPES;

/// Deterministic splitmix64 fill (same generator as `cas_bench`). The
/// mix is bijective in `seed`, so distinct seeds always yield distinct
/// buffers and hashes.
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

/// The synthetic corpus: files `0..500` are unique bases, `500..700`
/// are one-byte mutations of earlier bases, and `700..1000` are exact
/// copies of earlier files.
fn corpus() -> Vec<Vec<u8>> {
    let uniques = BASE_COUNT - NEAR_DUPES;
    let bases: Vec<Vec<u8>> = (0..uniques)
        .map(|i| gen_data(FILE_SIZE, i as u64 + 1))
        .collect();
    let mut files = Vec::with_capacity(FILE_COUNT);
    for i in 0..FILE_COUNT {
        if i < uniques {
            files.push(bases[i].clone());
        } else if i < BASE_COUNT {
            // Near-dupe: same content, one byte changed.
            let mut near = bases[i - uniques].clone();
            let flip = (i * 7) % FILE_SIZE;
            near[flip] ^= 0xFF;
            files.push(near);
        } else {
            // Exact dupe of an earlier file.
            files.push(files[i - BASE_COUNT].clone());
        }
    }
    files
}

/// Sanity-check the corpus split; returns (logical bytes, unique hashes).
fn corpus_stats(files: &[Vec<u8>]) -> (u64, usize) {
    let logical: u64 = files.iter().map(Vec::len).sum::<usize>() as u64;
    let unique: HashSet<Hash> = files.iter().map(|f| hash_bytes(f)).collect();
    assert_eq!(
        unique.len(),
        BASE_COUNT,
        "corpus must contain exactly {BASE_COUNT} unique objects"
    );
    (logical, unique.len())
}

fn bench_dedup_ingest(c: &mut Criterion) {
    let files = corpus();
    let total: u64 = files.iter().map(Vec::len).sum::<usize>() as u64;
    let mut group = c.benchmark_group("dedup");
    group.sample_size(30);
    group.throughput(Throughput::Bytes(total));
    group.bench_function(BenchmarkId::new("ingest_cold", "1000x16KiB"), |b| {
        b.iter(|| {
            let dir = TempDir::new().unwrap();
            let store = BlobStore::new(dir.path()).unwrap();
            for file in &files {
                black_box(store.put_blob(file).unwrap());
            }
            black_box(store.total_size().unwrap())
        })
    });
    group.finish();
}

fn bench_naive_ingest(c: &mut Criterion) {
    let files = corpus();
    let total: u64 = files.iter().map(Vec::len).sum::<usize>() as u64;
    let mut group = c.benchmark_group("naive");
    group.sample_size(30);
    group.throughput(Throughput::Bytes(total));
    group.bench_function(BenchmarkId::new("ingest_cold", "1000x16KiB"), |b| {
        b.iter(|| {
            let dir = TempDir::new().unwrap();
            for (i, file) in files.iter().enumerate() {
                let path = dir.path().join(format!("file-{i}"));
                std::fs::write(path, file).unwrap();
            }
        })
    });
    group.finish();
}

fn bench_dedup_hit(c: &mut Criterion) {
    let files = corpus();
    let total: u64 = files.iter().map(Vec::len).sum::<usize>() as u64;
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    for file in &files {
        store.put_blob(file).unwrap();
    }
    let mut group = c.benchmark_group("dedup");
    group.sample_size(50);
    group.throughput(Throughput::Bytes(total));
    group.bench_function(BenchmarkId::new("put_hit", "1000x16KiB"), |b| {
        b.iter(|| {
            for file in &files {
                black_box(store.put_blob(file).unwrap());
            }
        })
    });
    group.finish();
}

/// Ingest once and print the storage report the README table quotes:
/// logical vs stored bytes, whole-object savings, and the near-duplicate
/// share that only chunk-level dedup could reclaim.
fn print_corpus_report() {
    let files = corpus();
    let (logical, unique) = corpus_stats(&files);

    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    // One throwaway ingest so the measured pass reflects steady state
    // (page cache warm), matching the criterion groups below.
    for file in &files {
        store.put_blob(file).unwrap();
    }
    let dir = TempDir::new().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    let start = Instant::now();
    for file in &files {
        store.put_blob(file).unwrap();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let stored = store.total_size().unwrap();

    let savings = 100.0 * (1.0 - stored as f64 / logical as f64);
    println!(
        "\ncorpus report ({} files x {} KiB):",
        FILE_COUNT,
        FILE_SIZE / 1024
    );
    println!("  logical input:    {}", human_bytes(logical));
    println!("  unique objects:   {unique}");
    println!("  stored (on disk): {} (zstd level 3)", human_bytes(stored));
    println!("  storage savings:  {savings:.1}% (whole-object dedup)");
    println!(
        "  near-dupes:       {} files stored as {} full copies — reclaimable only by chunk-level dedup (future work)",
        NEAR_DUPES,
        human_bytes((NEAR_DUPES * FILE_SIZE) as u64)
    );
    println!(
        "  ingest throughput: {} (one-shot over {} files)",
        human_bytes((logical as f64 / elapsed) as u64),
        FILE_COUNT
    );
    println!("  (precise timing: criterion groups below)\n");
}

/// Human-readable byte count (binary units).
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

criterion_group!(
    benches,
    bench_dedup_ingest,
    bench_naive_ingest,
    bench_dedup_hit
);

/// Print the storage report first (criterion would otherwise swallow the
/// startup output as benchmark noise), then run the timing benchmarks.
fn main() {
    print_corpus_report();
    benches();
}
