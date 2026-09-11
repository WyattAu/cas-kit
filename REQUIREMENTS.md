# Requirements — cas-kit

Numbered, testable requirements. Every requirement maps to at least one named
test or doc-comment contract; security-relevant items cite THREAT-MODEL.md rows.

Scope: Content-addressed storage — SHA-256 addressed immutable blobs with dedup/GC and integrity verification

## Functional

| ID | Requirement | Priority |
|----|-------------|----------|
| REQ-CAS-001 | `put` returns the content hash; `get(hash)` returns byte-identical data or `Err(NotFound)` | MUST |
| REQ-CAS-002 | `verify` re-hashes stored blobs and reports corruption at the offending address | MUST |
| REQ-CAS-003 | GC removes only unreferenced blobs; referenced blobs survive a collection cycle | MUST |
| REQ-CAS-004 | GC mark validates host-supplied roots against physical presence and reports missing roots (objects are opaque: roots are the live set) | MUST |
| REQ-CAS-005 | GC sweep supports a no-op dry run and a recoverable trash mode; delete mode is explicit | MUST |
| REQ-CAS-006 | Pack rewriting preserves every live object byte-for-byte and removes only garbage (coverage rule); corrupt-index packs are never touched | MUST |
| REQ-CAS-007 | Sweeps concurrent with reads of live objects never fail those reads | MUST |

## Security

| ID | Requirement | Priority |
|----|-------------|----------|
| REQ-CAS-100 | Hash addressing makes stored bytes self-verifying: any bit corruption is detectable on next read/verify | MUST |
| REQ-CAS-101 | Hash values are used as path components only after hex validation (no traversal); trash relocation stays inside the store root | MUST |

## Observability & API hygiene

| ID | Requirement | Priority |
|----|-------------|----------|
| REQ-CAS-900 | All fallible public APIs return typed errors; production `unwrap`/`expect` is denied or explicitly justified with an invariant comment | MUST |
| REQ-CAS-901 | Public items carry doc comments with runnable examples where practical | SHOULD |
| REQ-CAS-902 | The `cas-gc` CLI uses only the library API and reports scanned/live/garbage/bytes in both human and `--json` forms | SHOULD |

Reviewed: 2026-09-11

Test mappings: REQ-CAS-001/002 → `tests/integration.rs`; REQ-CAS-003..007 →
`tests/gc.rs` (`mark_then_sweep_full_cycle_leaves_exactly_the_live_set`,
`mark_validates_roots_against_presence`, `dry_run_touches_nothing`,
`trash_mode_is_recoverable`, `sweep_with_overlapping_packs_rewrites_and_dedups`,
`unreadable_pack_is_never_touched`, `sweep_concurrent_with_reads_of_live_objects`);
REQ-CAS-101 → `tests/kani.rs` + `tests/cli.rs`; REQ-CAS-902 → `tests/cli.rs`.
