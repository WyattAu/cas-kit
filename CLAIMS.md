# cas-kit — claims inventory

Every verifiable numeric / behavioral performance claim in README.md, mapped
to its proof artifact. Generated as part of the perf-claims proof-back pass
(0.2.1).

Status legend:

- **backed** — an existing bench/test asserts the claim; linked below.
- **proven** — unproven at survey time; a bench/test was added by this pass.
- **reworded** — claim adjusted to what the artifacts actually prove.

## Throughput claims (README "Benchmarks" table)

All wall-clock numbers are indicative (single development machine, NVMe,
`/tmp` on tmpfs, deterministic incompressible payloads). The deterministic
per-binary regression gate is `benches/iai_cas.rs` (iai-callgrind; CI-only,
requires valgrind).

| Claim | Proof artifact | Status |
|---|---|---|
| `put/<size>` ~7 MiB/s (1 KiB) / ~320 MiB/s (1 MiB) / ~330 MiB/s (100 MiB) | `benches/cas_bench.rs::put` | backed |
| `get cold, verified` ~70 / ~430 / ~160 MiB/s | `benches/cas_bench.rs::get::cold` | backed |
| `get cold, unverified` ~70 / ~730 / ~260 MiB/s | `benches/cas_bench.rs::get::noverify` | backed |
| Verify-on-read tax at 100 MiB: ~260 → ~160 MiB/s | `get/noverify` vs `get/cold` groups in the same file | backed |
| `pack create` ~44 / ~320 / ~580 MiB/s | `benches/cas_bench.rs::pack::create` | backed |
| Cost model: one BLAKE3 pass per put, one per get unless disabled | structural (`src/store.rs`, `src/hasher.rs`) + the put/get groups above | backed |

## Deduplication claims (README "Deduplication" table)

| Claim | Proof artifact | Status |
|---|---|---|
| Storage savings 30.0% (15.6 MiB logical → 10.9 MiB stored); savings exactly match the exact-duplicate share | `benches/dedup.rs` corpus report (deterministic 30%-exact-dupe corpus) | backed |
| Cold ingest ~51 MiB/s (306 ms full corpus) | `benches/dedup.rs::dedup/ingest_cold` | backed |
| Naive plain-write baseline ~219 MiB/s (71 ms) | `benches/dedup.rs::naive/ingest_cold` | backed |
| Dedup ingest overhead ~4.3× vs naive | ratio of the two groups above | backed |
| Re-ingest of stored corpus ~518 MiB/s (30 ms), ~10× faster than cold | `benches/dedup.rs::dedup/put_hit` | backed |
| `put_blob` of identical bytes is a no-op re-store ("returns immediately") | `tests/zero_alloc_dedup_hit.rs` (heap: no content-proportional allocation, size-independent constant budget) + `benches/iai_cas.rs::put_hit` (instruction count) + `tests/integration.rs::dedup_and_delete_lifecycle` | **proven** |
| Dedup granularity is the whole blob; near-dupes stored as full copies | `benches/dedup.rs` corpus report near-dupe accounting | backed |
| `put_blob_new` fails with `AlreadyExists` on stored content | `tests/integration.rs::dedup_and_delete_lifecycle` | backed |

## Behavioral claims that double as perf claims

| Claim | Proof artifact | Status |
|---|---|---|
| Verify-on-read detects corruption (`HashMismatch`, never silent wrong bytes) | `tests/integration.rs::verify_on_read_detects_bitrot` | backed |
| Ring cache: bounded 1024 entries, MRU-promoted, serves repeat reads | `tests/integration.rs::blob_cache_serves_second_read` + `benches/iai_cas.rs::get_cache_hit` | backed |
| Zip-bomb safe: decompression capped at 1 GiB | `src/compressor.rs::MAX_DECOMPRESSED_SIZE` + unit tests | backed |
| Bucketed layout: 256 prefix dirs, bounded directory growth | `tests/integration.rs::bucketing_layout_uses_256_prefix_dirs` | backed |
| GC modes: `DryRun` changes nothing / `Trash` recoverable / `Delete` permanent | `tests/gc.rs::dry_run_touches_nothing`, `tests/gc.rs::trash_mode_is_recoverable`, `tests/gc.rs::sweep_removes_only_garbage_loose_objects`, `tests/cli.rs::cli_sweep_delete_mode_skips_trash` | backed |
| Pack rewrite coverage rules (all-garbage removed, healthy untouched, partial rewritten with unique live objects) | `tests/gc.rs::sweep_with_overlapping_packs_rewrites_and_dedups`, `::fully_garbage_pack_is_removed_not_rewritten`, `::redundant_partial_pack_is_removed_without_rewrite`, `::pack_rewrite_preserves_live_objects_byte_for_byte` | backed |
| Crash safety: interrupted sweep can only defer reclamation; re-run converges | `tests/gc.rs::orphan_pack_files_are_swept`, `::unreadable_pack_is_never_touched`, `::mark_then_sweep_full_cycle_leaves_exactly_the_live_set` | backed |
| `Send + Sync` via `Arc` | `tests/integration.rs::shared_across_threads_via_arc` | backed |
| No unsafe | `#![forbid(unsafe_code)]` in `src/lib.rs` | backed |

## Summary

- Backed by existing artifacts: 16
- Proven by artifacts added in this pass: 1 (dedup-hit no-re-store)
- Reworded or deleted: 0

New proof artifacts added in 0.2.1:

- `benches/iai_cas.rs` — iai-callgrind instruction-count gate for
  put-miss / dedup-hit / cold-verified-get / cache-hit-get (16 KiB blob).
- `tests/zero_alloc_dedup_hit.rs` — counting-allocator proof that the
  dedup-hit ingest path never allocates content-proportionally.
