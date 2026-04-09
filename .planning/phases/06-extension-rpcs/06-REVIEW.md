---
phase: 06-extension-rpcs
reviewed: 2026-04-08T20:15:00Z
depth: standard
files_reviewed: 3
files_reviewed_list:
  - crates/roz-server/src/grpc/embodiment_convert.rs
  - crates/roz-server/src/grpc/embodiment.rs
  - proto/roz/v1/embodiment.proto
findings:
  critical: 0
  warning: 2
  info: 2
  total: 4
status: issues_found
---

# Phase 6: Code Review Report

**Reviewed:** 2026-04-08T20:15:00Z
**Depth:** standard
**Files Reviewed:** 3
**Status:** issues_found

## Summary

Reviewed the EmbodimentService gRPC implementation, proto-to-domain conversion layer, and protobuf definition. The code is well-structured with comprehensive bidirectional conversions, thorough round-trip tests (including property-based tests via proptest), and correct tenant isolation in the service layer. Two warnings found: silent data loss in FrameTree reconstruction and a file-level lint suppression that contradicts project conventions.

## Warnings

### WR-01: FrameTree BFS silently drops orphaned nodes

**File:** `crates/roz-server/src/grpc/embodiment_convert.rs:1089-1113`
**Issue:** The `TryFrom<roz_v1::FrameTree> for FrameTree` BFS reconstruction silently drops any proto frame nodes whose `parent_id` chain does not connect to the root. If the proto contains a frame with a parent_id referencing a non-existent frame, or a disconnected subtree, those nodes are silently lost. The conversion returns `Ok(tree)` with fewer frames than the input, which violates the type-fidelity constraint ("no field drops").
**Fix:** After the BFS loop completes, compare `visited.len()` against `proto.frames.len()`. If they differ, return an error listing the orphaned frame IDs:
```rust
if visited.len() != proto.frames.len() {
    let orphaned: Vec<&str> = proto.frames.keys()
        .filter(|k| !visited.contains(k.as_str()))
        .map(String::as_str)
        .collect();
    return Err(EmbodimentConvertError::MissingField(
        format!("FrameTree has {} orphaned frames: {:?}", orphaned.len(), orphaned)
    ));
}
```

### WR-02: File-level lint suppression instead of targeted allow

**File:** `crates/roz-server/src/grpc/embodiment.rs:1`
**Issue:** `#![allow(clippy::result_large_err)]` is applied at file scope. Project convention (from CLAUDE.md) says: "use targeted `#[allow(...)]` or `#[expect(..., reason = "...")]` directly on the item, not broad crate-wide suppression." This blanket allow could mask future large-error issues in new functions added to this file.
**Fix:** Remove the file-level attribute and apply targeted `#[allow(clippy::result_large_err)]` on the `EmbodimentService` trait impl block, or use `#[expect(clippy::result_large_err, reason = "tonic Status is the return error type for all gRPC RPCs")]` on the impl block.

## Info

### IN-01: Repeated model/runtime deserialization boilerplate

**File:** `crates/roz-server/src/grpc/embodiment.rs:154-163,196-203,275-283,309-317`
**Issue:** The pattern `row.embodiment_model.ok_or_else(...)` followed by `serde_json::from_value(json).map_err(...)` is repeated four times for model deserialization and twice for runtime deserialization. This is a mild DRY violation.
**Fix:** Extract into helpers like `fn deserialize_model(row: &EmbodimentRow, host_id: Uuid) -> Result<EmbodimentModel, Status>` and `fn deserialize_runtime(row: &EmbodimentRow, host_id: Uuid) -> Result<EmbodimentRuntime, Status>`.

### IN-02: Negative proto Timestamp nanos silently zeroed

**File:** `crates/roz-server/src/grpc/embodiment_convert.rs:61`
**Issue:** `u32::try_from(ts.nanos).unwrap_or(0)` silently converts negative nanos to 0. Per the protobuf `Timestamp` well-known type spec, nanos must be non-negative, so this is defensively correct. However, a malformed Timestamp with negative nanos would lose subsecond precision without any warning or error.
**Fix:** No action required -- the current behavior is a reasonable defensive default. If stricter validation is desired, return `InvalidTimestamp` when nanos is negative.

---

_Reviewed: 2026-04-08T20:15:00Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: standard_
