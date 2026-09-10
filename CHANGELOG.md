# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [Unreleased]

### Added

- Criterion benchmark suite `benches/cas_bench.rs`: put (hash → compress →
  write), cold get with and without verify-on-read, and `PackFile::create`
  pack throughput, at 1 KiB / 1 MiB / 100 MiB blob sizes; incompressible
  payloads so numbers reflect zstd's worst case.
- `examples/file_store.rs`: content-addressed file store CLI (`add` / `get`
  / `list`) plus a self-contained demo of deduplication and verify-on-read
  corruption detection (including the blob-cache caveat that corrupted
  reads must bypass the in-memory cache to be observed).
- README: deduplication semantics (global, whole-blob granularity,
  `put_blob_new` inversion, verification interplay), a garbage-collection
  guide (mark–sweep over loose + packed objects, pack rewrite recipe,
  crash-safety notes), and a benchmarks section with indicative numbers.

## [0.1.0] - 2026-09-05

### Added
- Initial public release.
