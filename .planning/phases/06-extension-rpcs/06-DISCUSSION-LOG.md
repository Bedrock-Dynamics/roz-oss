# Phase 6: Extension RPCs - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-04-08
**Phase:** 06-extension-rpcs
**Areas discussed:** Proto additions, Coverage metadata, Data extraction, Conversion layer, Error handling
**Mode:** --auto (all decisions auto-selected using recommended defaults)

---

## Proto Additions

| Option | Description | Selected |
|--------|-------------|----------|
| Extend existing EmbodimentService | Add RPCs to the existing service definition | ✓ |
| New ExtensionService | Create a separate gRPC service for extension RPCs | |

**User's choice:** [auto] Extend existing EmbodimentService (recommended — natural extension, same auth/DB pattern)
**Notes:** RetargetingMap message needs to be added. ControlInterfaceManifest already exists.

---

## Coverage Metadata

| Option | Description | Selected |
|--------|-------------|----------|
| Include in response wrapper | mapped_count + total_binding_count in GetRetargetingMapResponse | ✓ |
| Include in RetargetingMap message | Add counts directly to the RetargetingMap proto type | |

**User's choice:** [auto] Response wrapper (recommended — keeps the core type clean, metadata is query-specific)
**Notes:** Clients compute coverage % as mapped_count / total_binding_count.

---

## Data Extraction

| Option | Description | Selected |
|--------|-------------|----------|
| Compute on-the-fly | Deserialize JSONB runtime, call from_bindings() | ✓ |
| Pre-compute and store | Worker uploads RetargetingMap alongside model | |

**User's choice:** [auto] Compute on-the-fly (recommended — per Out of Scope: "no cache invalidation needed")
**Notes:** Same strategy for ControlInterfaceManifest: extract from stored JSONB.

---

## Claude's Discretion

- Proto field naming/numbering
- Test fixture data for roundtrip proptests
- Whether to add DB helper or reuse existing get_by_host_id

## Deferred Ideas

None
