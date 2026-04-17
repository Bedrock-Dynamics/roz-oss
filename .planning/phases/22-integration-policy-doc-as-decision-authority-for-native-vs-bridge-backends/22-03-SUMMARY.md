---
phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends
plan: 03
subsystem: docs
tags: [documentation, rustdoc, readme, discoverability, integration-policy, roz-copper]

# Dependency graph
requires:
  - phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends
    provides: docs/integration-policy.md published under plan 22-01 (policy doc) and cross-referenced via PR template + CONTRIBUTING from plan 22-02
provides:
  - Module-level docstring pointer from crates/roz-copper/src/io.rs to docs/integration-policy.md (D-10)
  - Front-page Documentation section in README.md linking to docs/integration-policy.md (D-11)
  - Verified negative assertion that CLAUDE.md is NOT modified and contains no integration-policy reference (D-12)
affects:
  - Phase 25 MAVLink-native backend authors landing in crates/roz-copper/src/io.rs
  - First-time repo visitors browsing README.md
  - Future Spot / Franka / ROS2 / UR / Stretch backend PRs

# Tech tracking
tech-stack:
  added: []
  patterns:
    - Module-docstring → /docs cross-link via backticked path in `//!` comment (rustfmt + clippy-doc_markdown safe)
    - README front-page discoverability via standalone `## Documentation` section between `## Examples` and `## Status`

key-files:
  created: []
  modified:
    - crates/roz-copper/src/io.rs
    - README.md

key-decisions:
  - Used the recommended 3-line docstring form (line 1 existing prose, line 2 blank `//!` paragraph break, line 3 backticked pointer) per Phase 22 research Pitfall 2 guidance — rustfmt does not rewrap at 120-char max_width and clippy doc_markdown accepts the backticked path
  - Used `cargo clippy -p roz-copper --no-deps -- -D warnings` to scope the lint gate to the roz-copper crate itself; a pre-existing unrelated clippy error in roz-core/src/schedule.rs exists on unmodified HEAD and is logged as a deferred item rather than auto-fixed (out of scope per plan 22-03)
  - Placed the new README `## Documentation` section between `## Examples` (line 68) and `## Status` (line 75), matching the planner's recommended insertion point in §22-RESEARCH.md Existing File Shapes §README.md

patterns-established:
  - "Doc cross-link from code: extend existing `//!` module docstring with a blank `//!` paragraph-break line + one-line `` `path/to/doc.md` `` pointer"
  - "Front-page doc discoverability: insert a minimal `## Documentation` H2 section (heading + blank + single bullet + blank) above `## Status` rather than nesting the link inside existing sections"

requirements-completed: [INT-01]

# Metrics
duration: 3m13s
completed: 2026-04-17
---

# Phase 22 Plan 03: Cross-link integration-policy doc from io.rs and README — Summary

**Cross-linked `docs/integration-policy.md` from two discovery surfaces: the `roz-copper` trait-surface module docstring (D-10) and a new README `## Documentation` section (D-11); CLAUDE.md left untouched per D-12.**

## Performance

- **Duration:** 3m13s
- **Started:** 2026-04-17T14:01:25Z
- **Completed:** 2026-04-17T14:04:38Z
- **Tasks:** 2
- **Files modified:** 2

## Accomplishments

- `crates/roz-copper/src/io.rs` module docstring extended to the canonical 3-line form with a backticked pointer to `docs/integration-policy.md` — future backend authors landing at the trait surface see the policy pointer immediately.
- `README.md` now has a `## Documentation` section between `## Examples` and `## Status` with a single bullet `[Integration policy](docs/integration-policy.md) — decision authority for native-vs-bridge backends (MAVLink, Gazebo, Spot, Franka, ROS2).`
- `cargo fmt --check -p roz-copper` and `cargo clippy -p roz-copper --no-deps -- -D warnings` both exit 0 after the docstring change.
- D-12 negative assertion holds: `CLAUDE.md` is absent from the worktree (gitignored at repo root), and `git diff --name-only HEAD -- CLAUDE.md` returns empty.

## Task Commits

Each task was committed atomically:

1. **Task 1: Extend io.rs module docstring with doc pointer (D-10)** — `23f6ea8` (docs)
2. **Task 2: Add README `## Documentation` section + confirm CLAUDE.md unchanged (D-11, D-12)** — `27554c0` (docs)

## Files Created/Modified

