// SPDX-License-Identifier: MIT OR Apache-2.0
//! A BLAKE3 content hash (32 bytes / 256 bits).
//!
//! This is a self-contained copy of the `suture_common::Hash` type so the
//! crate has no dependency on the Suture workspace. The in-memory
//! representation (`pub [u8; 32]`) and the lowercase-hex text form are
//! identical to the Suture definition, making the two types trivially
//! interchangeable:
//!
//! ```text
//! suture_common::Hash(hash.0)          // kit -> suture
//! cas_kit::Hash(suture_hash.0)         // suture -> kit
//! ```

use std::fmt;

use blake3::Hash as Blake3Hash;

/// Error returned when parsing a [`Hash`] from text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HashError {
    /// The input was not exactly 64 characters.
    #[error("invalid hash length: expected 64 hex chars, got {0}")]
    InvalidLength(usize),
    /// The input contained non-hex or non-UTF-8 characters.
    #[error("invalid hex in hash")]
    InvalidHex,
}

/// A BLAKE3 content hash (32 bytes / 256 bits).
///
/// Used as the canonical identifier for blobs in the content-addressed
/// store. BLAKE3 provides SIMD-accelerated hashing with a 2^128 collision
/// resistance bound.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(
    /// The raw 32-byte digest.
    pub [u8; 32],
);

impl Hash {
    /// Compute the BLAKE3 hash of arbitrary data.
    #[must_use]
    pub fn from_data(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    /// Parse a hash from a 64-character lowercase hex string.
    pub fn from_hex(hex: &str) -> Result<Self, HashError> {
        if hex.len() != 64 {
            return Err(HashError::InvalidLength(hex.len()));
        }
        let mut bytes = [0u8; 32];
        hex.as_bytes()
            .chunks_exact(2)
            .zip(bytes.iter_mut())
            .try_for_each(|(chunk, byte)| {
                *byte = u8::from_str_radix(
                    std::str::from_utf8(chunk).map_err(|_| HashError::InvalidHex)?,
                    16,
                )
                .map_err(|_| HashError::InvalidHex)?;
                Ok::<_, HashError>(())
            })?;
        Ok(Self(bytes))
    }

    /// Convert to a 64-character lowercase hex string.
    #[must_use]
    pub fn to_hex(&self) -> String {
        const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for byte in &self.0 {
            s.push(HEX_CHARS[usize::from(byte >> 4)] as char);
            s.push(HEX_CHARS[usize::from(byte & 0x0f)] as char);
        }
        s
    }

    /// The zero hash (all zeros). Used as a sentinel value.
    pub const ZERO: Self = Self([0u8; 32]);

    /// Convert to a `blake3::Hash` value.
    #[must_use]
    pub fn as_blake3(&self) -> Blake3Hash {
        Blake3Hash::from_bytes(self.0)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display short form: first 12 hex chars.
        let hex = self.to_hex();
        write!(f, "{}…", &hex[..12])
    }
}

impl From<Blake3Hash> for Hash {
    fn from(h: Blake3Hash) -> Self {
        Self(*h.as_bytes())
    }
}

impl From<[u8; 32]> for Hash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BLAKE3 of the empty string (published test vector).
    const EMPTY_BLAKE3_HEX: &str =
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn hex_roundtrip() -> Result<(), HashError> {
        let h = Hash::from_data(b"hello");
        let parsed = Hash::from_hex(&h.to_hex())?;
        assert_eq!(h, parsed);
        Ok(())
    }

    #[test]
    fn empty_string_vector() -> Result<(), HashError> {
        assert_eq!(Hash::from_data(b"").to_hex(), EMPTY_BLAKE3_HEX);
        assert_eq!(Hash::from_hex(EMPTY_BLAKE3_HEX)?, Hash::from_data(b""));
        Ok(())
    }

    #[test]
    fn rejects_bad_length() {
        assert_eq!(Hash::from_hex("abc"), Err(HashError::InvalidLength(3)));
        assert_eq!(
            Hash::from_hex(&"a".repeat(63)),
            Err(HashError::InvalidLength(63))
        );
    }

    #[test]
    fn rejects_bad_hex() {
        let mut bad = "a".repeat(63);
        bad.push('g');
        assert_eq!(Hash::from_hex(&bad), Err(HashError::InvalidHex));
    }

    #[test]
    fn display_is_short_form() -> Result<(), HashError> {
        let h = Hash::from_hex(EMPTY_BLAKE3_HEX)?;
        assert_eq!(format!("{h}"), "af1349b9f5f9…");
        Ok(())
    }

    #[test]
    fn debug_is_full_hex() -> Result<(), HashError> {
        let h = Hash::from_hex(EMPTY_BLAKE3_HEX)?;
        assert_eq!(format!("{h:?}"), format!("Hash({EMPTY_BLAKE3_HEX})"));
        Ok(())
    }

    #[test]
    fn zero_sentinel() {
        assert_eq!(Hash::ZERO.0, [0u8; 32]);
    }

    #[test]
    fn ordering_is_byte_lexicographic() {
        let mut a = Hash::ZERO;
        a.0[0] = 0x00;
        let mut b = Hash::ZERO;
        b.0[0] = 0x01;
        assert!(a < b);
    }
}
