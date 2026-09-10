# cas-kit

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
   a no-op that still yields the address.

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
behind. cas-kit is deliberately storage-only — it knows nothing about your
reference graph — so reclamation is a mark–sweep the host implements:

1. **Mark.** Walk whatever your application uses to name blobs (manifests,
   indices, message attachments) and collect the live set of hashes into a
   `HashSet<Hash>`.
2. **Enumerate.** List everything physically present:

   ```rust
   # use cas_kit::BlobStore;
   # fn example(store: &BlobStore) -> Result<(), cas_kit::CasError> {
   let mut present = store.list_blobs()?;        // loose objects
   present.extend(store.list_blobs_packed()?);   // objects inside packs
   # Ok(())
   # }
   ```

3. **Sweep.** Delete unreachable loose blobs:

   ```rust
   # use cas_kit::{BlobStore, Hash};
   # use std::collections::HashSet;
   # fn example(store: &BlobStore, live: &HashSet<Hash>) -> Result<(), cas_kit::CasError> {
   for hash in store.list_blobs()? {
       if !live.contains(&hash) {
           store.delete_blob(&hash)?;
       }
   }
   # Ok(())
   # }
   ```

Packed blobs are the one wrinkle: there is no per-blob delete inside a pack.
To drop garbage that was already packed, *rewrite* the pack with only live
objects — read each live blob, `PackFile::create` a new pack, delete the old
`.pack`/`.idx` pair, then `invalidate_pack_cache()`. Packs are immutable, so
a rebuild is always safe; schedule rewrites for maintenance windows.

Operational notes:

- Don't sweep while a concurrent writer may re-add a blob you are about to
  delete — take the same application-level lock your writers use.
- Deleting a still-referenced blob surfaces only at *read* time
  (`BlobNotFound`); the store cannot know your reference graph.
- A crash mid-sweep is safe: `delete_blob` removes one immutable file per
  call. Worst case is leftover garbage until the next sweep.

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

See [`examples/file_store.rs`](examples/file_store.rs) for a runnable
content-addressed file store (add / get / list / dedup / corruption demo).

## Cargo features

| Feature | Default | Effect                                            |
|---------|---------|---------------------------------------------------|
| `zstd`  | yes     | Zstd compression for loose blobs and pack payloads. |

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

- 78 tests: unit, integration (roundtrip, 256-bucket layout, pack
  write/read/repack, cache-hit behavior) and property-based tests
  (`proptest`) for arbitrary-bytes roundtrips with/without compression and
  hash stability.
- `cargo check --no-default-features` verified.
- `cargo clippy -D warnings` and `cargo fmt --check` clean.

## License

MIT OR Apache-2.0
