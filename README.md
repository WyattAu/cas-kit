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
