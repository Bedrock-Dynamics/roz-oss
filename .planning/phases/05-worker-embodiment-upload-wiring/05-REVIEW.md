---
phase: 05-worker-embodiment-upload-wiring
reviewed: 2026-04-08T00:00:00Z
depth: standard
files_reviewed: 5
files_reviewed_list:
  - crates/roz-db/src/embodiments.rs
  - crates/roz-server/src/routes/hosts.rs
  - crates/roz-worker/src/config.rs
  - crates/roz-worker/src/main.rs
  - crates/roz-worker/src/registration.rs
findings:
  critical: 0
  warning: 2
  info: 2
  total: 4
status: issues_found
---

# Phase 05: Code Review Report

**Reviewed:** 2026-04-08
**Depth:** standard
**Files Reviewed:** 5
**Status:** issues_found

## Summary

Reviewed the worker embodiment upload wiring added in phase 05. The implementation covers: a new `embodiments` DB module with conditional upsert, a `PUT /v1/hosts/:id/embodiment` REST endpoint, a `robot_toml` config field, manifest loading in `main.rs`, and an `upload_embodiment` client function in `registration.rs`.

The code is well-structured, follows existing project conventions, and has good test coverage. Tenant isolation is correctly enforced at the REST handler level. The conditional upsert with `IS DISTINCT FROM` is a solid pattern for idempotent uploads. Two warnings relate to missing input validation on the server endpoint and a redundant HTTP client allocation on the worker side.

## Warnings

### WR-01: No payload size or schema validation on update_embodiment endpoint

**File:** `crates/roz-server/src/routes/hosts.rs:154-157`
**Issue:** The `UpdateEmbodimentRequest` accepts `model: serde_json::Value` -- any arbitrary JSON. There is no validation that the model contains the expected fields (e.g., `model_digest`, `joints`), nor any body size limit. A caller could store arbitrarily large or malformed JSON in the `embodiment_model` column, which the `conditional_upsert` would accept. The `model_digest` comparison in `conditional_upsert` silently treats a missing `model_digest` key as NULL, so a payload without `model_digest` always triggers a write.
**Fix:** Add basic validation in the handler before calling `conditional_upsert`. At minimum, verify the `model_digest` field is present and is a string:
```rust
if body.model.get("model_digest").and_then(|v| v.as_str()).is_none() {
    return Err(AppError::bad_request("model must contain a non-null model_digest field"));
}
```
Consider adding an Axum `RequestBodyLimit` layer on this route for defense-in-depth.

### WR-02: upload_embodiment creates a new reqwest::Client per call

**File:** `crates/roz-worker/src/registration.rs:129-131`
**Issue:** `upload_embodiment` builds a fresh `reqwest::Client` on every call. In `main.rs` this is called once at startup so the impact is negligible today, but the function's public API signature suggests reuse. The `register_host` function in the same file also builds a new client per call. If either function is called in a loop or retry path in the future, this will allocate a new connection pool and TLS session each time.
**Fix:** Accept a `&reqwest::Client` parameter (or reuse the existing `http` client from main) instead of constructing one internally:
```rust
pub async fn upload_embodiment(
    client: &reqwest::Client,
    api_url: &str,
    api_key: &str,
    host_id: Uuid,
    model: &roz_core::embodiment::model::EmbodimentModel,
    runtime: Option<&roz_core::embodiment::embodiment_runtime::EmbodimentRuntime>,
) -> Result<()> {
```

## Info

### IN-01: Context strings contain literal `{id}` instead of the actual host_id

**File:** `crates/roz-worker/src/registration.rs:148,150`
**Issue:** The `.context()` error messages contain the literal text `{id}` rather than interpolating the actual `host_id` value. This makes error diagnostics harder when debugging upload failures.
**Fix:** Use `format!` to include the actual host_id:
```rust
.context(format!("PUT /v1/hosts/{host_id}/embodiment request failed"))?
.error_for_status()
.context(format!("PUT /v1/hosts/{host_id}/embodiment returned error status"))?;
```

### IN-02: Test coverage for upload_embodiment is shape-only

**File:** `crates/roz-worker/src/registration.rs:209-225`
**Issue:** The `upload_embodiment_body_has_model_key` and `upload_embodiment_body_with_runtime` tests only verify the JSON body shape using manually constructed `serde_json::json!` values. They do not exercise the actual `upload_embodiment` function or verify that `serde_json::to_value(model)` produces the expected shape from a real `EmbodimentModel` struct. These tests would not catch a serialization mismatch between the Rust type and the expected JSON contract.
**Fix:** Consider adding a test that constructs a minimal `EmbodimentModel` struct, serializes it with `serde_json::to_value`, and asserts the result contains the expected keys (e.g., `model_digest`). This would catch any `#[serde(rename)]` or `#[serde(skip)]` surprises.

---

_Reviewed: 2026-04-08_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
