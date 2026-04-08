# Phase 6: Extension RPCs - Research

**Researched:** 2026-04-08
**Domain:** gRPC service extension, proto message design, Rust domain-to-proto conversion
**Confidence:** HIGH

## Summary

Phase 6 adds two unary RPCs (`GetRetargetingMap` and `GetManifest`) to the existing `EmbodimentService` in `proto/roz/v1/embodiment.proto`. The phase is architecturally straightforward -- it follows the exact pattern established in Phase 4 for `GetModel`, `GetRuntime`, `ListBindings`, and `ValidateBindings`. The main deliverables are: (1) new proto messages for `RetargetingMap` and the request/response wrappers, (2) `RetargetingMap` domain-to-proto conversions with proptest roundtrip, (3) two new RPC handler methods on `EmbodimentServiceImpl`.

One critical finding: CONTEXT.md D-05/D-06 assume data is extracted from `embodiment_runtime` JSONB, but Phase 5 uploads model only (runtime is `None`). Both RPCs must extract from `embodiment_model` JSONB. Additionally, `ControlInterfaceManifest` is NOT stored in either JSONB column -- it's constructed at the worker from `robot.toml` and never uploaded. The `GetManifest` handler must synthesize a `ControlInterfaceManifest` from the model's `channel_bindings`, which is feasible but involves a lossy `BindingType` -> `CommandInterfaceType` mapping.

**Primary recommendation:** Follow the Phase 4 handler pattern exactly. Use `embodiment_model` JSONB as the data source for both RPCs. Add a `ControlInterfaceManifest::from_channel_bindings()` constructor to handle the synthesis for `GetManifest`.

<user_constraints>

## User Constraints (from CONTEXT.md)

### Locked Decisions
- **D-01:** Add `RetargetingMap` proto message to `embodiment.proto` mirroring the Rust `RetargetingMap` struct from `crates/roz-core/src/embodiment/retargeting.rs`. Include `embodiment_family`, `canonical_to_local` map, `local_to_canonical` map.
- **D-02:** Add `GetRetargetingMapRequest`/`GetRetargetingMapResponse` and `GetManifestRequest`/`GetManifestResponse` wrapper messages. Response wrappers allow including metadata (coverage counts) without polluting the core type.
- **D-03:** Add `GetRetargetingMap` and `GetManifest` RPCs to the existing `EmbodimentService` service definition. No new service -- these are natural extensions of the same service.
- **D-04:** `GetRetargetingMapResponse` includes `uint32 mapped_count` and `uint32 total_binding_count` fields alongside the `RetargetingMap`. This lets clients compute coverage percentage without a second query.
- **D-05:** RetargetingMap is computed on-the-fly from the stored JSONB. Per Out of Scope decision: "RetargetingMap persistence in DB -- Pure function of bindings + family; no cache invalidation needed." The server deserializes the model, extracts bindings, and calls the existing `RetargetingMap::from_bindings()` function.
- **D-06:** ControlInterfaceManifest is extracted from the stored JSONB. The `ControlInterfaceManifest` proto message already exists -- just needs conversion and a new RPC handler.
- **D-07:** Add `RetargetingMap` conversions (domain to/from proto) to `embodiment_convert.rs` following the existing pattern. Roundtrip proptest for the new type.
- **D-08:** `ControlInterfaceManifest` conversions already exist (Phase 3). No new conversion code needed for GetManifest -- just wire the handler.
- **D-09:** Follow existing pattern in `embodiment.rs`: return `NOT_FOUND` when host has no embodiment data, `INVALID_ARGUMENT` for bad host_id format, `INTERNAL` for deserialization failures. Map serde errors to `Status::internal()` with sanitized messages (per Phase 4 WR-01/WR-02 fix pattern).

### Claude's Discretion
- Request/response field naming and numbering within proto messages
- Test fixture data for proptest roundtrips
- Whether to add a DB helper or reuse existing `get_by_host_id`

### Deferred Ideas (OUT OF SCOPE)
None

</user_constraints>

<phase_requirements>

## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| EXT-01 | Client can fetch canonical-to-local joint mapping via GetRetargetingMap unary RPC | `RetargetingMap::from_bindings()` exists in roz-core; model has `channel_bindings` + `embodiment_family`; handler follows Phase 4 `get_model` pattern |
| EXT-02 | Client can fetch ControlInterfaceManifest via GetManifest unary RPC without fetching full model | Proto message and conversions already exist; need synthesis from `channel_bindings` since manifest not stored in DB |
| EXT-03 | GetRetargetingMap response includes mapped/total binding counts for coverage reporting | `mapped_count` = `retargeting_map.canonical_to_local.len()`; `total_binding_count` = `model.channel_bindings.len()` |

