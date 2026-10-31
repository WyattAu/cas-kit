- **BLAKE3 addressing**; 256-bit content hashes; identical blobs deduplicate
- **Bucketed layout**; blobs live at `objects/<2-hex>/<62-hex>` (256 buckets)
- **Verify-on-read**; every read re-hashes by default; disable per-store for
- **Pack files**; bundle many small blobs into one `.pack` + sorted `.idx`
- **Mark–sweep GC**; `gc::mark` / `gc::sweep` with dry-run, recoverable
- **Ring cache**; bounded in-memory blob cache (1024 entries, MRU-promoted).
- **Zip-bomb safe**; decompression is capped (1 GiB).
- **`Send + Sync`**; share a store across threads via `Arc`. Lock poisoning
- **No unsafe**; `#![forbid(unsafe_code)]`.
3. The second write sees the file already exists and returns immediately -
- Dedup is **global**: any two callers writing the same bytes; from any
  path, process, or "file"; store one copy.
behind. cas-kit objects are opaque; no tree objects, no embedded pointers -
1. **Mark**; you supply the complete live set (every hash your application
2. **Plan**; `gc::plan_sweep` classifies `present − live` as garbage and
3. **Sweep**; `gc::sweep` executes in one of three modes:
  covers; an object that also survives loose, or in a healthy pack, is not
  sharing violation if a reader holds the file open; retry the sweep.
> views); not part of 0.2.0.
- `put/<size>`; hash → Zstd compress → write, fresh store per iteration
- `get/cold/<size>`; disk read + decompress + **verify-on-read**, caches
- `get/noverify/<size>`; same read path with verification disabled
- `pack/create/<size>`; `PackFile::create` over loose objects
deterministic *incompressible* payloads; zstd's worst case, so compressible
² 16 384 objects, each a separate Zstd frame; per-object compression
every read). Turn it off per-store for hot read paths; the on-disk layout
16 KiB deterministic incompressible bytes; 50% unique, 20% near-duplicates
(same content, one byte changed), 30% exact duplicates; and measures
| Re-ingest of an already-stored corpus (all hits) | ~518 MiB/s (30 ms); 10× faster than cold ingest |
Granularity note: cas-kit has **no chunking**; dedup is whole-object.
Run `cargo bench --bench dedup` to reproduce; the corpus report prints