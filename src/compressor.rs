// SPDX-License-Identifier: MIT OR Apache-2.0
//! Zstd compression/decompression utilities.
//!
//! Zstd provides an excellent balance of compression ratio and speed.
//! The default compression level is 3, which provides ~2-3x compression
//! on typical project data with minimal CPU overhead.
//!
//! This module is only compiled with the `zstd` feature (default-on).
//! [`is_zstd_compressed`] lives in [`crate::store`] instead so the
//! feature-less build keeps a single magic-check implementation.

#[cfg(feature = "zstd")]
use std::io::{Read, Write};

#[cfg(feature = "zstd")]
use crate::CasError;

/// Default Zstd compression level.
/// Level 3 provides a good balance of speed (~500 MB/s) and ratio (~2.5x).
pub const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

/// Maximum decompressed size to prevent zip bombs.
/// 1 GB is a reasonable safety limit for v0.1.
#[cfg(feature = "zstd")]
pub const MAX_DECOMPRESSED_SIZE: usize = 1024 * 1024 * 1024;

/// Compress data using Zstd at the given level.
///
/// Returns the compressed bytes as a Zstd frame (which includes the
/// uncompressed size in the header).
#[cfg(feature = "zstd")]
pub fn compress(data: &[u8], level: i32) -> Result<Vec<u8>, CasError> {
    let mut encoder = zstd::Encoder::new(Vec::new(), level)
        .map_err(|e| CasError::CompressionError(e.to_string()))?;
    encoder
        .write_all(data)
        .map_err(|e| CasError::CompressionError(e.to_string()))?;
    let compressed = encoder
        .finish()
        .map_err(|e| CasError::CompressionError(e.to_string()))?;
    Ok(compressed)
}

/// Compress data at the default level (3).
#[cfg(feature = "zstd")]
pub fn compress_default(data: &[u8]) -> Result<Vec<u8>, CasError> {
    compress(data, DEFAULT_COMPRESSION_LEVEL)
}

/// Decompress data using Zstd.
///
/// Validates that the decompressed size does not exceed
/// [`MAX_DECOMPRESSED_SIZE`] to prevent zip bomb attacks.
#[cfg(feature = "zstd")]
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, CasError> {
    let mut decoder =
        zstd::Decoder::new(data).map_err(|e| CasError::DecompressionError(e.to_string()))?;

    // Use a bounded reader to prevent zip bombs.
    let mut output = Vec::with_capacity(data.len() * 2); // Heuristic initial size
    let mut buffer = [0u8; 64 * 1024]; // 64 KB read buffer

    loop {
        let n = decoder
            .read(&mut buffer)
            .map_err(|e| CasError::DecompressionError(e.to_string()))?;
        if n == 0 {
            break;
        }
        if output.len() + n > MAX_DECOMPRESSED_SIZE {
            return Err(CasError::DecompressionTooLarge {
                max: MAX_DECOMPRESSED_SIZE,
            });
        }
        output.extend_from_slice(&buffer[..n]);
    }

    Ok(output)
}

#[cfg(all(test, feature = "zstd"))]
mod tests {
    // Miri cannot execute foreign (C) functions, so every test that reaches
    // into zstd's FFI boundary (`ZSTD_createCCtx` and friends) is ignored
    // under miri. cas-kit itself is `#![forbid(unsafe_code)]`; the excluded
    // surface is pure FFI delegation, and the rest of the crate (hashing,
    // store, pack parsing) is still exercised by the miri suite.
    // See .github/workflows/ci.yml for the miri configuration.
    use super::*;

    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_compress_decompress_roundtrip() -> Result<(), CasError> {
        let original = b"Hello! This is test data for compression roundtrip.";
        let compressed = compress_default(original)?;
        let decompressed = decompress(&compressed)?;
        assert_eq!(original.as_slice(), decompressed.as_slice());
        Ok(())
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_compress_larger_data() -> Result<(), CasError> {
        let original: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let compressed = compress_default(&original)?;
        let decompressed = decompress(&compressed)?;
        assert_eq!(original, decompressed);

        // Compressed should be smaller (repetitive data compresses well).
        assert!(compressed.len() < original.len());
        Ok(())
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_compress_empty() -> Result<(), CasError> {
        let original = b"";
        let compressed = compress_default(original)?;
        let decompressed = decompress(&compressed)?;
        assert_eq!(original.as_slice(), decompressed.as_slice());
        Ok(())
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_is_zstd_compressed_magic() -> Result<(), CasError> {
        // Zstd frames start with 0x28 0xB5 0x2F 0xFD.
        let compressed = compress_default(b"hello")?;
        assert!(compressed.len() >= 4);
        assert_eq!(&compressed[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        Ok::<_, CasError>(())
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_decompress_invalid_data() {
        let result = decompress(b"not zstd data at all!");
        assert!(matches!(result, Err(CasError::DecompressionError(_))));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn test_compress_levels() -> Result<(), CasError> {
        let data = "The quick brown fox jumps over the lazy dog. ".repeat(1000);
        let bytes = data.as_bytes();

        let c1 = compress(bytes, 1)?;
        let c3 = compress(bytes, 3)?;
        let c9 = compress(bytes, 9)?;

        // Higher compression should generally produce smaller output.
        assert!(c9.len() <= c3.len());
        assert!(c3.len() <= c1.len());
        Ok(())
    }
}
