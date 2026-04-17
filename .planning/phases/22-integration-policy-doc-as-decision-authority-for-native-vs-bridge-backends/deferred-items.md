# Deferred items — Phase 22-03

## Pre-existing clippy error in roz-core (not caused by this plan)

- **File:** `crates/roz-core/src/schedule.rs` line 183
- **Lint:** `clippy::unnecessary-wraps` on `fn occurrences_between` (returns `Result<Vec<...>, ScheduleError>` but the error path was removed in an earlier commit)
- **Verification:** `git stash`-ed the 22-03 io.rs change and ran `cargo clippy -p roz-copper -- -D warnings` — same error reproduces on unmodified `HEAD` (commit 29477208). Not introduced by 22-03's docstring edit.
- **Scope boundary:** Out of scope for plan 22-03 (doc-only edit). Should be fixed by a separate plan or tracked as a broader v2.2/v3.0 cleanup item.
- **Date logged:** 2026-04-17
