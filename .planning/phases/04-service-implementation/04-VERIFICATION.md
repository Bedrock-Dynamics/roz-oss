---
phase: 04-service-implementation
verified: 2026-04-08T15:45:00Z
status: human_needed
score: 4/4
overrides_applied: 0
human_verification:
  - test: "Send a GetModel gRPC request to a running roz-server with embodiment data loaded"
    expected: "Returns a complete EmbodimentModel proto response with all fields populated"
    why_human: "Requires a running server with Postgres, seeded host and embodiment JSONB data"
  - test: "Send a GetRuntime gRPC request for a host with both model and runtime data"
    expected: "Returns EmbodimentRuntime with model, calibration, safety overlay, and digests"
    why_human: "Requires running server with realistic embodiment runtime JSON in database"
  - test: "Call ValidateBindings for a host with known unbound channels"
    expected: "Returns valid=false with correct unbound_channels list"
    why_human: "Requires carefully crafted test data with intentional binding mismatches"
  - test: "PUT /v1/hosts/{id}/embodiment with valid bearer auth, then GET via gRPC"
    expected: "Full round-trip: REST upload persists, gRPC serves the same data"
    why_human: "Integration test requiring running server, auth, and both REST and gRPC clients"
---

# Phase 4: Service Implementation Verification Report

**Phase Goal:** A working EmbodimentService serves GetModel, GetRuntime, ListBindings, and ValidateBindings RPCs; substrate-ide can fetch embodiment data over gRPC
**Verified:** 2026-04-08T15:45:00Z
**Status:** human_needed
**Re-verification:** No -- initial verification

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | `GetModel` RPC returns a complete EmbodimentModel for a given host identifier | VERIFIED | `crates/roz-server/src/grpc/embodiment.rs:96-112` implements `get_model` -- authenticates, fetches from DB, deserializes to domain `EmbodimentModel`, converts via `ProtoModel::from(&domain_model)`, returns `Response::new()` |
| 2 | `GetRuntime` RPC returns the compiled EmbodimentRuntime (model + calibration + safety overlays + digests) | VERIFIED | `crates/roz-server/src/grpc/embodiment.rs:115-132` implements `get_runtime` -- same pattern as GetModel but deserializes `embodiment_runtime` JSONB column to `EmbodimentRuntime` and converts via `ProtoRuntime::from()` |
| 3 | `ListBindings` and `ValidateBindings` RPCs return correct results | VERIFIED | `list_bindings` (lines 134-159) extracts `channel_bindings` from model and converts each via `ChannelBinding::from()`. `validate_bindings` (lines 162-212) deserializes runtime, extracts joint_names/sensor_ids/frame_ids/channel_count, calls `roz_core::embodiment::binding::validate_bindings()`, converts `UnboundChannel` to proto |
| 4 | Service is registered in server startup and appears in gRPC reflection | VERIFIED | `crates/roz-server/src/main.rs:128-143` creates `EmbodimentServiceImpl::new()` and adds `EmbodimentServiceServer::new()` to tonic Routes chain. `crates/roz-server/build.rs:15` includes `embodiment.proto` in compile_protos, so `FILE_DESCRIPTOR_SET` covers it. Reflection service at lines 145-149 uses this descriptor set. |

