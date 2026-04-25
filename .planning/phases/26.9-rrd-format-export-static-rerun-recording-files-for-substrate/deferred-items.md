# Phase 26.9 — Deferred Items

Out-of-scope discoveries logged during plan execution. Each item is **not blocking** for the discovering plan but should be addressed by a subsequent plan or maintenance pass.

## Plan 03 Discoveries

### Plan 02 test code triggers `clippy::option_as_ref_deref` under `--tests`

- **Discovered:** Plan 03 Task 2 verification, while running `cargo clippy -p roz-cli --features export-rrd --tests -- -D warnings`
- **Files:** `crates/roz-cli/src/commands/mcap/mod.rs` lines 154, 158, 184
- **Issue:** Plan 02's `parse_mcap_to_rrd_*` tests use `to_rrd_args.input.as_ref().map(|p| p.as_path())`, which clippy flags as `clippy::option_as_ref_deref` ("called `.as_ref().map(|p| p.as_path())` on an `Option` value — consider using `as_deref`").
- **Why deferred:** Plan 03's acceptance criterion is `cargo clippy -p roz-cli --features export-rrd -- -D warnings` (no `--tests` flag). Plan 03 changes neither `mod.rs` test code nor the function signatures the tests use; this is pre-existing Plan 02 code. The fix is mechanical (`as_ref().map(|p| p.as_path())` → `as_deref()`) and could be safely applied in any maintenance pass or as a Rule 1 fix in a future plan that touches `mod.rs` tests.
- **Suggested fix location:** Plan 04, 05, 06, 07, or 08 — whichever first runs `cargo clippy ... --tests` as part of its verify block. Or a dedicated maintenance pass.
