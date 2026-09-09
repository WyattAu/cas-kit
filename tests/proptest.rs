// Tests assert invariants directly; unwraps keep failures loud.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Property-based tests for `cas-kit`.
//!
//! Covers: arbitrary-bytes put/get roundtrips with and without
//! compression, hash stability (including the published BLAKE3 empty
//! vector), and "delta of delta" — blobs derived from other blobs
//! (delta blobs) roundtrip and address stably.

use proptest::prelude::*;

use cas_kit::{BlobStore, Hash};

/// Convert any fallible step into a proptest failure (no unwraps).
fn soft<T, E: std::fmt::Display>(r: Result<T, E>) -> Result<T, TestCaseError> {
    r.map_err(|e| TestCaseError::fail(e.to_string()))
}

fn arb_bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(proptest::num::u8::ANY, 0..max)
}

/// A byte vector biased to contain zeros and repetition (compressible and
/// binary-like content).
fn arb_compressible(max: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        prop_oneof![Just(0u8), Just(0u8), (0u8..=255), (0u8..=16),],
        0..max,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn roundtrip_with_compression(data in arb_bytes(4096)) {
        let dir = soft(tempfile::tempdir())?;
        let store = soft(BlobStore::new(dir.path()))?;
        let hash = soft(store.put_blob(&data))?;
        let out = soft(store.get_blob(&hash))?;
        prop_assert_eq!(out, data);
    }

    #[test]
    fn roundtrip_without_compression(data in arb_bytes(4096)) {
        let dir = soft(tempfile::tempdir())?;
        let store = soft(BlobStore::new_uncompressed(dir.path()))?;
        let hash = soft(store.put_blob(&data))?;
        let out = soft(store.get_blob(&hash))?;
        prop_assert_eq!(out, data);
    }

    #[test]
    fn roundtrip_compressible_content(data in arb_compressible(8192)) {
        let dir = soft(tempfile::tempdir())?;
        let store = soft(BlobStore::new(dir.path()))?;
        let hash = soft(store.put_blob(&data))?;
        let out = soft(store.get_blob(&hash))?;
        prop_assert_eq!(out, data);
    }

    #[test]
    fn hash_stability_across_stores(data in arb_bytes(2048)) {
        let d1 = soft(tempfile::tempdir())?;
        let d2 = soft(tempfile::tempdir())?;
        let s1 = soft(BlobStore::new_uncompressed(d1.path()))?;
        let s2 = soft(BlobStore::new(d2.path()))?;

        let h1 = soft(s1.put_blob(&data))?;
        let h2 = soft(s2.put_blob(&data))?;
        prop_assert_eq!(h1, h2, "hash must depend only on content");

        // Re-putting in the same store yields the same address.
        let h1_again = soft(s1.put_blob(&data))?;
        prop_assert_eq!(h1, h1_again);

        // The parsed hex form round-trips.
        let parsed = soft(Hash::from_hex(&h1.to_hex()))?;
        prop_assert_eq!(parsed, h1);
    }

    #[test]
    fn distinct_content_distinct_hash(data1 in arb_bytes(512), data2 in arb_bytes(512)) {
        prop_assume!(data1 != data2);
        let h1 = Hash::from_data(&data1);
        let h2 = Hash::from_data(&data2);
        prop_assert_ne!(h1, h2);
    }

    /// "Delta of delta": blobs derived from stored blobs (a delta blob,
    /// then a delta of that delta) roundtrip and are addressed stably.
    #[test]
    fn delta_of_delta_blobs_roundtrip(a in arb_bytes(1024), b in arb_bytes(1024)) {
        let dir = soft(tempfile::tempdir())?;
        let store = soft(BlobStore::new_uncompressed(dir.path()))?;

        // Trivial XOR delta: d1 = a XOR b (over the shorter length), then
        // d2 = delta of the delta: a XOR d1. Every layer is just another
        // blob to the store and must survive an arbitrary number of
        // derivations.
        let d1: Vec<u8> = a.iter().zip(b.iter()).map(|(x, y)| x ^ y).collect();
        let d2: Vec<u8> = a.iter().zip(d1.iter()).map(|(x, y)| x ^ y).collect();

        let _ha = soft(store.put_blob(&a))?;
        let hd1 = soft(store.put_blob(&d1))?;
        let hd2 = soft(store.put_blob(&d2))?;

        let got_d1 = soft(store.get_blob(&hd1))?;
        let got_d2 = soft(store.get_blob(&hd2))?;
        prop_assert_eq!(got_d1.as_slice(), d1.as_slice());
        prop_assert_eq!(got_d2.as_slice(), d2.as_slice());

        // Address stability for derived blobs.
        prop_assert_eq!(hd2, soft(store.put_blob(&d2))?);
    }
}

/// The BLAKE3 empty-string vector anchors hash stability across releases.
#[test]
fn blake3_known_vector() {
    assert_eq!(
        Hash::from_data(b"").to_hex(),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
}