</phase_requirements>

## Project Constraints (from CLAUDE.md)

- Rust 2024 / toolchain 1.92.0
- `clippy::pedantic` and `clippy::nursery` at warn, `unsafe_code = "deny"`
- `max_width = 120` in rustfmt
- CI runs `cargo fmt --check` and `cargo clippy --workspace -- -D warnings`
- Tonic 0.13 + Prost 0.13 for gRPC/proto codegen
- `tonic_build` in `crates/roz-server/build.rs` with `.btree_map([".roz.v1"])`
- Proto files in `proto/roz/v1/` with `roz.v1` package
- Conversions in `crates/roz-server/src/grpc/embodiment_convert.rs`
- Service impl in `crates/roz-server/src/grpc/embodiment.rs`
- DB access via `crates/roz-db/src/embodiments.rs`

## Standard Stack

No new dependencies. This phase uses only what's already in the workspace.

### Core (already present)
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| tonic | 0.13 | gRPC server/client | Already used for all gRPC services |
| prost | 0.13 | Protobuf codegen | Paired with tonic |
| tonic-build | 0.13 | Proto compilation | In build.rs, compiles embodiment.proto |
| proptest | 1.6 | Property-based testing | Already used for all roundtrip tests |
| sqlx | 0.8 | DB access | Already used via roz-db |

[VERIFIED: crates/roz-server/Cargo.toml]

## Architecture Patterns

### Existing Handler Pattern (Phase 4)

Every embodiment RPC follows this exact flow, established in `crates/roz-server/src/grpc/embodiment.rs`:

```
authenticated_tenant_id(&request) -> parse_host_id() -> fetch_embodiment_row() -> deserialize JSONB -> domain logic -> convert to proto -> Response
```

[VERIFIED: crates/roz-server/src/grpc/embodiment.rs lines 96-213]

### Proto Extension Pattern

Add RPCs to the existing `service EmbodimentService {}` block. Add request/response messages in the same file. No new service, no new proto file, no build.rs changes.

[VERIFIED: crates/roz-server/build.rs already compiles embodiment.proto]

### Conversion Pattern

Domain-to-proto: `impl From<&DomainType> for roz_v1::ProtoType`
Proto-to-domain: `impl TryFrom<roz_v1::ProtoType> for DomainType` with `EmbodimentConvertError`
Roundtrip proptest: `proptest! { fn roundtrip_xxx(val in arb_xxx()) { ... } }`

Map fields use `BTreeMap<String, String>` in generated code because `build.rs` configures `.btree_map([".roz.v1"])`.

[VERIFIED: crates/roz-server/build.rs line 7, crates/roz-server/src/grpc/embodiment_convert.rs]

### Data Source

**Critical finding:** Phase 5 uploads `embodiment_model` JSONB but passes `None` for `embodiment_runtime`. Both new RPCs must read from `embodiment_model`, not `embodiment_runtime`.

[VERIFIED: crates/roz-worker/src/main.rs line 869 passes `None` for runtime; crates/roz-worker/src/registration.rs line 128]

### Recommended File Changes

```
proto/roz/v1/embodiment.proto          # Add RetargetingMap message, request/response wrappers, RPCs
crates/roz-server/src/grpc/embodiment_convert.rs  # Add RetargetingMap From impls + proptest
crates/roz-server/src/grpc/embodiment.rs           # Add get_retargeting_map + get_manifest handlers
```

No new files. No build.rs changes.

### Anti-Patterns to Avoid
- **Separate DB query per RPC:** Reuse `fetch_embodiment_row()` -- don't create separate DB helpers
- **Reading from `embodiment_runtime` column:** Use `embodiment_model` -- runtime is NULL after Phase 5 upload
- **Leaking serde error details:** Always map to `Status::internal("failed to deserialize embodiment data")` per Phase 4 WR-01/WR-02

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Retargeting map computation | Custom binding-to-map logic | `RetargetingMap::from_bindings()` | Already tested, handles semantic_role filtering |
| Proto code generation | Manual struct definitions | tonic-build + prost | build.rs already handles this |
| Auth + tenant isolation | Custom auth logic | `authenticated_tenant_id()` + `fetch_embodiment_row()` | Phase 4 helpers with tenant isolation |
| Error mapping | Custom error responses | Existing pattern: `Status::not_found`, `Status::invalid_argument`, `Status::internal` | Consistent with all other embodiment RPCs |

