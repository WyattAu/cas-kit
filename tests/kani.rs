#![cfg(kani)]
//! Kani bounded model-checking harnesses for the 2-hex-prefix bucketing
//! scheme of the content-addressed store layout
//! (`objects/<2-hex-prefix>/<62-hex-filename>`).
//!
//! # Properties (over an arbitrary 32-byte BLAKE3 digest)
//!
//! 1. **Bucket bound** (`kani_bucket_matches_hex_prefix`): `Hash::bucket()`
//!    is a byte, so the bucket index is always `< 256` — the layout's
//!    bucket-space bound (256 directories). Documented as an assertion; the
//!    type system guarantees it.
//! 2. **Hex agreement** (same harness): the first two characters of
//!    `to_hex()` are exactly the lowercase-hex encoding of `bucket()` —
//!    the hex text form and the bucket index can never disagree. This is
//!    what makes the `&hex[..2]` / `&hex[2..]` split in
//!    `BlobStore::blob_path` infallible and consistent (every path maps a
//!    hash under the directory named by its bucket; the comment in
//!    `store.rs` references this harness).
//! 3. **Roundtrip stability** (`kani_hex_roundtrip_stability`):
//!    `from_hex(to_hex(h)) == h` and the bucket is unchanged under the
//!    roundtrip — a blob's path never depends on which side of a hex
//!    serialization produced it.
//!
//! The two harnesses are separate so the cheap prefix property (which
//! depends on the digest's first byte only) verifies independently of the
//! full 256-bit roundtrip instance.
//!
//! Run with:
//! ```text
//! cargo kani --tests
//! ```

use cas_kit::Hash;

/// Value of a lowercase hex digit, or a sentinel above 15 for non-hex
/// input (the harnesses assert the sentinel never appears).
fn hex_val(b: u8) -> u32 {
    match b {
        b'0'..=b'9' => u32::from(b - b'0'),
        b'a'..=b'f' => u32::from(b - b'a') + 10,
        _ => 99,
    }
}

#[kani::proof]
#[kani::unwind(40)]
#[kani::solver(kissat)]
fn kani_bucket_matches_hex_prefix() {
    let bytes: [u8; 32] = kani::any();
    let h = Hash(bytes);

    // Property 1: bucket index < 256 (2-hex-prefix bucket space).
    kani::assert(
        u32::from(h.bucket()) < 256,
        "bucket is a 2-hex-prefix index in 0..256",
    );

    // Property 2: the hex form agrees with the bucket — `blob_path`'s
    // `&hex[..2]` bucket directory is exactly `bucket()` in hex.
    let hex = h.to_hex();
    kani::assert(hex.len() == 64, "to_hex emits exactly 64 chars");
    let v0 = hex_val(hex.as_bytes()[0]);
    let v1 = hex_val(hex.as_bytes()[1]);
    kani::assert(v0 < 16 && v1 < 16, "hex chars are lowercase hex digits");
    kani::assert(
        v0 * 16 + v1 == u32::from(h.bucket()),
        "first 2 hex chars of to_hex encode bucket()",
    );
}

#[kani::proof]
#[kani::unwind(40)]
#[kani::solver(kissat)]
fn kani_hex_roundtrip_stability() {
    let bytes: [u8; 32] = kani::any();
    let h = Hash(bytes);

    // Property 3: roundtrip stability (hex -> parse -> same hash & bucket).
    match Hash::from_hex(&h.to_hex()) {
        Ok(parsed) => {
            kani::assert(parsed == h, "from_hex(to_hex(h)) == h");
            kani::assert(
                parsed.bucket() == h.bucket(),
                "bucket stable under hex roundtrip",
            );
        }
        Err(_) => kani::assert(false, "from_hex always accepts to_hex output"),
    }
}
