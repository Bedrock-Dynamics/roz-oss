# Requirements: Roz Embodiment Protos

**Defined:** 2026-04-08
**Core Value:** substrate-ide can fetch the complete embodiment model (structure, bindings, frame tree, calibration, safety overlays) over gRPC

## v1.1 Requirements

Requirements for milestone v1.1: Embodiment Streaming, CLI, and Extensions.

### Streaming RPCs

- [ ] **STRM-01**: Server can stream frame tree structural changes to connected gRPC clients via StreamFrameTree RPC
- [ ] **STRM-02**: Server can stream calibration overlay changes to connected gRPC clients via WatchCalibration RPC
- [ ] **STRM-03**: Streaming responses include digest fields so clients can detect actual data changes vs keepalives

### Extension RPCs

- [ ] **EXT-01**: Client can fetch canonical-to-local joint mapping via GetRetargetingMap unary RPC
- [ ] **EXT-02**: Client can fetch ControlInterfaceManifest via GetManifest unary RPC without fetching full model
- [ ] **EXT-03**: GetRetargetingMap response includes mapped/total binding counts for coverage reporting

### CLI

- [ ] **CLI-01**: Operator can inspect a host's embodiment model via `roz host embodiment <id>`
- [ ] **CLI-02**: Operator can inspect a host's channel bindings via `roz host bindings <id>`
- [ ] **CLI-03**: Operator can validate a host's bindings from CLI via `roz host validate <id>`

### Worker Wiring

- [ ] **WIRE-01**: Worker calls upload_embodiment() after successful host registration at startup
- [ ] **WIRE-02**: Worker skips upload when server-side model digest matches local digest (conditional upload)

## v2 Requirements

Deferred to future release. Tracked but not in current roadmap.

### Streaming Optimization

- **STRM-04**: StreamFrameTree sends initial full snapshot then only changed FrameNodes (delta pattern)

### CLI Polish

- **CLI-04**: Embodiment CLI commands support `--format` flag (JSON/YAML/table output)

## Out of Scope

| Feature | Reason |
|---------|--------|
| High-frequency (>10Hz) frame streaming | bridge.proto handles real-time telemetry; EmbodimentService is structural changes only |
| Bidirectional streaming | EmbodimentService is read-only; no client-to-server data flow needed |
| CLI gRPC client for inspection | REST is simpler for one-shot commands; reserve gRPC for streaming (TUI) |
| Separate `roz embodiment` CLI group | Embodiment is a host property; subcommands under `roz host` |
| RetargetingMap persistence in DB | Pure function of bindings + family; no cache invalidation needed |
| Manifest storage separate from runtime | Extract from runtime JSONB; separate column creates sync hazards |

## Traceability

| Requirement | Phase | Status |
|-------------|-------|--------|
| STRM-01 | Phase 7 | Pending |
| STRM-02 | Phase 9 | Pending |
| STRM-03 | Phase 7 | Pending |
| EXT-01 | Phase 6 | Pending |
| EXT-02 | Phase 6 | Pending |
| EXT-03 | Phase 6 | Pending |
| CLI-01 | Phase 8 | Pending |
| CLI-02 | Phase 8 | Pending |
| CLI-03 | Phase 8 | Pending |
| WIRE-01 | Phase 5 | Pending |
| WIRE-02 | Phase 5 | Pending |

**Coverage:**
- v1.1 requirements: 11 total
- Mapped to phases: 11
- Unmapped: 0

---
*Requirements defined: 2026-04-08*
*Last updated: 2026-04-09 after gap closure phase 9 added (STRM-02 reassigned from Phase 7)*
