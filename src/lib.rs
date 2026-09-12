// SPDX-License-Identifier: MIT OR Apache-2.0
//! `cas-kit` — a content-addressed storage (CAS) primitive.
//!
//! Blobs are stored on the local filesystem and identified by their
//! BLAKE3 hash (32 bytes / 256 bits). Identical blobs are deduplicated
//! automatically; integrity can be verified on every read.
//!
//! # On-Disk Layout
//!
//! ```text
//! <root>/
//!   objects/
//!     ab/           # First 2 hex chars of the hash (256 buckets)
//!       cdef...     # Remaining 62 hex chars = blob filename
//!     pack/
//!       pack-<hex>.pack   # Bundled blobs (always Zstd-compressed)
//!       pack-<hex>.idx    # Sorted hash -> offset index
//! ```
//!
//! The 2-hex-prefix bucketing avoids any single directory holding too
//! many entries. Pack files bundle many small blobs into one file to
//! reduce filesystem overhead; see [`PackFile`].
//!
//! # Correctness Properties
//!
//! - **Integrity**: `get(H(data)) == data` (BLAKE3 collision resistance),
//!   enforced by optional-but-default verify-on-read.
//! - **Deduplication**: storing the same blob twice writes one copy.
//! - **Lossless**: Zstd compression/decompression is lossless.
//!
//! # Thread Safety
//!
//! [`BlobStore`] is `Send + Sync` and can be shared across threads via
//! `Arc`. Interior mutability (the blob cache, pack-index cache and
//! bucket-directory cache) uses `std::sync::Mutex`; lock poisoning is
//! reported as [`CasError::LockPoisoned`] rather than unwrapped.
//!
//! # Features
//!
//! - `zstd` (default): enables Zstd compression of loose blobs and pack
//!   contents. Builds without this feature store everything raw and
//!   cannot read stores written by zstd-enabled builds (reads of
//!   compressed frames fail hash verification rather than silently
//!   returning wrong bytes).
//! - `tokio` (optional): async wrappers `gc::mark_async` /
//!   `gc::sweep_async` over the blocking thread pool.
//!
//! # Garbage collection
//!
//! Objects are opaque blobs, so reachability is a host-level concept;
//! [`gc`] implements mark–sweep over a host-supplied live set, with
//! dry-run / trash / delete modes and pack-rewrite support. The
//! `cas-gc` binary (same crate) drives it from the command line.
//!
//! # Example
//!
//! ```no_run
//! use cas_kit::BlobStore;
//!
//! # fn main() -> Result<(), cas_kit::CasError> {
//! let store = BlobStore::new("/tmp/my-store")?;
//! let hash = store.put_blob(b"hello, world")?;
//! assert_eq!(store.get_blob(&hash)?, b"hello, world".to_vec());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod compressor;
mod error;
pub mod gc;
mod hash;
mod hasher;
pub mod pack;
pub mod store;

pub use error::CasError;
pub use gc::{LiveSet, SweepMode, SweepOptions, SweepPlan, SweepReport};
pub use hash::Hash;
pub use hasher::{hash_bytes, hash_file, hash_with_context, verify_hash};
pub use pack::{PackCache, PackError, PackFile, PackIndex};
pub use store::BlobStore;
