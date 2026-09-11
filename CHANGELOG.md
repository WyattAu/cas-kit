# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [0.2.0] - 2026-09-11

### Added

- **Garbage collection** (`cas_kit::gc`): mark–sweep over a host-supplied
  live set (objects are opaque blobs, so roots *are* the live set — the
  module docs specify the reference model precisely). `mark` validates
  roots against physical presence and reports missing ones; `plan_sweep`
  classifies garbage; `sweep` executes in `DryRun` / `Trash` (recoverable
  copies under `<root>/trash/`, atomic per-file renames) / `Delete` modes.
  Pack-aware: all-garbage packs are removed, healthy packs untouched, and
  partial packs are rewritten with only the live objects they uniquely
  cover (refcount-aware against loose copies and healthy packs). Crash
  safety: rewrites are additive and ordered before removals, so an
  interrupted sweep can only defer reclamation, never lose live data.
- **`cas-gc` CLI** (`[[bin]]` in the same crate): `cas-gc --root <dir>
  mark <roots-file>` and `sweep <roots-file> --dry-run|--apply
  [--delete]`; newline-separated hex roots (`#` comments, `-` = stdin);
  human and `--json` output (scanned, live, garbage, missing roots,
  bytes reclaimable/reclaimed, trash path); exit codes 0/1/2.
- **Async GC wrappers** behind the optional, non-default `tokio` feature:
  `gc::mark_async` / `gc::sweep_async` via `spawn_blocking`.
- **Dedup benchmark** (`benches/dedup.rs`, criterion): 1000-file corpus
  (50% unique, 20% near-dupes, 30% exact dupes) measuring storage
  savings (30.0% whole-object), ingest throughput vs a plain-write
  baseline (~4.3× dedup overhead, dominated by zstd on incompressible
  data) and the dedup-hit path (~10× faster than cold ingest). Confirms
  whole-object granularity: near-dupes cost full storage pending future
  chunking.
- GC integration tests: overlapping-pack rewrite/dedup, dry-run
  immutability, trash recoverability, redundant-pack removal, orphan and
  unreadable-pack handling, sweeps concurrent with live-object readers,
  mark→sweep full cycle, async parity, and `cas-gc` CLI integration
  (tempdir fixtures, JSON output, exit codes).
- Criterion benchmark suite `benches/cas_bench.rs`: put (hash → compress →
  write), cold get with and without verify-on-read, and `PackFile::create`
  pack throughput, at 1 KiB / 1 MiB / 100 MiB blob sizes; incompressible
  payloads so numbers reflect zstd's worst case.
- `examples/file_store.rs`: content-addressed file store CLI (`add` / `get`
  / `list`) plus a self-contained demo of deduplication and verify-on-read
  corruption detection (including the blob-cache caveat that corrupted
  reads must bypass the in-memory cache to be observed).
- README: deduplication semantics, the garbage-collection guide
  (reference model, pack coverage rule, crash-safety and concurrency
  notes, `cas-gc` recipes), a dedup benchmark table, and FUSE noted as
  future work.
- `BlobStore::root()` accessor; `CasError::GcPathEscape` and
  `CasError::TaskJoin` variants.

## [0.1.0] - 2026-09-05

### Added
- Initial public release.
