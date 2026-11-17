# PATTERN_ADOPTION

Relationship to the WyattAu pattern canon (`pattern-standards`),
machine-checked by `pattern-standards/scripts/adoption-check.py`.

| Pattern | Status | Evidence |
|---|---|---|
| T28 | partial | src/store.rs:169 (BLAKE3 hash is the storage address; a second write of identical bytes is a dedup no-op, src/store.rs:173). Merkle DAG structure is absent by design: objects are opaque with no embedded tree pointers, so gc::mark requires the host-supplied transitive closure (src/gc.rs:192) |
| T29 | partial | src/store.rs:174 (identical content is stored once: storage-level structural sharing); src/gc.rs:533 (identical keep-sets converge to the same content-derived pack name, and a replacement pack is fully written before the old pair is removed, so readers never see torn state). No MVCC or versioned snapshots |
| T13 | partial | src/gc.rs:346 (plan_sweep classifies present versus live into a SweepPlan) and src/gc.rs:494 (sweep executes the plan). Decision and execution are separated, though planning enumerates store state, so I/O sits inside the decision step |
| P64 | adopted | tests/proptest.rs:11 (arbitrary-bytes put/get roundtrips with and without compression, hash stability) |
| P116 | partial | src/store.rs:225 (put_blob_with_hash verifies a caller-declared digest before accept) and src/compressor.rs:25 (1 GiB decompression cap against zip bombs). Library primitive with no upload API surface; path traversal is impossible by construction because object names are hash-derived |
