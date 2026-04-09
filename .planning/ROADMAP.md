# Roadmap: Roz Embodiment Protos

## Milestones

- ✅ **v1.0 Roz Embodiment Protos** — Phases 1-4 (shipped 2026-04-08)
- 🚧 **v1.1 Embodiment Streaming, CLI, and Extensions** — Phases 5-8 (in progress)

## Phases

<details>
<summary>v1.0 Roz Embodiment Protos (Phases 1-4) — SHIPPED 2026-04-08</summary>

- [x] Phase 1: Proto Definition and Build Integration (2/2 plans) — completed 2026-04-07
- [x] Phase 2: Leaf Type Conversions (1/1 plan) — completed 2026-04-07
- [x] Phase 3: Composite Type Conversions and Round-Trip Tests (2/2 plans) — completed 2026-04-07
- [x] Phase 4: Service Implementation (2/2 plans) — completed 2026-04-08

</details>

### v1.1 Embodiment Streaming, CLI, and Extensions

- [ ] **Phase 5: Worker Embodiment Upload Wiring** - Wire upload_embodiment into worker startup with digest-based conditional upload
- [ ] **Phase 6: Extension RPCs** - Add GetRetargetingMap and GetManifest unary RPCs with coverage metadata
- [ ] **Phase 7: Streaming RPCs** - Add StreamFrameTree and WatchCalibration server-streaming RPCs with digest-based change detection
- [ ] **Phase 8: CLI Embodiment Commands** - Add host embodiment inspection and validation commands to roz CLI

## Phase Details

### Phase 5: Worker Embodiment Upload Wiring
**Goal**: Workers automatically upload their embodiment model to the server at startup, skipping when unchanged
**Depends on**: Phase 4 (upload_embodiment function and REST endpoint exist)
**Requirements**: WIRE-01, WIRE-02
**Success Criteria** (what must be TRUE):
  1. Worker uploads embodiment model to server after successful host registration without manual intervention
  2. Worker skips upload when server already has a model with matching digest (no redundant writes)
  3. Server has embodiment data in DB for any registered worker that has an EmbodimentModel, enabling all downstream queries
**Plans:** 2 plans
Plans:
- [x] 05-01-PLAN.md — Server-side digest comparison for conditional write (204/200)
- [x] 05-02-PLAN.md — Worker config field, manifest loading, and upload wiring

### Phase 6: Extension RPCs
**Goal**: Clients can fetch retargeting maps and control interface manifests as standalone lightweight queries
**Depends on**: Phase 5 (DB must be populated with embodiment data from worker upload)
**Requirements**: EXT-01, EXT-02, EXT-03
**Success Criteria** (what must be TRUE):
  1. Client can fetch canonical-to-local joint mapping for a host via GetRetargetingMap RPC without fetching the full model
  2. Client can fetch ControlInterfaceManifest via GetManifest RPC without fetching the full model
  3. GetRetargetingMap response includes mapped and total binding counts so clients can report coverage percentage
**Plans:** 2 plans
Plans:
- [x] 06-01-PLAN.md — Proto messages, RPC declarations, and RetargetingMap conversions with proptest
- [x] 06-02-PLAN.md — GetRetargetingMap and GetManifest handler implementations

### Phase 7: Streaming RPCs
**Goal**: Connected clients receive real-time updates when a host's frame tree structure or calibration overlays change
**Depends on**: Phase 5 (DB must be populated; change notifications require stored state to diff against)
**Requirements**: STRM-01, STRM-02, STRM-03
**Success Criteria** (what must be TRUE):
  1. Client connected via StreamFrameTree receives frame tree updates when the host's frame tree changes on the server
  2. Client connected via WatchCalibration receives calibration overlay updates when calibration data changes on the server
  3. Streaming responses include a digest field so clients can compare against their local state and detect actual data changes vs keepalives
  4. Streams remain open and deliver keepalives when no changes occur, without dropping the connection
**Plans:** 2 plans
Plans:
- [ ] 07-01-PLAN.md — Streaming proto messages, NATS change event plumbing, and EmbodimentServiceImpl wiring
- [ ] 07-02-PLAN.md — StreamFrameTree and WatchCalibration RPC handler implementations

### Phase 8: CLI Embodiment Commands
**Goal**: Operators can inspect and validate embodiment data for any registered host from the command line
**Depends on**: Phase 5 (DB must be populated with embodiment data); Phase 6 (validate uses existing RPCs)
**Requirements**: CLI-01, CLI-02, CLI-03
**Success Criteria** (what must be TRUE):
  1. Operator can run `roz host embodiment <id>` and see the host's embodiment model summary (joints, links, frame tree stats)
  2. Operator can run `roz host bindings <id>` and see the host's channel bindings with semantic roles
  3. Operator can run `roz host validate <id>` and see binding validation results with pass/fail status and specific errors
**Plans**: TBD
**UI hint**: yes

## Progress

| Phase | Milestone | Plans Complete | Status | Completed |
|-------|-----------|----------------|--------|-----------|
| 1. Proto Definition and Build Integration | v1.0 | 2/2 | Complete | 2026-04-07 |
| 2. Leaf Type Conversions | v1.0 | 1/1 | Complete | 2026-04-07 |
| 3. Composite Type Conversions and Round-Trip Tests | v1.0 | 2/2 | Complete | 2026-04-07 |
| 4. Service Implementation | v1.0 | 2/2 | Complete | 2026-04-08 |
| 5. Worker Embodiment Upload Wiring | v1.1 | 0/2 | Planned | - |
| 6. Extension RPCs | v1.1 | 0/2 | Planned | - |
| 7. Streaming RPCs | v1.1 | 0/2 | Planned | - |
| 8. CLI Embodiment Commands | v1.1 | 0/0 | Not started | - |