**Score:** 4/4 truths verified

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `migrations/022_host_embodiment.sql` | JSONB columns on roz_hosts | VERIFIED | 6 lines. `ALTER TABLE roz_hosts ADD COLUMN embodiment_model JSONB, ADD COLUMN embodiment_runtime JSONB` |
| `crates/roz-db/src/embodiments.rs` | CRUD functions for embodiment JSONB data | VERIFIED | 136 lines. Exports `EmbodimentRow`, `get_by_host_id`, `upsert`. 3 integration tests with real Postgres. |
| `crates/roz-db/src/lib.rs` | `pub mod embodiments` declaration | VERIFIED | Line 6: `pub mod embodiments;` in alphabetical order |
| `crates/roz-server/src/grpc/embodiment.rs` | EmbodimentServiceImpl with 4 RPCs | VERIFIED | 213 lines. `pub struct EmbodimentServiceImpl`, `impl EmbodimentService for EmbodimentServiceImpl` with `get_model`, `get_runtime`, `list_bindings`, `validate_bindings`. Auth via `GrpcAuth`, tenant isolation via `fetch_embodiment_row` helper. |
| `crates/roz-server/src/grpc/mod.rs` | `pub mod embodiment` declaration | VERIFIED | Line 3: `pub mod embodiment;` |
| `crates/roz-server/src/main.rs` | EmbodimentServiceServer registration | VERIFIED | Lines 128-143: `EmbodimentServiceImpl::new()` and `.add_service(EmbodimentServiceServer::new(...))` |
| `crates/roz-server/src/routes/hosts.rs` | PUT /v1/hosts/:id/embodiment endpoint | VERIFIED | Lines 153-175: `UpdateEmbodimentRequest` struct, `update_embodiment` handler with tenant isolation, calls `roz_db::embodiments::upsert` |
| `crates/roz-server/src/lib.rs` | Route registration for embodiment endpoint | VERIFIED | Line 54: `.route("/v1/hosts/{id}/embodiment", put(routes::hosts::update_embodiment))` |
| `crates/roz-worker/src/registration.rs` | upload_embodiment function | VERIFIED | Lines 122-149: `pub async fn upload_embodiment` with `PUT /v1/hosts/{host_id}/embodiment`, bearer auth, JSON body with model + optional runtime. 2 unit tests. |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|-----|--------|---------|
| `embodiment.rs` (gRPC) | `roz_db::embodiments` (DB) | `get_by_host_id` call | WIRED | Line 73: `roz_db::embodiments::get_by_host_id(pool, host_id)` in shared `fetch_embodiment_row` helper used by all 4 RPCs |
| `embodiment.rs` (gRPC) | `embodiment_convert.rs` | `From`/`into()` conversions | WIRED | Lines 112, 131: `ProtoModel::from(&domain_model)`, `ProtoRuntime::from(&domain_runtime)`. Line 156: `ChannelBinding::from()`. Line 204: `domain_binding_type_to_proto()`. All resolve to impls in embodiment_convert.rs |
| `main.rs` | `embodiment.rs` | `EmbodimentServiceImpl::new()` | WIRED | Lines 128-131: creates instance with `pool.clone()` and `Arc<dyn GrpcAuth>` |
| `main.rs` | tonic Routes | `EmbodimentServiceServer::new()` | WIRED | Lines 141-143: `.add_service(EmbodimentServiceServer::new(embodiment_svc))` |
| `hosts.rs` (REST) | `roz_db::embodiments` (DB) | `upsert()` call | WIRED | Line 173: `roz_db::embodiments::upsert(&state.pool, id, &body.model, body.runtime.as_ref())` |
| `registration.rs` (worker) | REST endpoint | `PUT /v1/hosts/{id}/embodiment` | WIRED | Line 140: `.put(format!("{base}/v1/hosts/{host_id}/embodiment"))` with bearer auth and JSON body. Not yet called from main.rs (documented scope limitation). |

### Data-Flow Trace (Level 4)

| Artifact | Data Variable | Source | Produces Real Data | Status |
|----------|---------------|--------|--------------------|--------|
| `embodiment.rs::get_model` | `model_json` | `roz_db::embodiments::get_by_host_id` -> `row.embodiment_model` | DB query against JSONB column, deserialized via `serde_json::from_value` | FLOWING |
| `embodiment.rs::get_runtime` | `runtime_json` | `roz_db::embodiments::get_by_host_id` -> `row.embodiment_runtime` | DB query against JSONB column, deserialized via `serde_json::from_value` | FLOWING |
| `embodiment.rs::list_bindings` | `domain_model.channel_bindings` | DB -> deserialize -> `model.channel_bindings.iter()` | Real data from DB through typed deserialization | FLOWING |
| `embodiment.rs::validate_bindings` | `unbound` | DB -> deserialize -> `roz_core::embodiment::binding::validate_bindings()` | Real validation against domain data | FLOWING |
| `hosts.rs::update_embodiment` | `body.model` / `body.runtime` | HTTP request body | Caller-provided JSON persisted to DB | FLOWING |

