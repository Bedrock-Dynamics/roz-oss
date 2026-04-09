---
phase: 06-extension-rpcs
fixed_at: 2026-04-08T20:25:00Z
review_path: .planning/phases/06-extension-rpcs/06-REVIEW.md
iteration: 1
findings_in_scope: 2
fixed: 2
skipped: 0
status: all_fixed
---

# Phase 6: Code Review Fix Report

**Fixed at:** 2026-04-08T20:25:00Z
**Source review:** .planning/phases/06-extension-rpcs/06-REVIEW.md
**Iteration:** 1

**Summary:**
- Findings in scope: 2
- Fixed: 2
- Skipped: 0

## Fixed Issues

### WR-01: FrameTree BFS silently drops orphaned nodes

**Files modified:** `crates/roz-server/src/grpc/embodiment_convert.rs`
**Commit:** fe0e4ea
**Applied fix:** Added a post-BFS check comparing `visited.len()` against `proto.frames.len()`. When they differ, the conversion now returns `EmbodimentConvertError::MissingField` listing the orphaned frame IDs instead of silently dropping them.

### WR-02: File-level lint suppression instead of targeted allow

**Files modified:** `crates/roz-server/src/grpc/embodiment.rs`
**Commit:** a097e21
**Applied fix:** Removed the file-level `#![allow(clippy::result_large_err)]` attribute and added a targeted `#[expect(clippy::result_large_err, reason = "tonic Status is the return error type for all gRPC RPCs")]` on the `EmbodimentService` impl block, matching project conventions.

---

_Fixed: 2026-04-08T20:25:00Z_
_Fixer: Claude (gsd-code-fixer)_
_Iteration: 1_
