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

## Security

| ID | Requirement | Priority |
|----|-------------|----------|
| REQ-CAS-100 | Hash addressing makes stored bytes self-verifying: any bit corruption is detectable on next read/verify | MUST |
| REQ-CAS-101 | Hash values are used as path components only after hex validation (no traversal) | MUST |

## Observability & API hygiene

| ID | Requirement | Priority |
|----|-------------|----------|
| REQ-CAS-900 | All fallible public APIs return typed errors; production `unwrap`/`expect` is denied or explicitly justified with an invariant comment | MUST |
| REQ-CAS-901 | Public items carry doc comments with runnable examples where practical | SHOULD |

Reviewed: 2026-09-11
