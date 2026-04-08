# Phase 6: Extension RPCs - Context

**Gathered:** 2026-04-08
**Status:** Ready for planning

<domain>
## Phase Boundary

Add `GetRetargetingMap` and `GetManifest` as standalone unary RPCs to `EmbodimentService`. Clients can fetch retargeting maps and control interface manifests as lightweight queries without fetching the full model. GetRetargetingMap includes coverage metadata (mapped/total counts).

</domain>

<decisions>
## Implementation Decisions

### Proto Additions
- **D-01:** Add `RetargetingMap` proto message to `embodiment.proto` mirroring the Rust `RetargetingMap` struct from `crates/roz-core/src/embodiment/retargeting.rs`. Include `embodiment_family`, `canonical_to_local` map, `local_to_canonical` map.
- **D-02:** Add `GetRetargetingMapRequest`/`GetRetargetingMapResponse` and `GetManifestRequest`/`GetManifestResponse` wrapper messages. Response wrappers allow including metadata (coverage counts) without polluting the core type.
- **D-03:** Add `GetRetargetingMap` and `GetManifest` RPCs to the existing `EmbodimentService` service definition. No new service — these are natural extensions of the same service.

### Coverage Metadata (EXT-03)
- **D-04:** `GetRetargetingMapResponse` includes `uint32 mapped_count` and `uint32 total_binding_count` fields alongside the `RetargetingMap`. This lets clients compute coverage percentage without a second query.

### Data Extraction Strategy
- **D-05:** RetargetingMap is computed on-the-fly from the stored `embodiment_runtime` JSONB. Per Out of Scope decision: "RetargetingMap persistence in DB — Pure function of bindings + family; no cache invalidation needed." The server deserializes the runtime, extracts bindings, and calls the existing `RetargetingMap::from_bindings()` function.
- **D-06:** ControlInterfaceManifest is extracted from the stored `embodiment_runtime` JSONB. The `ControlInterfaceManifest` proto message already exists — just needs conversion and a new RPC handler.

### Conversion Layer
- **D-07:** Add `RetargetingMap` conversions (domain ↔ proto) to `embodiment_convert.rs` following the existing pattern. Roundtrip proptest for the new type.
- **D-08:** `ControlInterfaceManifest` conversions already exist (Phase 3). No new conversion code needed for GetManifest — just wire the handler.

### Error Handling
- **D-09:** Follow existing pattern in `embodiment.rs`: return `NOT_FOUND` when host has no embodiment data, `INVALID_ARGUMENT` for bad host_id format, `INTERNAL` for deserialization failures. Map serde errors to `INTERNAL` with sanitized messages (per Phase 4 WR-01/WR-02 fix pattern).

### Claude's Discretion
- Request/response field naming and numbering within proto messages
- Test fixture data for proptest roundtrips
- Whether to add a DB helper or reuse existing `get_by_host_id`

</decisions>

<canonical_refs>
## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Embodiment Types
- `crates/roz-core/src/embodiment/retargeting.rs` — `RetargetingMap` struct definition, `from_bindings()` constructor
- `crates/roz-core/src/embodiment/binding.rs` — `ControlInterfaceManifest`, `ChannelBinding`, `ControlChannelDef` structs
- `crates/roz-core/src/embodiment/model.rs` — `EmbodimentModel`, `EmbodimentFamily`, `SemanticRole`

### Proto & Conversion
- `proto/roz/v1/embodiment.proto` — existing service definition, `ControlInterfaceManifest` message already present, `RetargetingMap` message needs adding
- `crates/roz-server/src/grpc/embodiment_convert.rs` — conversion layer pattern (domain ↔ proto), existing roundtrip proptests
- `crates/roz-server/src/grpc/embodiment.rs` — `EmbodimentServiceImpl` with auth pattern, `parse_host_id`, `load_embodiment_row`

### DB Layer
- `crates/roz-db/src/embodiments.rs` — `get_by_host_id`, `EmbodimentRow` with `embodiment_model` and `embodiment_runtime` JSONB columns

### Out of Scope Decisions
- `.planning/REQUIREMENTS.md` — "RetargetingMap persistence in DB — Pure function of bindings + family; no cache invalidation needed"

</canonical_refs>

<code_context>
## Existing Code Insights

### Reusable Assets
- `EmbodimentServiceImpl` in `crates/roz-server/src/grpc/embodiment.rs` — existing service impl with auth, tenant isolation, host lookup pattern
- `embodiment_convert.rs` — 58 impl blocks with `From<Domain> for Proto` and `From<Proto> for Domain` pattern + proptest roundtrips
- `RetargetingMap::from_bindings()` in `crates/roz-core/src/embodiment/retargeting.rs` — computes the retargeting map from bindings
- `ControlInterfaceManifest` conversion already exists — proto message and From impls from Phase 3

### Established Patterns
- gRPC handlers: `authenticated_tenant_id()` → `parse_host_id()` → `load_embodiment_row()` → deserialize JSONB → convert to proto → return Response
- Error mapping: serde errors → `Status::internal()` with sanitized message (Phase 4 pattern)
- Proto service extension: add RPC to `service EmbodimentService {}`, add request/response messages, implement trait method

### Integration Points
- `proto/roz/v1/embodiment.proto` — add new messages and RPCs
- `crates/roz-server/src/grpc/embodiment.rs` — implement new RPC handlers
- `crates/roz-server/src/grpc/embodiment_convert.rs` — add RetargetingMap conversions
- `crates/roz-server/build.rs` — no changes needed (already compiles all of embodiment.proto)

</code_context>

<specifics>
## Specific Ideas

No specific requirements — open to standard approaches following the established Phase 4 patterns.

</specifics>

<deferred>
## Deferred Ideas

None — discussion stayed within phase scope

</deferred>

---

*Phase: 06-extension-rpcs*
*Context gathered: 2026-04-08*
