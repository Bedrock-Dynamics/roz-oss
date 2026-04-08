---
phase: 04-service-implementation
reviewed: 2026-04-08T12:00:00Z
depth: standard
files_reviewed: 9
files_reviewed_list:
  - crates/roz-db/src/embodiments.rs
  - crates/roz-db/src/lib.rs
  - crates/roz-server/src/grpc/embodiment.rs
  - crates/roz-server/src/grpc/mod.rs
  - crates/roz-server/src/lib.rs
  - crates/roz-server/src/main.rs
  - crates/roz-server/src/routes/hosts.rs
  - crates/roz-worker/src/registration.rs
  - migrations/022_host_embodiment.sql
findings:
  critical: 0
  warning: 2
  info: 3
  total: 5
status: issues_found
---

# Phase 4: Code Review Report

**Reviewed:** 2026-04-08T12:00:00Z
**Depth:** standard
**Files Reviewed:** 9
**Status:** issues_found

## Summary

The EmbodimentService gRPC implementation, REST endpoint, DB module, worker upload function, and migration are well-structured and follow existing codebase conventions. Authentication and tenant isolation are properly enforced through the `fetch_embodiment_row` helper and pre-existing REST middleware patterns. The migration is correctly numbered (022 follows 021). No critical security issues found.

Two warnings relate to information leakage via serde error details in gRPC status messages, and a copy-paste bug in error messages. Three informational items cover minor issues in error context strings and the REST endpoint's lack of schema validation on JSONB input.

## Warnings

### WR-01: Serde deserialization errors leaked to gRPC clients

**File:** `crates/roz-server/src/grpc/embodiment.rs:109,128,150,178`
**Issue:** All four `Status::internal(format!("corrupt model data: {e}"))` calls embed the full `serde_json::Error` in the gRPC status message returned to clients. Serde errors can reveal internal struct field names, expected types, and schema details that aid attackers in crafting payloads or understanding storage internals.
**Fix:** Log the full error server-side (already done via `tracing::error!`) and return a generic message to the client:
```rust
Status::internal("failed to deserialize embodiment data")
```

### WR-02: Copy-paste error in runtime deserialization error messages

**File:** `crates/roz-server/src/grpc/embodiment.rs:128,178`
**Issue:** In `get_runtime` (line 128) and `validate_bindings` (line 178), the `Status::internal` message says "corrupt model data" when the operation is actually deserializing runtime data. The `tracing::error!` on the preceding line correctly says "corrupt runtime data", but the client-facing status is wrong. This will confuse debugging when a client reports the error.
**Fix:**
```rust
// Line 128 (get_runtime)
Status::internal("corrupt runtime data")

// Line 178 (validate_bindings)
Status::internal("corrupt runtime data")
```
Note: If WR-01 is also applied, both fixes collapse into the same generic message.

## Info

### IN-01: Literal `{id}` in anyhow context strings

**File:** `crates/roz-worker/src/registration.rs:145,147`
**Issue:** The `.context()` strings contain `{id}` as literal text, not interpolated. The actual URL on line 140 correctly interpolates `host_id` via `format!`, but the error context shows the literal string `PUT /v1/hosts/{id}/embodiment` instead of the resolved UUID.
**Fix:**
```rust
.context(format!("PUT /v1/hosts/{host_id}/embodiment request failed"))?
.error_for_status()
.context(format!("PUT /v1/hosts/{host_id}/embodiment returned error status"))?;
```

### IN-02: No schema validation on REST embodiment upload

**File:** `crates/roz-server/src/routes/hosts.rs:154-157`
**Issue:** `UpdateEmbodimentRequest.model` is typed as `serde_json::Value`, allowing any JSON to be stored in the `embodiment_model` JSONB column. If a client sends malformed JSON (e.g., `{"garbage": true}`), the gRPC read path will fail at deserialization time with an opaque "corrupt model data" error. The worker uses strongly-typed `EmbodimentModel` so this is unlikely in normal operation, but the REST API is publicly accessible to any authenticated tenant.
**Fix:** Consider deserializing into `roz_core::embodiment::model::EmbodimentModel` at the REST boundary and returning a 422 on failure, or accept this as a known limitation documented by the `FAILED_PRECONDITION` handling in the gRPC layer.

### IN-03: Unused import in `grpc/mod.rs` test module

**File:** `crates/roz-server/src/grpc/mod.rs:27`
**Issue:** The test module has `#[allow(unused_imports)]` applied broadly. This suppresses warnings for any import that becomes stale as the proto schema evolves. The attribute was likely added to cover the large import list used for generated-type verification, but it masks future dead-import warnings.
**Fix:** Remove `#[allow(unused_imports)]` and delete individual imports as tests that reference them are removed, or convert to targeted `#[allow]` on specific imports that are only used for type-existence assertions.

---

_Reviewed: 2026-04-08T12:00:00Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