### Behavioral Spot-Checks

Step 7b: SKIPPED (requires running server with Postgres -- no runnable entry points without external services)

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|------------|-------------|--------|----------|
| SERV-01 | 04-01, 04-02 | EmbodimentService trait impl in roz-server following existing agent.rs pattern | SATISFIED | `impl EmbodimentService for EmbodimentServiceImpl` in embodiment.rs. Uses `GrpcAuth` trait, `pool + auth` constructor, `authenticated_tenant_id` helper -- same pattern as `TaskServiceImpl`. |
| SERV-02 | 04-01, 04-02 | Service registered in main.rs server startup | SATISFIED | `EmbodimentServiceImpl::new()` and `.add_service(EmbodimentServiceServer::new(...))` in main.rs grpc_router() |
| SERV-03 | 04-01, 04-02 | gRPC reflection support for embodiment service | SATISFIED | `embodiment.proto` in build.rs compile_protos list. `FILE_DESCRIPTOR_SET` used by reflection service. Test `file_descriptor_set_is_not_empty` and `embodiment_service_trait_is_generated` confirm codegen. |

No orphaned requirements -- REQUIREMENTS.md maps exactly SERV-01, SERV-02, SERV-03 to Phase 4, and all are claimed and satisfied.

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| (none) | - | - | - | No TODOs, FIXMEs, placeholders, empty returns, or stub patterns found in any phase artifact |

### Human Verification Required

### 1. GetModel end-to-end with running server

**Test:** Start roz-server with Postgres, create a host, PUT embodiment data via REST, call GetModel via grpcurl
**Expected:** Returns complete EmbodimentModel proto with all fields matching the uploaded JSON
**Why human:** Requires running server infrastructure (Postgres, auth) and gRPC client tooling

### 2. GetRuntime with full runtime data

**Test:** Upload both model and runtime JSONB, call GetRuntime
**Expected:** Returns EmbodimentRuntime with model, calibration overlay, safety overlay, and all digest fields
**Why human:** Requires realistic runtime data in database and running server

### 3. ValidateBindings with intentional binding mismatches

**Test:** Upload model with channel bindings that reference non-existent joints/sensors, call ValidateBindings
**Expected:** Returns `valid=false` with correct unbound_channels listing the mismatched bindings
**Why human:** Requires crafted test data with specific binding mismatches

### 4. Full pipeline: REST upload then gRPC read

**Test:** PUT /v1/hosts/{id}/embodiment with bearer auth, then immediately call GetModel via gRPC
**Expected:** gRPC response contains exactly the data that was uploaded via REST
**Why human:** Integration test spanning REST and gRPC boundaries with auth

### Gaps Summary

No automated verification gaps found. All 4 roadmap success criteria are met at the code level: the gRPC service implements all 4 RPCs with proper authentication, tenant isolation, DB-backed data access, domain-to-proto conversion, and server registration with reflection. The REST upload endpoint and worker push function complete the data pipeline.

The `upload_embodiment` function in `registration.rs` is not yet called from worker `main.rs`. This is explicitly documented in the plan as intentional -- the worker does not yet construct `EmbodimentModel` at startup. The function is provided and ready for future wiring.

4 human verification items remain for end-to-end runtime testing that cannot be verified statically.

---

_Verified: 2026-04-08T15:45:00Z_
_Verifier: Claude (gsd-verifier)_
