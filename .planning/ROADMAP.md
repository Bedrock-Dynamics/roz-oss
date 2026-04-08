# Roadmap: Roz Embodiment Protos

## Overview

Add a gRPC EmbodimentService to roz-server that exposes the full Rust embodiment type graph over protobuf. The work follows a hard compile-time dependency chain: proto definition and codegen must succeed before conversion code can reference generated types, and conversions must exist before the service implementation can use them. Four phases deliver this incrementally: proto definition, leaf-type conversions, composite-type conversions with round-trip tests, and service wiring.

## Phases

**Phase Numbering:**
- Integer phases (1, 2, 3): Planned milestone work
- Decimal phases (2.1, 2.2): Urgent insertions (marked with INSERTED)

Decimal phases appear between their surrounding integers in numeric order.

- [ ] **Phase 1: Proto Definition and Build Integration** - Define all embodiment messages, enums, and service RPCs in embodiment.proto; wire into build pipeline
- [ ] **Phase 2: Leaf Type Conversions** - Bidirectional From/TryFrom impls for primitive and leaf types (transforms, enums, geometry, scalars)
- [ ] **Phase 3: Composite Type Conversions** - Bidirectional conversions for aggregate types (Joint, Link, Model, Runtime) plus round-trip property tests
- [ ] **Phase 4: Service Implementation** - EmbodimentService trait impl with all RPCs, server registration, and reflection

## Phase Details

### Phase 1: Proto Definition and Build Integration
**Goal**: A compilable embodiment.proto exists with all message types, enums, oneof structures, and service definition; cargo build succeeds with codegen
**Depends on**: Nothing (first phase)
**Requirements**: PROTO-01, PROTO-02, PROTO-03, PROTO-04, PROTO-05, PROTO-06, PROTO-07, PROTO-08, PROTO-09, PROTO-10, PROTO-11, PROTO-12, PROTO-13, PROTO-14, PROTO-15, PROTO-16, PROTO-17, PROTO-18, PROTO-19, GRPC-01, GRPC-02, GRPC-03, GRPC-04, SERV-04
**Success Criteria** (what must be TRUE):
  1. `proto/roz/v1/embodiment.proto` exists with all ~25 message types matching the Rust embodiment type graph
  2. `cargo build -p roz-server` succeeds with embodiment proto codegen (build.rs updated)
  3. Generated Rust types are accessible in roz-server code (e.g., `roz::v1::EmbodimentModel` resolves)
  4. Proto uses `optional` keyword on all fields corresponding to Rust `Option<T>` scalars
  5. Quaternion fields use XYZW convention; enums with associated data use oneof with per-variant messages
**Plans:** 2 plans

Plans:
- [x] 01-01-PLAN.md — Write embodiment.proto with all ~48 message/enum/service definitions
- [x] 01-02-PLAN.md — Wire into build.rs, add btree_map config, verify generated types

### Phase 2: Leaf Type Conversions
**Goal**: All primitive and leaf-level embodiment types convert bidirectionally between roz-core domain types and generated proto types
**Depends on**: Phase 1
**Requirements**: CONV-01, CONV-02, CONV-03, CONV-06
**Success Criteria** (what must be TRUE):
  1. Transform3D, Vec3, Quaternion convert with correct WXYZ-to-XYZW index swapping
  2. All enum types (JointType, GeometryShape variants, SensorType, SemanticRole, etc.) convert bidirectionally
  3. `Option<f64>` fields round-trip correctly -- `Some(0.0)` does not collapse to `None`
  4. `EmbodimentConvertError` type exists with variants covering all leaf conversion failure modes
**Plans:** 1 plan

Plans:
- [x] 02-01-PLAN.md — Error type, geometry primitives, enum conversions, oneof conversions, scalar wrappers, and comprehensive tests

### Phase 3: Composite Type Conversions and Round-Trip Tests
**Goal**: All aggregate embodiment types (Joint, Link, EmbodimentModel, EmbodimentRuntime) convert losslessly; round-trip property tests prove identity
**Depends on**: Phase 2
**Requirements**: CONV-04, CONV-05
**Success Criteria** (what must be TRUE):
  1. All ~25 type pairs have working From/TryFrom impls (domain -> proto -> domain == identity for every type)
  2. Digest fields (model_digest, calibration_digest, manifest_digest, combined_digest) pass through as opaque strings -- never recomputed
  3. Round-trip property tests pass for every converted type including EmbodimentModel and EmbodimentRuntime
**Plans:** 2 plans

Plans:
- [ ] 03-01-PLAN.md — Composite From/TryFrom impls for all ~16 type pairs, PartialEq on EmbodimentRuntime, proptest dev-dep
- [ ] 03-02-PLAN.md — Proptest round-trip property tests for every converted type (CONV-05)

### Phase 4: Service Implementation
**Goal**: A working EmbodimentService serves GetModel, GetRuntime, ListBindings, and ValidateBindings RPCs; substrate-ide can fetch embodiment data over gRPC
**Depends on**: Phase 3
**Requirements**: SERV-01, SERV-02, SERV-03
**Success Criteria** (what must be TRUE):
  1. `GetModel` RPC returns a complete EmbodimentModel for a given host identifier
  2. `GetRuntime` RPC returns the compiled EmbodimentRuntime (model + calibration + safety overlays + digests)
  3. `ListBindings` and `ValidateBindings` RPCs return correct results
  4. Service is registered in server startup and appears in gRPC reflection
**Plans**: TBD

Plans:
- [ ] 04-01: TBD

## Progress

**Execution Order:**
Phases execute in numeric order: 1 -> 2 -> 3 -> 4

| Phase | Plans Complete | Status | Completed |
|-------|----------------|--------|-----------|
| 1. Proto Definition and Build Integration | 0/2 | Not started | - |
| 2. Leaf Type Conversions | 0/1 | Not started | - |
| 3. Composite Type Conversions and Round-Trip Tests | 0/2 | Not started | - |
| 4. Service Implementation | 0/1 | Not started | - |
