# Phase 7: Streaming RPCs - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md -- this log preserves the alternatives considered.

**Date:** 2026-04-09
**Phase:** 07-streaming-rpcs
**Areas discussed:** Change source, Stream content, Proto design, Failure modes

---

## Change Source

| Option | Description | Selected |
|--------|-------------|----------|
| NATS event on PUT | PUT handler publishes change event to NATS subject. Matches StreamTaskStatus/WatchTeam patterns. Requires adding NATS to EmbodimentServiceImpl. | :heavy_check_mark: |
| Polling DB with interval | Poll DB on interval, compare digest. No NATS dependency but adds latency. | |
| tokio::sync::broadcast in AppState | In-process broadcast channel. Zero external dependency but single-process only. | |

**User's choice:** NATS event on PUT
**Notes:** None

### Follow-up: NATS requirement

| Option | Description | Selected |
|--------|-------------|----------|
| Required | Return FAILED_PRECONDITION if NATS unavailable. Matches StreamTaskStatus pattern. | :heavy_check_mark: |
| Fallback to polling | Fall back to DB polling without NATS. More resilient but complex. | |

**User's choice:** Required
**Notes:** None

---

## Stream Content

| Option | Description | Selected |
|--------|-------------|----------|
| Full snapshot + digest | Every response includes complete data plus digest. Simple, stateless. | |
| Digest-only, client re-fetches | Response only has digest. Client calls GetModel if different. | |
| Delta updates after initial snapshot | First message full snapshot, subsequent only changed data. | :heavy_check_mark: |

**User's choice:** Delta updates after initial snapshot
**Notes:** User explicitly chose to pull STRM-04 (delta pattern, deferred to v2 in REQUIREMENTS.md) into Phase 7 scope.

### Follow-up: Initial snapshot

| Option | Description | Selected |
|--------|-------------|----------|
| Immediate snapshot | First message always full snapshot. Client hydrated immediately. | :heavy_check_mark: |
| Wait for first change | Client gets nothing until data changes. | |

**User's choice:** Immediate snapshot
**Notes:** None

### Follow-up: Keepalive content

| Option | Description | Selected |
|--------|-------------|----------|
| Digest-only heartbeat | Periodic message with digest and timestamp. | :heavy_check_mark: |
| Empty ping | Signal stream alive, no digest. | |
| Full snapshot as keepalive | Resend full state periodically. | |

**User's choice:** "industry standard" (mapped to digest-only heartbeat)
**Notes:** User selected "Other" and typed "industry standard". Digest-only heartbeat is the industry standard for gRPC change-detection streams.

---

## Proto Design

| Option | Description | Selected |
|--------|-------------|----------|
| oneof payload wrapper | Response has oneof { Snapshot, Delta, Keepalive } with host_id/digest at top level. Type-safe. | :heavy_check_mark: |
| Flat message with optionals | Single flat message, client infers type from field presence. | |
| Separate message types via oneof | Top-level event wrapper with per-variant host_id/digest. Extra wrapper layer. | |

**User's choice:** oneof payload wrapper
**Notes:** User reviewed the preview showing StreamFrameTreeResponse with oneof payload.

### Follow-up: WatchCalibration symmetry

| Option | Description | Selected |
|--------|-------------|----------|
| Same oneof pattern | Consistent with StreamFrameTree. snapshot/delta/keepalive. | :heavy_check_mark: |
| Snapshot-only + keepalive | Always full CalibrationOverlay on change. No delta variant. | |

**User's choice:** Same oneof pattern
**Notes:** None

### Follow-up: Keepalive message sharing

| Option | Description | Selected |
|--------|-------------|----------|
| Shared Keepalive | One Keepalive message type for both RPCs. | :heavy_check_mark: |
| Per-stream keepalive | Separate FrameTreeKeepalive and CalibrationKeepalive. | |

**User's choice:** Shared Keepalive
**Notes:** None

---

## Failure Modes

| Option | Description | Selected |
|--------|-------------|----------|
| End stream with error | Send Status::internal, close stream. Client reconnects for fresh snapshot. | :heavy_check_mark: |
| Reconnect NATS silently | Retry subscription internally. Stream stays open. | |
| Switch to polling fallback | On NATS loss, switch to DB polling. | |

**User's choice:** End stream with error
**Notes:** Matches WatchTeam pattern.

### Follow-up: Error for missing embodiment data

| Option | Description | Selected |
|--------|-------------|----------|
| NOT_FOUND for both | Same as GetModel. No distinction between missing host and missing data. | :heavy_check_mark: |
| Separate error codes | NOT_FOUND vs FAILED_PRECONDITION. | |

**User's choice:** NOT_FOUND for both
**Notes:** Follows Phase 6 D-09 pattern.

---

## Claude's Discretion

- Keepalive interval timing
- NATS subject naming convention for embodiment change events
- Server-side diff algorithm for computing FrameTree deltas
- Proto field numbering
- Test fixture data and integration test structure

## Deferred Ideas

None -- discussion stayed within phase scope