## Common Pitfalls

### Pitfall 1: ControlInterfaceManifest Not in DB
**What goes wrong:** CONTEXT.md D-06 says "extracted from embodiment_runtime JSONB" but `EmbodimentRuntime` does NOT contain a `ControlInterfaceManifest` field. The manifest is constructed from `robot.toml` at the worker and never persisted to the server DB.
**Why it happens:** During discuss phase, it was assumed the manifest lived inside the runtime JSONB.
**How to avoid:** Synthesize the manifest from `channel_bindings` stored in `embodiment_model`. Each `ChannelBinding` has `physical_name`, `binding_type`, `units`, `frame_id` -- sufficient to construct `ControlChannelDef` entries with a `BindingType` -> `CommandInterfaceType` mapping.
**Warning signs:** Runtime JSONB is NULL; even if it weren't, `EmbodimentRuntime` struct has no manifest field.

### Pitfall 2: embodiment_runtime is NULL
**What goes wrong:** Handler tries to read `embodiment_runtime` column and gets `None`.
**Why it happens:** Phase 5 worker upload passes `None` for runtime.
**How to avoid:** Read `embodiment_model` column instead. Both RPCs only need data from the model (bindings + family).
**Warning signs:** `FAILED_PRECONDITION: host has no embodiment runtime` errors in testing.

### Pitfall 3: BTreeMap Proto Field Ordering
**What goes wrong:** Proto `map<string, string>` fields generate as `BTreeMap` due to `.btree_map([".roz.v1"])` in build.rs. Using `HashMap` in conversion code causes type mismatch.
**Why it happens:** Default prost generates `HashMap` but build.rs overrides.
**How to avoid:** Use `BTreeMap` consistently. The domain `RetargetingMap` already uses `BTreeMap`, so this is naturally aligned.
**Warning signs:** Compilation errors on map field types.

### Pitfall 4: Missing EmbodimentFamily on Model
**What goes wrong:** `RetargetingMap::from_bindings()` requires an `EmbodimentFamily`, but `EmbodimentModel.embodiment_family` is `Option<EmbodimentFamily>`.
**Why it happens:** Not all robots have a family classification.
**How to avoid:** Return appropriate error when `embodiment_family` is `None` and `GetRetargetingMap` is called. Use `FAILED_PRECONDITION` status: "host has no embodiment family -- retargeting requires a family classification."
**Warning signs:** Unwrap on None family field.

### Pitfall 5: BindingType to CommandInterfaceType Mapping
**What goes wrong:** When synthesizing `ControlInterfaceManifest` from `channel_bindings`, `BindingType::Command` has no corresponding `CommandInterfaceType`, and three IMU binding types collapse to one `CommandInterfaceType::ImuSensor`.
**Why it happens:** These are different enums with different granularity.
**How to avoid:** Define a best-effort mapping function. For `Command`, use `JointPosition` as fallback or skip the channel. For IMU types, map all three to `ImuSensor`. Document the lossy mapping.
**Warning signs:** Match exhaustiveness errors, semantic mismatches.

## Code Examples

### Proto Additions (embodiment.proto)

```protobuf
// Source: follows existing message patterns in embodiment.proto
// Add to service EmbodimentService:
rpc GetRetargetingMap(GetRetargetingMapRequest) returns (GetRetargetingMapResponse);
rpc GetManifest(GetManifestRequest) returns (GetManifestResponse);

// Add request/response messages:
message RetargetingMap {
  EmbodimentFamily embodiment_family = 1;
  map<string, string> canonical_to_local = 2;
  map<string, string> local_to_canonical = 3;
}

message GetRetargetingMapRequest {
  string host_id = 1;
}

message GetRetargetingMapResponse {
  RetargetingMap retargeting_map = 1;
  uint32 mapped_count = 2;
  uint32 total_binding_count = 3;
}

message GetManifestRequest {
  string host_id = 1;
}

message GetManifestResponse {
  ControlInterfaceManifest manifest = 1;
}
```

