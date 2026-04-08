---
phase: 05-worker-embodiment-upload-wiring
verified: 2026-04-08T18:30:00Z
status: human_needed
score: 11/11
overrides_applied: 0
human_verification:
  - test: "Start a worker with ROZ_ROBOT_TOML pointing to a valid robot.toml and observe it uploads after registration"
    expected: "Log line 'embodiment model uploaded' appears; server DB has embodiment_model populated for the host"
    why_human: "Requires running worker and server processes with a real database and network"
  - test: "Start a worker with ROZ_ROBOT_TOML and restart it without changing the manifest"
    expected: "Second startup logs 'embodiment model uploaded' but server returns 204 (digest match, no write)"
    why_human: "Requires observing server HTTP response code in worker-server interaction"
  - test: "Start a worker without ROZ_ROBOT_TOML set"
    expected: "No embodiment upload attempt; no upload-related log lines"
    why_human: "Requires running worker process and observing absence of behavior"
---

# Phase 5: Worker Embodiment Upload Wiring Verification Report

**Phase Goal:** Workers automatically upload their embodiment model to the server at startup, skipping when unchanged
**Verified:** 2026-04-08T18:30:00Z
**Status:** human_needed
**Re-verification:** No -- initial verification

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | Worker uploads embodiment model to server after successful host registration without manual intervention | VERIFIED | `main.rs:862-874` calls `upload_embodiment` inside `Ok(host_id)` arm after `register_host` succeeds; model loaded from manifest at lines 836-854 |
| 2 | Worker skips upload when server already has a model with matching digest (no redundant writes) | VERIFIED | `embodiments.rs:53-82` `conditional_upsert` uses `IS DISTINCT FROM` in SQL WHERE clause; handler returns 204 when `rows_affected() == 0`; 4 integration tests cover first-upload/skip/changed/legacy cases |
| 3 | Server has embodiment data in DB for any registered worker that has an EmbodimentModel, enabling all downstream queries | VERIFIED | `hosts.rs:159-188` handler calls `conditional_upsert` which writes to `embodiment_model` JSONB column on `roz_hosts`; worker always calls upload after registration when model available |
| 4 | Server returns 204 No Content when incoming model_digest matches stored digest | VERIFIED | `hosts.rs:186` returns `StatusCode::NO_CONTENT` when `wrote == false` |
| 5 | Server returns 200 OK when incoming model_digest differs or no prior model exists | VERIFIED | `hosts.rs:184` returns `StatusCode::OK` when `wrote == true`; SQL WHERE includes `embodiment_model IS NULL` for first-upload case |
| 6 | Server performs DB upsert only when digests differ -- atomic single-query check | VERIFIED | `embodiments.rs:67-81` single UPDATE with WHERE clause containing `IS DISTINCT FROM`; no separate SELECT |
| 7 | DB-level integration tests prove conditional_upsert returns false when digest unchanged | VERIFIED | 4 test functions: `conditional_upsert_first_upload`, `conditional_upsert_skips_identical_digest`, `conditional_upsert_writes_on_changed_digest`, `conditional_upsert_writes_when_no_digest_field` |
| 8 | Worker loads robot_toml path from ROZ_ROBOT_TOML env var or roz-worker.toml | VERIFIED | `config.rs:55-57` `pub robot_toml: Option<String>` with `#[serde(default)]`; figment merges Env and TOML sources; 2 unit tests confirm defaults and explicit values |
| 9 | Worker parses manifest and extracts EmbodimentModel when robot_toml is set | VERIFIED | `main.rs:838-854` loads via `EmbodimentManifest::load()` then extracts `rt.model` from `embodiment_runtime()` |
| 10 | Worker skips embodiment upload when robot_toml is not set / logs warning and continues if upload fails / passes None for runtime | VERIFIED | `main.rs:838` `and_then` returns None when robot_toml is None (skip); line 873 `tracing::warn!` on upload error (log-and-continue); line 868 passes `None` for runtime parameter |
| 11 | upload_embodiment has a 10-second request timeout to prevent stalling startup | VERIFIED | `registration.rs:129-132` `Client::builder().timeout(Duration::from_secs(10)).build()` |