- `crates/roz-copper/src/io.rs` — module docstring extended from 1 line to the recommended 3-line form (`//! Pluggable IO traits for the controller loop.` / `//!` / `` //! Backend-choice policy: see `docs/integration-policy.md`. ``). Use statements, traits, and struct definition byte-for-byte unchanged.
- `README.md` — inserted a 4-line block (`## Documentation` heading + blank + Integration-policy bullet + blank) between the existing `## Examples` block (line 68) and `## Status` (line 75 → new line 79). H2 count went from 7 → 8; all 7 pre-existing H2s preserved verbatim.

## Top-of-`io.rs` verbatim (first 5 lines)

```
//! Pluggable IO traits for the controller loop.
//!
//! Backend-choice policy: see `docs/integration-policy.md`.

use roz_core::command::CommandFrame;
```

## `cargo fmt` and `cargo clippy` exit statuses

| Command | Exit | Notes |
|---------|------|-------|
| `cargo fmt --check -p roz-copper` | `0` | Recommended 3-line docstring form is well under 120-char max_width; rustfmt leaves doc-comments alone |
| `cargo check -p roz-copper` | `0` | Crate compiles; docstring change has no effect on the crate's API surface |
| `cargo clippy -p roz-copper --no-deps -- -D warnings` | `0` | No new clippy warnings introduced in roz-copper by the docstring addition; backticked `docs/integration-policy.md` satisfies doc_markdown |
| `cargo clippy -p roz-copper -- -D warnings` (with deps) | `101` | Pre-existing `clippy::unnecessary_wraps` error on `roz_core::schedule::occurrences_between` (line 183) reproduces on unmodified HEAD — unrelated to this plan, logged to `deferred-items.md` |

## README H2 count

- **Before:** 7 (`What Roz Is`, `Quick Start`, `Repo Layout`, `Running Tests`, `Examples`, `Status`, `License`)
- **After:** 8 (added `Documentation` between `Examples` and `Status`)
- Monotonic order verified: `## Examples` at line 68 < `## Documentation` at line 75 < `## Status` at line 79.

## CLAUDE.md diff summary (D-12 negative assertion)

```bash
$ git diff --name-only HEAD -- CLAUDE.md
(empty — no diff)

$ git diff --stat HEAD -- CLAUDE.md
(empty — no lines changed)

$ ls CLAUDE.md 2>&1
ls: CLAUDE.md: No such file or directory
```

Per the main-checkout `.gitignore` line 13 (`CLAUDE.md`), `CLAUDE.md` is gitignored at the repo root and therefore never present in a fresh worktree checkout. D-12's negative assertion (`if [ -f CLAUDE.md ]; then ! grep -q 'integration-policy' CLAUDE.md; fi`) is vacuously satisfied, and the stricter form (`test -z "$(git diff --name-only HEAD -- CLAUDE.md)"`) is also satisfied because there is no diff.

## Decisions Made

- **Scoped clippy check with `--no-deps`:** The CLAUDE.md and plan 22-03 CI gate is that the docstring edit introduce no new clippy warnings. A workspace-wide `cargo clippy -p roz-copper -- -D warnings` fails on a pre-existing `clippy::unnecessary_wraps` error in `roz-core/src/schedule.rs:183` that reproduces on unmodified HEAD (verified via `git stash` + rerun). Per the plan's scope boundary, this is out of scope for a doc-only edit, so the task-specific gate is `--no-deps` on `roz-copper`. The pre-existing error is logged to `deferred-items.md` for a future plan to pick up.
- **Placed `## Documentation` above `## Status`:** Matches the planner's recommendation in `22-RESEARCH.md §Existing File Shapes §README.md`. Keeps the reader's attention on technical content before the research-preview notice in `## Status`.
- **Did not add pointers to any per-impl file (e.g., Gazebo sensor bridge):** D-10 explicitly restricts the pointer to the trait surface only.

## Deviations from Plan

### Auto-fixed Issues

None that changed scope. One scoped test decision (`--no-deps` on clippy) was needed to avoid an unrelated pre-existing workspace clippy error; this is documented above under Decisions Made and in `deferred-items.md`, but it does not change any file besides `io.rs` and `README.md`.

---

**Total deviations:** 0 (plan executed exactly as written)
**Impact on plan:** None. Both edits landed in the exact shape specified by the plan's `files_modified` and `artifacts` fields.

## Issues Encountered

- **Pre-existing roz-core clippy error surfaced by the cargo-clippy workspace gate.** A `cargo clippy -p roz-copper -- -D warnings` run (which includes dependencies) fails on a `clippy::unnecessary_wraps` error in `crates/roz-core/src/schedule.rs:183` that reproduces on unmodified `29477208` HEAD. Confirmed via `git stash` + re-run. Logged to `.planning/phases/22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends/deferred-items.md`. Out of scope for this doc-only plan.

## Deferred Issues

See `.planning/phases/22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends/deferred-items.md` for the pre-existing `clippy::unnecessary_wraps` finding in `roz-core/src/schedule.rs`.

## User Setup Required

None — no external service configuration required.

## Next Phase Readiness

- `docs/integration-policy.md` is now discoverable from both the code trait surface (`crates/roz-copper/src/io.rs`) and the repo front-page (`README.md`), satisfying INT-01's "cited as decision authority" requirement.
- Phase 25 (MAVLink-native backend) can cite `docs/integration-policy.md` without needing a README or source-code grep to find it.
- No blockers; no follow-on work is required from plan 22-03.

## Self-Check: PASSED

- `crates/roz-copper/src/io.rs` line 1-3: FOUND (`//! Pluggable IO traits for the controller loop.` / `//!` / `` //! Backend-choice policy: see `docs/integration-policy.md`. ``)
- `README.md` contains `^## Documentation`: FOUND
- `README.md` contains `[Integration policy](docs/integration-policy.md)`: FOUND
- `README.md` H2 count = 8: FOUND
- Commit `23f6ea8` (Task 1): FOUND (`git log --oneline | grep 23f6ea8` succeeds)
- Commit `27554c0` (Task 2): FOUND
- `cargo fmt --check -p roz-copper`: exit 0
- `cargo clippy -p roz-copper --no-deps -- -D warnings`: exit 0
- `CLAUDE.md` unchanged (gitignored, not in worktree): VERIFIED

---
*Phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends*
*Completed: 2026-04-17*
