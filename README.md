# cas-kit

[![docs.rs](https://docs.rs/cas-kit/badge.svg)](https://docs.rs/cas-kit)
[![crates.io](https://img.shields.io/crates/v/cas-kit.svg)](https://crates.io/crates/cas-kit)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

A content-addressed storage (CAS) primitive for Rust: blobs stored on the
local filesystem, addressed by their BLAKE3 hash, with optional Zstd
compression, bucketed directory layout, pack files, and verify-on-read.

Extracted from the [Suture](https://github.com/WyattAu/suture) codebase
(`suture-core/src/cas`), with every internal `unwrap()` eliminated.

## Features

- **BLAKE3 addressing** — 256-bit content hashes; identical blobs deduplicate
  automatically.
- **Bucketed layout** — blobs live at `objects/<2-hex>/<62-hex>` (256 buckets)
  so no single directory grows unbounded.
- **Verify-on-read** — every read re-hashes by default; disable per-store for
  hot paths.
- **Pack files** — bundle many small blobs into one `.pack` + sorted `.idx`
  pair; lazy pack-index caching; `repack(threshold)` in one call.
- **Mark–sweep GC** — `gc::mark` / `gc::sweep` with dry-run, recoverable
  trash, and delete modes; pack rewriting that drops garbage without losing
  shared objects. Ships with the `cas-gc` CLI.
- **Ring cache** — bounded in-memory blob cache (1024 entries, MRU-promoted).
- **Zip-bomb safe** — decompression is capped (1 GiB).
- **`Send + Sync`** — share a store across threads via `Arc`. Lock poisoning
  is reported as `CasError::LockPoisoned`, never unwrapped.
- **No unsafe** — `#![forbid(unsafe_code)]`.

## Usage

```rust
use cas_kit::BlobStore;

# fn main() -> Result<(), cas_kit::CasError> {
let store = BlobStore::new("/tmp/my-store")?;

// Put returns the BLAKE3 address; putting again is a dedup no-op.
let hash = store.put_blob(b"hello, world")?;
assert_eq!(store.put_blob(b"hello, world")?, hash);

// Get verifies integrity by default.
assert_eq!(store.get_blob(&hash)?, b"hello, world".to_vec());

// Bundle loose blobs into pack files once there are enough of them.
let packed = store.repack(1024)?;
assert_eq!(packed, 0); // nothing to pack yet

// Blobs remain readable from packs transparently.
assert!(store.has_blob(&hash));
# Ok(())
# }
```

Disable verification for a hot read path:

```rust
use cas_kit::BlobStore;

# fn main() -> Result<(), cas_kit::CasError> {
let mut store = BlobStore::new("/tmp/my-store")?;
store.set_verify_on_read(false);
# let hash = store.put_blob(b"x")?;
# assert!(store.get_blob(&hash).is_ok());
# Ok(())
# }
```

## Deduplication

Content addressing makes deduplication automatic and exact:

1. `put_blob` hashes the payload with BLAKE3.
2. The hash *is* the storage address (`objects/<2-hex>/<62-hex>`), so two
   writes of identical bytes target the same path.
3. The second write sees the file already exists and returns immediately —
   a no-op that still yields the address. This is not just an honor-system
   claim: `tests/zero_alloc_dedup_hit.rs` proves with a counting allocator
   that the hit path never allocates content-proportionally (no
   compression buffer, no re-store), and `benches/iai_cas.rs::put_hit`
   pins its instruction count for CI.

Implications:

- Dedup is **global**: any two callers writing the same bytes — from any
  path, process, or "file" — store one copy.
- Dedup granularity is the whole blob. Two 1 GB files differing in one byte
  are two blobs. If you need sub-file dedup, chunk large content into
  smaller blobs (64 KiB–4 MiB works well) and store a manifest blob
  referencing the chunk hashes.
- `put_blob_new` inverts the contract: it *fails* with `AlreadyExists` when
  the content is already stored (useful for "must be new" upload APIs).
- Verification interplay: `get_blob` re-hashes what it reads, so a corrupted
  or truncated copy fails loudly with `HashMismatch` instead of silently
  returning bytes that no longer deserve their address.

Cost: one BLAKE3 pass per put, and one per get unless disabled with
`set_verify_on_read(false)`. See [Benchmarks](#benchmarks) for what that
trade-off measures on real hardware.

## Garbage collection

A CAS never rewrites in place, so dropping a *reference* leaves its blob
behind. cas-kit objects are opaque — no tree objects, no embedded pointers —
so **reachability is a host-level concept**: the pack manifest maps digest →
offset only. GC is therefore an honest set difference, shipped as the
[`gc`](https://docs.rs/cas-kit/latest/cas_kit/gc/index.html) module and the
`cas-gc` CLI:

1. **Mark** — you supply the complete live set (every hash your application
   still wants; expand manifests/chunk lists to their transitive closure
   first). `gc::mark` validates it against what is physically present and
   reports roots that resolve to nothing.
2. **Plan** — `gc::plan_sweep` classifies `present − live` as garbage and
   decides pack handling.
3. **Sweep** — `gc::sweep` executes in one of three modes:

   | Mode | Effect |
   |---|---|
   | `DryRun` | enumerate and report; change nothing |
   | `Trash` | move garbage to `<root>/trash/` (atomic renames, recoverable) |
   | `Delete` | unlink permanently |

   ```rust
   # use std::collections::HashSet;
   # use cas_kit::gc::{self, SweepMode, SweepOptions};
   # use cas_kit::{BlobStore, Hash};
   # fn example(store: &BlobStore, roots: HashSet<Hash>) -> Result<(), cas_kit::CasError> {
   let live = gc::mark(store, &roots)?;
   let live_set: HashSet<Hash> = live.live.iter().copied().collect();
   let report = gc::sweep(store, &live_set, SweepOptions {
       mode: SweepMode::Trash,
       ..SweepOptions::default()
   })?;
   println!("reclaimed {} bytes", report.bytes_reclaimed);
   # Ok(())
   # }
   ```

### Packs

Packs are immutable and have no per-blob delete, so packed garbage is
reclaimed at pack granularity with a coverage rule:

- a pack that is *all* garbage is removed;
- a pack with no garbage is untouched, and its objects count as covered;
- a *partial* pack is rewritten with only the live objects it **uniquely**
  covers — an object that also survives loose, or in a healthy pack, is not
  copied. If nothing unique remains the pack is simply removed.

Rewrites run first and are additive: the replacement pack is fully written
before any old file is removed, and identical keep-sets converge to the same
content-derived pack name.

### Crash safety and concurrency

- A crash mid-sweep can lose garbage *reclamation*, never live data: loose
  deletion is one immutable file per call, pack removals delete the `.idx`
  first, and the old pack pair is only removed after the replacement is
  fully on disk. Re-running the sweep converges (and repairs a torn
  partially-rewritten pack).
- Reads of live objects are safe throughout: sweep never touches a file
  whose hash is in the live set. On Windows an unlink can fail with a
  sharing violation if a reader holds the file open — retry the sweep.
- If a writer re-puts a blob *between* enumeration and deletion, the sweep
  deletes the fresh copy (it was classified garbage first). Quiesce writers
  with the same application-level lock your writers use.
- Restoring from trash is a reverse rename of the mirrored path
  (`trash/objects/<2-hex>/<62-hex>` → back under `objects/`). Empty the
  trash once you are confident no restore is needed.

With the optional `tokio` feature, `gc::mark_async` / `gc::sweep_async` run
the same logic on the blocking thread pool.

### The `cas-gc` CLI

```text
cargo install cas-kit   # installs the cas-gc binary

cas-gc --root <DIR> mark <ROOTS_FILE>            # report; changes nothing
cas-gc --root <DIR> sweep <ROOTS_FILE> --dry-run # what would be removed
cas-gc --root <DIR> sweep <ROOTS_FILE> --apply   # sweep to trash (recoverable)
cas-gc --root <DIR> sweep <ROOTS_FILE> --apply --delete  # unlink permanently
```

`ROOTS_FILE` is newline-separated 64-char hex (`#` comments, `-` = stdin).
Human output covers scanned / live / garbage / bytes; `--json` emits a
single stable-keyed object for automation. Exit codes: 0 success,
1 operational failure, 2 usage error.

> Future work: a FUSE mount over the store (content-addressed filesystem
> views) — not part of 0.2.0.

## Benchmarks

`benches/cas_bench.rs` (criterion) measures the full loose-object path per
blob size:

- `put/<size>` — hash → Zstd compress → write, fresh store per iteration
- `get/cold/<size>` — disk read + decompress + **verify-on-read**, caches
  defeated by reopening the store each iteration
- `get/noverify/<size>` — same read path with verification disabled
- `pack/create/<size>` — `PackFile::create` over loose objects

```text
cargo bench --bench cas_bench            # everything
cargo bench --bench cas_bench -- get/1MiB  # one slice
```

Wall-clock numbers are noisy; the deterministic regression gate is
`benches/iai_cas.rs` ([iai-callgrind](https://github.com/iai-callgrind/iai-callgrind)):
it counts CPU instructions for the put-miss, dedup-hit, cold-verified-read,
and cache-hit paths on a fixed 16 KiB blob, so a hot-path regression fails
CI even when a busy runner hides it in the wall clock. It needs valgrind,
so it executes in CI only (`cargo bench --bench iai_cas --no-run` works
anywhere).

Every numeric claim in this README is mapped to its proof artifact in
[CLAIMS.md](CLAIMS.md).

Indicative numbers from a development machine (NVMe, `/tmp` on tmpfs,
deterministic *incompressible* payloads — zstd's worst case, so compressible
real-world content will do better):

| Operation | 1 KiB | 1 MiB | 100 MiB |
|---|---|---|---|
| put (hash+compress+write) | ~7 MiB/s¹ | ~320 MiB/s | ~330 MiB/s |
| get cold, verified | ~70 MiB/s | ~430 MiB/s | ~160 MiB/s |
| get cold, unverified | ~70 MiB/s | ~730 MiB/s | ~260 MiB/s |
| pack create | ~44 MiB/s² | ~320 MiB/s | ~580 MiB/s |

¹ dominated by per-blob filesystem syscalls, not throughput limits.
² 16 384 objects, each a separate Zstd frame — per-object compression
overhead; fewer, larger objects pack proportionally faster.

The verify-on-read tax at 100 MiB: ~260 → ~160 MiB/s (BLAKE3 re-hash on
every read). Turn it off per-store for hot read paths — the on-disk layout
(address = filename) still guarantees corruption is *detectable* whenever
you do choose to verify.

### Deduplication

`benches/dedup.rs` (criterion) ingests a synthetic corpus of 1000 files ×
16 KiB deterministic incompressible bytes — 50% unique, 20% near-duplicates
(same content, one byte changed), 30% exact duplicates — and measures
ingest throughput, the dedup-hit path, and a plain-write baseline:

| Measurement | Result (indicative*) |
|---|---|
| Storage savings (whole-object dedup) | **30.0%** (15.6 MiB logical → 10.9 MiB stored) |
| Ingest throughput, cold store | ~51 MiB/s (306 ms for the full corpus) |
| Naive plain-file write of the same corpus | ~219 MiB/s (71 ms) |
| Dedup ingest overhead vs naive | **~4.3×** (BLAKE3 + zstd level 3 + bucketed writes on incompressible data) |
| Re-ingest of an already-stored corpus (all hits) | ~518 MiB/s (30 ms) — 10× faster than cold ingest |

\* Single development machine, `/tmp` on tmpfs, machine under variable
load; treat ratios as the signal, absolute numbers as noise-prone.

Granularity note: cas-kit has **no chunking** — dedup is whole-object.
The 30% savings exactly match the exact-duplicate share. The 200
near-duplicates (files differing from an existing blob by a single byte)
are each stored as full new copies (~3.1 MiB): that is the headroom a
future chunking layer (64 KiB–4 MiB chunks + manifest blobs) could reclaim.
Run `cargo bench --bench dedup` to reproduce — the corpus report prints
your machine's numbers at startup.

See [`examples/file_store.rs`](examples/file_store.rs) for a runnable
content-addressed file store (add / get / list / dedup / corruption demo).

## Cargo features

| Feature | Default | Effect                                            |
|---------|---------|---------------------------------------------------|
| `zstd`  | yes     | Zstd compression for loose blobs and pack payloads. |
| `tokio` | no      | Async GC wrappers (`gc::mark_async`, `gc::sweep_async`) via `spawn_blocking`. |

Builds without `zstd` store everything raw. They cannot read stores written
by zstd-enabled builds: compressed frames fail hash verification rather than
silently returning wrong bytes.

## On-disk layout

```text
<root>/
  objects/
    ab/                 # 2-hex prefix bucket (256 total)
      cdef…             # remaining 62 hex chars = blob filename
    pack/
      pack-<hex>.pack   # "SPCK" header, typed length-prefixed objects
      pack-<hex>.idx    # "SIDX" header, digest → offset, binary-searchable
  trash/                # only after a trash-mode sweep (recoverable copies)
```

## Hash interop with Suture

`cas_kit::Hash` is a self-contained copy of `suture_common::Hash` with the
same in-memory representation (`pub [u8; 32]`) and lowercase-hex text form:

```text
suture_common::Hash(kit_hash.0)   // kit → suture
cas_kit::Hash(suture_hash.0)      // suture → kit
```

`hash_with_context` uses the same domain string as Suture, so context-keyed
hashes are stable across both.

## Testing

- ~100 tests: unit, integration (roundtrip, 256-bucket layout, pack
  write/read/repack, cache-hit behavior), GC (mark validation, dry-run,
  trash recoverability, overlapping-pack rewrites, orphan cleanup,
  unreadable-pack preservation, sweeps concurrent with readers), CLI
  integration (`cas-gc` mark/sweep/JSON/exit codes), and property-based
  tests (`proptest`) for arbitrary-bytes roundtrips with/without
  compression and hash stability.
- `tests/zero_alloc_dedup_hit.rs`: counting-allocator proof that the
  dedup-hit ingest path allocates a size-independent constant budget
  (no compression buffer, no re-store of duplicates).
- `benches/iai_cas.rs`: iai-callgrind instruction-count gate for the
  put/get hot paths (CI-only; requires valgrind).
- `cargo check --no-default-features` verified.
- `cargo clippy -D warnings` (all-features and no-default-features),
  `cargo fmt --check`, and `cargo doc` (zero warnings) clean.

## License

MIT OR Apache-2.0