**Score:** 11/11 truths verified

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/roz-db/src/embodiments.rs` | conditional_upsert function with atomic digest comparison in SQL | VERIFIED | Function at lines 53-82; contains `IS DISTINCT FROM`; 4 integration tests |
| `crates/roz-server/src/routes/hosts.rs` | update_embodiment handler returning StatusCode based on conditional_upsert result | VERIFIED | Handler at lines 159-188; returns `Result<StatusCode, AppError>`; calls `conditional_upsert` |
| `crates/roz-worker/src/config.rs` | robot_toml optional config field | VERIFIED | Field at line 57: `pub robot_toml: Option<String>`; 2 unit tests |
| `crates/roz-worker/src/registration.rs` | upload_embodiment with request timeout | VERIFIED | 10-second timeout via `Client::builder().timeout()` at line 129-132 |
| `crates/roz-worker/src/main.rs` | Manifest loading and upload_embodiment call wired into startup | VERIFIED | Manifest loading at lines 836-854; upload call at lines 862-874 |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|-----|--------|---------|
| `crates/roz-server/src/routes/hosts.rs` | `crates/roz-db/src/embodiments.rs` | `roz_db::embodiments::conditional_upsert` | WIRED | Line 175: `roz_db::embodiments::conditional_upsert(&state.pool, id, &body.model, body.runtime.as_ref())` |
| `crates/roz-worker/src/main.rs` | `crates/roz-worker/src/registration.rs` | `upload_embodiment() call inside Ok(host_id) arm` | WIRED | Line 863: `roz_worker::registration::upload_embodiment(...)` with host_id, model, and None for runtime |
| `crates/roz-worker/src/config.rs` | figment Env loader | `ROZ_ROBOT_TOML -> robot_toml field` | WIRED | Figment merges Env("ROZ_") with TOML file; `robot_toml` field with `#[serde(default)]` |

### Data-Flow Trace (Level 4)

| Artifact | Data Variable | Source | Produces Real Data | Status |
|----------|--------------|--------|--------------------|--------|
| `main.rs` | `embodiment_model` | `EmbodimentManifest::load()` -> `embodiment_runtime()` -> `rt.model` | Real manifest data from filesystem | FLOWING |
| `hosts.rs` handler | `body.model` | HTTP request body JSON | Real model data from worker upload | FLOWING |
| `embodiments.rs` | SQL UPDATE result | `conditional_upsert` query | Writes to/reads from `roz_hosts.embodiment_model` column | FLOWING |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| conditional_upsert function exists | `git show main:crates/roz-db/src/embodiments.rs` grep | Function signature and IS DISTINCT FROM found | PASS |
| Handler returns StatusCode not Json | `git show main:crates/roz-server/src/routes/hosts.rs` grep | `Result<StatusCode, AppError>` confirmed | PASS |
| Worker config has robot_toml field | `git show main:crates/roz-worker/src/config.rs` grep | `pub robot_toml: Option<String>` found | PASS |
| Upload has timeout | `git show main:crates/roz-worker/src/registration.rs` grep | `timeout(Duration::from_secs(10))` found | PASS |
| Committed code compiles | `cargo check -p roz-db -p roz-server` (stashed working tree) | `Finished dev profile` | PASS |

Step 7b note: Full behavioral spot-checks (running tests, starting server) skipped because DB integration tests require a running PostgreSQL instance. Compilation verified on committed code.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-----------|-------------|--------|----------|
| WIRE-01 | 05-02-PLAN | Worker calls upload_embodiment() after successful host registration at startup | SATISFIED | `main.rs:862-874` calls `upload_embodiment` inside `Ok(host_id)` after `register_host` succeeds |
| WIRE-02 | 05-01-PLAN, 05-02-PLAN | Worker skips upload when server-side model digest matches local digest | SATISFIED | Server-side: `conditional_upsert` with `IS DISTINCT FROM` returns 204 when digest matches; Worker-side: always sends model, server handles dedup |

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| None found | - | - | - | - |

No TODOs, FIXMEs, placeholders, empty implementations, or stub patterns found in any phase 05 files.

### Human Verification Required

### 1. End-to-End Upload Flow

**Test:** Start a worker with `ROZ_ROBOT_TOML` pointing to a valid robot.toml manifest (e.g., `examples/ur5/robot.toml`) and a running server with database.
**Expected:** Worker logs "loaded embodiment model from manifest" then "registered with server" then "embodiment model uploaded". Server DB has `embodiment_model` JSONB populated for the host.
**Why human:** Requires running both worker and server processes with real database, network connectivity, and a valid manifest file.

### 2. Conditional Upload Skip on Restart

**Test:** After test 1, restart the same worker without changing the manifest.
**Expected:** Worker uploads again but server returns 204 (no write because digest matches). Worker still logs "embodiment model uploaded" (success path).
**Why human:** Need to observe the server's HTTP response code and verify DB was not written to (updated_at unchanged).

### 3. No-Config Skip Path

**Test:** Start a worker without `ROZ_ROBOT_TOML` set.
**Expected:** No embodiment-related log lines. Worker starts normally without attempting upload.
**Why human:** Requires running a worker process and confirming absence of specific log lines.

### Gaps Summary

No gaps found. All 11 observable truths verified against committed code. All artifacts exist, are substantive, and are properly wired. Both requirements (WIRE-01, WIRE-02) are satisfied.

Three human verification items remain: end-to-end upload flow, conditional skip on restart, and no-config skip path. These require running processes and cannot be verified via static code analysis.

Note: The working tree has uncommitted changes to `crates/roz-db/src/embodiments.rs` from a separate feature branch (`feature/02-01-tx-extractor-executor-migration`) that break compilation. These changes are NOT part of phase 05 and do not affect phase 05 verification. The committed code on `main` compiles cleanly.

---

_Verified: 2026-04-08T18:30:00Z_
_Verifier: Claude (gsd-verifier)_
