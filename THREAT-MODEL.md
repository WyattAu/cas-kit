# Threat Model — cas-kit

Reference: STRIDE. Scope: the crate's public API surface (`BlobStore`, `Hash`,
`hash_*`, `PackFile`, `gc`, the `cas-gc` binary) as used by a downstream
service. Trust boundaries:
(1) bytes entering `put_blob` and hashes entering `get_blob`/`Hash::from_hex`,
(2) on-disk files under the store root (shared with backup jobs, other
processes),
(3) the roots file supplied to GC (host-controlled live-set assertion),
(4) the dependency tree (BLAKE3, zstd, clap).

## Assets

| ID | Asset | Example |
|----|-------|---------|
| A1 | Integrity of stored blobs (bit-rot, partial write, silent corruption) | A flipped byte served as if it were the original content |
| A2 | Availability of the store (bounded memory / disk) | Decompression bomb or unbounded blob count exhausts the host |
| A3 | Uniqueness of content addresses | A crafted collision lets one blob impersonate another |

## STRIDE Analysis

| # | Threat | Category | Surface | Mitigation | Verifying test |
|---|--------|----------|---------|------------|----------------|
| T1 | Corrupted blob returned as valid | Tampering | `BlobStore::get_blob` | Verify-on-read is **on by default**: every read recomputes BLAKE3 and fails with `CasError::HashMismatch` on any divergence; opt-out is explicit via `set_verify_on_read(false)` | `verify_on_read_detects_bitrot`, `put_get_verify_roundtrip` (`tests/integration.rs`) |
| T2 | Forged hash preimage (blob impersonation) | Spoofing | `put_blob`, `Hash` | BLAKE3 256-bit content addressing; the address *is* the digest, so impersonation requires a collision | `blake3_known_vector`, `distinct_content_distinct_hash`, `hash_stability_across_stores` |
| T3 | Hash string injection / path traversal via hex | Spoofing/Tampering | `Hash::from_hex`, `blob_path` | Hex parsing rejects non-hex and wrong-length input before it can reach a filename; filenames are derived solely from validated 64-char hex, split into 2-char bucket dirs | `rejects_bad_hex`, `rejects_bad_length`, `bucketing_layout_uses_256_prefix_dirs`, `kani_bucket_matches_hex_prefix` (`tests/kani.rs`) |
| T4 | Decompression bomb on read | DoS | `get_blob` (zstd feature) | Decompressed size capped at `MAX_DECOMPRESSED_SIZE`; excess fails with `CasError::DecompressionTooLarge` instead of allocating | `test_decompress_rejects_zip_bomb` (`tests/integration.rs:748`) |
| T5 | Truncated/partial write becomes readable garbage | Tampering | loose + pack objects | Even without verify-on-read, pack reads are bounds-checked against the sorted `.idx`; a wrong-size or corrupt frame surfaces as a hash mismatch or decode error, not silent wrong bytes | `pack_write_read_repack_roundtrip`, `get_blob_packed`, `has_blob_packed` |
| T6 | Feature downgrades corrupt data invisibly | Tampering | zstd vs no-zstd builds | A zstd-compressed frame read by a non-zstd build fails hash verification loudly rather than returning wrong bytes (documented in crate docs) | `roundtrip_without_compression` vs `roundtrip_with_compression`; no silent cross-build read path |
| T7 | Store exhaustion (unbounded blobs) | DoS | `put_blob` | **Partially mitigated in 0.2.0** — `cas_kit::gc` + the `cas-gc` CLI reclaim unreachable objects (dry-run/trash/delete). Residual risk: nothing runs GC *for* you; a deployment that never sweeps still grows unbounded, and a sweep with a wrong/incomplete roots file deletes reachable data (see T8) | `mark_then_sweep_full_cycle_leaves_exactly_the_live_set`, `sweep_removes_only_garbage_loose_objects` (`tests/gc.rs`) |
| T8 | GC deletes live data via wrong roots or a writer race | DoS/Repudiation | `gc::mark`, `gc::sweep`, roots file | Roots are the host's live-set assertion (objects are opaque; documented in `gc` module docs). Mitigations: mark reports missing roots; dry-run mode for rehearsal; trash mode (default for `cas-gc sweep --apply`) makes every sweep recoverable by reverse rename; crash-safe phase ordering (rewrites additive, removals last) means an interrupted sweep never loses live data. Residual: a writer re-putting a blob between enumeration and deletion loses the fresh copy — quiesce writers (documented) | `dry_run_touches_nothing`, `trash_mode_is_recoverable`, `sweep_concurrent_with_reads_of_live_objects` |
| T9 | Trash directory read as store content | Tampering | `<root>/trash/` | Trash lives outside `objects/` and the store never reads it; relocation uses `strip_prefix` against the store root and fails closed with `CasError::GcPathEscape` on any path outside the root | `trash_mode_is_recoverable`; `gc::remove_or_trash` code review |

## Repudiation

There is no audit log of put/get/delete operations. Deletion is unlogged and
irreversible — a caller (or anyone with filesystem write access) can remove a
blob without a trace. Accepted: the store is a primitive, not a compliance
surface. Trash-mode sweeps mitigate accidental (not malicious) deletion by
keeping recoverable copies under `<root>/trash/` until emptied.

## Out of Scope

- Filesystem-level access control: anyone with read access to the store root
  can read every blob; write access enables tampering the store cannot
  detect until the next verified read. The OS owns this boundary.
- Encryption at rest: blobs are plaintext by design (content addressing).
- Concurrent multi-process writes to the same store root: single-writer per
  bucket is assumed; `put_blob` writes are atomic per-file (write-rename not
  guaranteed on all platforms) — see `dedup_and_delete_lifecycle` for the
  tested lifecycle.

## Residual Risks

- **R1 (Low, accepted):** Dedup oracle. Storing blob X and observing the
  returned hash (or the bucket filename) reveals whether identical content
  existed. Inherent to content addressing; document for deployments where
  content existence is sensitive.
- **R2 (Low, accepted):** `set_verify_on_read(false)` trades integrity for
  speed; the resulting silent-corruption window is caller-chosen and flagged
  in the API docs.
- **R3 (Low, accepted):** Dependency risk in `blake3`/`zstd` C bindings
  (zstd-sys). No in-repo `cargo audit` gate; relies on org-level Dependabot.
- **R4 (Low, accepted):** No lock-file or cross-process coordination; two
  processes packing or sweeping concurrently into the same store can
  interleave pack files. Single-writer deployments assumed (GC included:
  run one sweep at a time per store root).
- **R5 (Low, accepted):** Transient read errors during pack rewrites: a
  reader with a stale in-memory pack index can hit a pack that a concurrent
  sweep just removed; the object remains readable via its other copies once
  the reader reloads (retry the read). Documented in the `gc` module docs.
- **R6 (Low, accepted):** The `cas-gc` roots file is trusted input: whoever
  controls it controls what a sweep keeps. Filesystem permissions on the
  roots file are the host's responsibility.
