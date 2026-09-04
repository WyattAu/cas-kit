// SPDX-License-Identifier: MIT OR Apache-2.0
//! Error types for the content-addressed store.

use std::io;

use thiserror::Error;

use crate::pack::PackError;

/// Errors that can occur during CAS operations.
#[derive(Error, Debug)]
pub enum CasError {
    /// The requested blob does not exist in the store (loose or packed).
    #[error("blob not found: {0}")]
    BlobNotFound(String),

    /// The stored bytes did not hash to the requested address.
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// The hash the blob was addressed by.
        expected: String,
        /// The hash of what was actually read back.
        actual: String,
    },

    /// A mutex protecting interior cache state was poisoned.
    #[error("lock poisoned: {0}")]
    LockPoisoned(String),

    /// An underlying filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Zstd compression failed (requires the `zstd` feature).
    #[error("compression error: {0}")]
    CompressionError(String),

    /// Zstd decompression failed (requires the `zstd` feature).
    #[error("decompression error: {0}")]
    DecompressionError(String),

    /// Decompressed data exceeded the configured safety limit
    /// (zip-bomb protection).
    #[error("decompressed data too large: {max} bytes max")]
    DecompressionTooLarge {
        /// The maximum accepted decompressed size in bytes.
        max: usize,
    },

    /// [`BlobStore::put_blob_new`] was called for an existing blob.
    #[error("blob already exists: {0}")]
    AlreadyExists(String),

    /// A derived path was not usable (e.g. malformed hex on disk).
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// A packfile operation failed.
    #[error("pack error: {0}")]
    Pack(#[from] PackError),
}