[ASSUMED: field numbers are Claude's discretion per CONTEXT.md]

### RetargetingMap Conversion (embodiment_convert.rs)

```rust
// Source: follows existing conversion pattern in embodiment_convert.rs

// Domain -> Proto
impl From<&RetargetingMap> for roz_v1::RetargetingMap {
    fn from(rm: &RetargetingMap) -> Self {
        Self {
            embodiment_family: Some(roz_v1::EmbodimentFamily::from(&rm.embodiment_family)),
            canonical_to_local: rm.canonical_to_local.clone(),
            local_to_canonical: rm.local_to_canonical.clone(),
        }
    }
}

// Proto -> Domain
impl TryFrom<roz_v1::RetargetingMap> for RetargetingMap {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::RetargetingMap) -> Result<Self, Self::Error> {
        let embodiment_family = proto
            .embodiment_family
            .ok_or_else(|| EmbodimentConvertError::MissingField("embodiment_family".into()))?;
        Ok(Self {
            embodiment_family: EmbodimentFamily::from(embodiment_family),
            canonical_to_local: proto.canonical_to_local,
            local_to_canonical: proto.local_to_canonical,
        })
    }
}
```

[VERIFIED: conversion pattern matches existing impls in embodiment_convert.rs]

### RPC Handler (embodiment.rs)

```rust
// Source: follows get_model/get_runtime pattern in embodiment.rs

async fn get_retargeting_map(
    &self,
    request: Request<GetRetargetingMapRequest>,
) -> Result<Response<GetRetargetingMapResponse>, Status> {
    let tenant_id = self.authenticated_tenant_id(&request).await?;
    let host_id = parse_host_id(&request.get_ref().host_id)?;
    let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;

    let model_json = row
        .embodiment_model
        .ok_or_else(|| Status::failed_precondition("host has no embodiment model"))?;

    let domain_model: EmbodimentModel = serde_json::from_value(model_json).map_err(|e| {
        tracing::error!(error = %e, host_id = %host_id, "corrupt model data");
        Status::internal("failed to deserialize embodiment data")
    })?;

    let family = domain_model
        .embodiment_family
        .ok_or_else(|| Status::failed_precondition("host has no embodiment family"))?;

    let retargeting_map = RetargetingMap::from_bindings(family, &domain_model.channel_bindings);
    let mapped_count = u32::try_from(retargeting_map.canonical_to_local.len()).unwrap_or(u32::MAX);
    let total_binding_count = u32::try_from(domain_model.channel_bindings.len()).unwrap_or(u32::MAX);

    Ok(Response::new(GetRetargetingMapResponse {
        retargeting_map: Some(roz_v1::RetargetingMap::from(&retargeting_map)),
        mapped_count,
        total_binding_count,
    }))
}
```

[VERIFIED: pattern matches existing handlers in embodiment.rs]

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Fetch full model for retargeting data | Dedicated GetRetargetingMap RPC | Phase 6 | Clients avoid deserializing full model for lightweight query |
| No manifest query | GetManifest RPC | Phase 6 | substrate-ide can fetch control interface without full model |

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | `ControlInterfaceManifest` can be synthesized from `channel_bindings` via a `BindingType -> CommandInterfaceType` mapping | Pitfalls, Code Examples | If the mapping is considered too lossy, GetManifest may need to wait for worker to upload the manifest separately |
| A2 | `embodiment_model` JSONB is always populated when `GetRetargetingMap` or `GetManifest` is called | Architecture Patterns | If model is NULL, handler returns `FAILED_PRECONDITION` which is correct behavior |
| A3 | Proto field numbers in the new messages don't conflict with any reserved ranges | Code Examples | Low risk -- new messages have their own field numbering space |

## Open Questions (RESOLVED)

1. **GetManifest Data Source**
   - What we know: `ControlInterfaceManifest` is NOT stored in the DB. It's constructed from `robot.toml` at the worker and never uploaded.
   - What's unclear: Is a synthesized manifest from `channel_bindings` acceptable, or should the worker upload the manifest?
   - RESOLVED: Synthesize from `channel_bindings`. The mapping is imperfect (`BindingType::Command` -> `CommandInterfaceType::JointPosition` is lossy), but it provides useful data. If higher fidelity is needed, that can be addressed in a future phase by adding manifest to the upload payload. Plan 06-02 Task 2 implements this.

2. **EmbodimentFamily Required for Retargeting**
   - What we know: `EmbodimentModel.embodiment_family` is `Option<EmbodimentFamily>`. Some robots may not have a family set.
   - What's unclear: Should `GetRetargetingMap` return an error or an empty map when no family is set?
   - RESOLVED: Return `FAILED_PRECONDITION` error -- retargeting inherently requires a family classification to be meaningful. Empty maps are misleading. Plan 06-02 Task 1 implements this.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | cargo test (built-in Rust test harness) + proptest 1.6 |
| Config file | none (standard Cargo test) |
| Quick run command | `cargo test -p roz-server --lib embodiment_convert -- --test-threads=1` |
| Full suite command | `cargo test -p roz-server` |

### Phase Requirements -> Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| EXT-01 | GetRetargetingMap returns canonical-to-local mapping | unit | `cargo test -p roz-server --lib embodiment -- get_retargeting_map` | No -- Wave 0 |
| EXT-02 | GetManifest returns ControlInterfaceManifest | unit | `cargo test -p roz-server --lib embodiment -- get_manifest` | No -- Wave 0 |
| EXT-03 | Response includes mapped_count and total_binding_count | unit | `cargo test -p roz-server --lib embodiment -- coverage_counts` | No -- Wave 0 |
| CONV | RetargetingMap roundtrip conversion | proptest | `cargo test -p roz-server --lib embodiment_convert -- roundtrip_retargeting_map` | No -- Wave 0 |

### Sampling Rate
- **Per task commit:** `cargo test -p roz-server --lib`
- **Per wave merge:** `cargo test -p roz-server`
- **Phase gate:** Full suite green (`cargo test --workspace`) before verify

### Wave 0 Gaps
- [ ] Proptest `arb_retargeting_map()` strategy in `embodiment_convert.rs`
- [ ] `roundtrip_retargeting_map` proptest assertion in `embodiment_convert.rs`
- [ ] Unit tests for handler logic are inline -- no separate test file needed

## Security Domain

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | yes | `authenticated_tenant_id()` -- existing pattern, reused |
| V3 Session Management | no | Unary RPCs, no session state |
| V4 Access Control | yes | `fetch_embodiment_row()` enforces tenant isolation -- returns NOT_FOUND for cross-tenant |
| V5 Input Validation | yes | `parse_host_id()` validates UUID format |
| V6 Cryptography | no | No crypto operations |

### Known Threat Patterns

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Cross-tenant data access | Information Disclosure | `fetch_embodiment_row()` checks `tenant_id` match, returns NOT_FOUND (not FORBIDDEN) |
| Serde error information leakage | Information Disclosure | Sanitized error messages: `Status::internal("failed to deserialize embodiment data")` |
| Invalid host_id injection | Tampering | UUID parse validation via `parse_host_id()` |

All mitigations already exist and are reused from Phase 4 without modification. [VERIFIED: embodiment.rs lines 60-88]

## Sources

### Primary (HIGH confidence)
- `crates/roz-core/src/embodiment/retargeting.rs` -- RetargetingMap struct, from_bindings() logic
- `crates/roz-core/src/embodiment/binding.rs` -- ControlInterfaceManifest, ChannelBinding, BindingType structs
- `crates/roz-core/src/embodiment/embodiment_runtime.rs` -- EmbodimentRuntime struct fields (confirms no manifest field)
- `proto/roz/v1/embodiment.proto` -- existing service, message definitions, ControlInterfaceManifest proto
- `crates/roz-server/src/grpc/embodiment.rs` -- existing EmbodimentService impl, handler pattern
- `crates/roz-server/src/grpc/embodiment_convert.rs` -- conversion pattern, proptest roundtrip pattern
- `crates/roz-server/build.rs` -- tonic-build config with btree_map
- `crates/roz-db/src/embodiments.rs` -- get_by_host_id, EmbodimentRow schema
- `crates/roz-worker/src/registration.rs` -- upload_embodiment passes None for runtime

### Secondary (MEDIUM confidence)
- `crates/roz-core/src/manifest.rs` -- control_interface_manifest() construction from robot.toml (confirms manifest source)

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH -- no new dependencies, all patterns verified in codebase
- Architecture: HIGH -- exact handler pattern exists from Phase 4, verified line by line
- Pitfalls: HIGH -- data source mismatch verified by reading actual upload code and struct definitions

**Research date:** 2026-04-08
**Valid until:** 2026-05-08 (stable -- no external dependency changes)
