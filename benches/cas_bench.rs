//! Throughput benchmarks for the content-addressed store.
//!
//! Sizes: 1 KiB, 1 MiB, 100 MiB. Groups:
//!
//! - `put` — full loose-object path (BLAKE3 hash → Zstd compress → write)
//!   on a fresh store per iteration; dedup would otherwise turn every
//!   repeat into a no-op.
//! - `get/cold` — reads N distinct blobs through a *freshly reopened*
//!   store per iteration, so the blob cache and pack cache can never
//!   serve a hit; this measures real disk read + decompress +
//!   verify-on-read.
//! - `get/noverify` — same read path with `set_verify_on_read(false)`,
//!   isolating the cost of per-read integrity verification.
//! - `pack/create` — `PackFile::create` over loose objects (compress +
//!   serialize + index) into a fresh pack directory per iteration.
//!
//! Payloads are deterministic pseudo-random bytes: incompressible, so the
//! numbers reflect zstd's worst case rather than its best. Real-world
//! compressible content will show higher effective throughput.
//!
//! Run everything: `cargo bench`
//! Run one group:  `cargo bench --bench cas_bench -- put/1MiB`

// Benchmarks exercise only fixed happy-path inputs; unwraps here surface
// as benchmark failures, never as production behavior.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::Duration;

use cas_kit::{hash_bytes, BlobStore, Hash, PackFile};
use criterion::measurement::WallTime;
use criterion::{
    criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion, Throughput,
};
use tempfile::TempDir;

/// (bytes, label) for every measured object size.
const SIZES: &[(usize, &str)] = &[
    (1024, "1KiB"),
    (1024 * 1024, "1MiB"),
    (100 * 1024 * 1024, "100MiB"),
];

/// (bytes, blob count, label) for the `get` group. Counts trade syscall
/// overhead (small blobs) against I/O volume (large blobs) the way real
/// workloads do: a sweep reads ~2 KiB of metadata-heavy small objects or
/// hundreds of MiB of large ones.
const GET_SETS: &[(usize, usize, &str)] = &[
    (1024, 2048, "1KiB"),
    (1024 * 1024, 64, "1MiB"),
    (100 * 1024 * 1024, 8, "100MiB"),
];

/// (bytes, object count, label) for the `pack` group — many small objects
/// (the packing sweet spot) down to a few huge ones.
const PACK_SETS: &[(usize, usize, &str)] = &[
    (1024, 16_384, "1KiB"),
    (1024 * 1024, 256, "1MiB"),
    (100 * 1024 * 1024, 4, "100MiB"),
];

/// Deterministic splitmix64 fill. The mix is bijective in `seed`, so
/// distinct seeds always yield distinct buffers and hashes.
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

/// 100 MiB iterations are ~0.3 s each; shrink the sampling so the whole
/// suite stays inside a coffee break while keeping >= 10 samples.
fn tune_for_size(group: &mut BenchmarkGroup<'_, WallTime>, size: usize) {
    if size >= 100 * 1024 * 1024 {
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(1));
        group.measurement_time(Duration::from_secs(15));
    } else {
        group.sample_size(100);
        group.warm_up_time(Duration::from_secs(2));
        group.measurement_time(Duration::from_secs(5));
    }
}

fn bench_put(c: &mut Criterion) {
    let mut group = c.benchmark_group("put");
    for &(size, label) in SIZES {
        tune_for_size(&mut group, size);
        group.throughput(Throughput::Bytes(size as u64));
        let data = gen_data(size, 1);
        group.bench_with_input(BenchmarkId::from_parameter(label), &data, |b, data| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let store = BlobStore::new(dir.path()).unwrap();
                black_box(store.put_blob(black_box(data)).unwrap());
            })
        });
    }
    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("get");
    for &(size, count, label) in GET_SETS {
        tune_for_size(&mut group, size);
        group.throughput(Throughput::Bytes((size * count) as u64));

        // Populate once, then drop the store so per-iteration reads start
        // with cold blob/pack caches.
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        {
            let store = BlobStore::new(&root).unwrap();
            for i in 0..count {
                let data = gen_data(size, i as u64 + 2);
                store.put_blob(&data).unwrap();
            }
        }
        let hashes: Vec<Hash> = {
            let store = BlobStore::new(&root).unwrap();
            store.list_blobs().unwrap()
        };
        assert_eq!(hashes.len(), count);

        group.bench_with_input(BenchmarkId::new("cold", label), &hashes, |b, hashes| {
            b.iter(|| {
                let store = black_box(BlobStore::new(&root).unwrap());
                for hash in hashes {
                    black_box(store.get_blob(hash).unwrap());
                }
            })
        });

        group.bench_with_input(BenchmarkId::new("noverify", label), &hashes, |b, hashes| {
            b.iter(|| {
                let mut store = black_box(BlobStore::new(&root).unwrap());
                store.set_verify_on_read(false);
                for hash in hashes {
                    black_box(store.get_blob(hash).unwrap());
                }
            })
        });
    }
    group.finish();
}

fn bench_pack(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack");
    for &(size, count, label) in PACK_SETS {
        tune_for_size(&mut group, size);
        group.throughput(Throughput::Bytes((size * count) as u64));

        let objects: Vec<(Hash, Vec<u8>)> = (0..count)
            .map(|i| {
                let data = gen_data(size, i as u64 + 2);
                (hash_bytes(&data), data)
            })
            .collect();

        group.bench_function(BenchmarkId::new("create", label), |b| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let pack_dir = dir.path().join("objects").join("pack");
                let (pack, idx) = PackFile::create(&pack_dir, &objects).unwrap();
                black_box((pack, idx));
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_put, bench_get, bench_pack);
criterion_main!(benches);
