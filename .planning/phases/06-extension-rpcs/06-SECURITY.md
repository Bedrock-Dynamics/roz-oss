# Phase 06 Security Audit

**Phase:** 06 -- Extension RPCs
**Audited:** 2026-04-08
**Threats Closed:** 6/6
**ASVS Level:** 1

## Threat Verification

| Threat ID | Category | Disposition | Status | Evidence |
|-----------|----------|-------------|--------|----------|
| T-06-01 | Tampering | accept | CLOSED | Proto field numbers are independent per message; RetargetingMap starts at field 1 with no conflict. Verified in `proto/roz/v1/embodiment.proto:241-245`. |
| T-06-02 | Spoofing | mitigate | CLOSED | `authenticated_tenant_id()` called at entry of both `get_retargeting_map` (line 269) and `get_manifest` (line 303) in `crates/roz-server/src/grpc/embodiment.rs`, before any DB access. |
| T-06-03 | Information Disclosure | mitigate | CLOSED | `fetch_embodiment_row()` (lines 72-92) checks `row.tenant_id != tenant_id` and returns `Status::not_found("host not found")` -- never FORBIDDEN. Both new handlers call this shared function. |
| T-06-04 | Information Disclosure | mitigate | CLOSED | Serde errors logged via `tracing::error!` but client receives only `Status::internal("failed to deserialize embodiment data")`. Verified at lines 281 and 315. No serde detail leakage. |
| T-06-05 | Tampering | mitigate | CLOSED | `parse_host_id()` (line 64) validates UUID format, returns `Status::invalid_argument("host_id is not a valid UUID")`. Called at lines 270 and 304 for both new handlers. |
| T-06-06 | Denial of Service | accept | CLOSED | RetargetingMap computation is O(n) on bindings count (typically <100). No amplification vector. Accepted risk documented. |

## Accepted Risks

| Threat ID | Category | Risk Statement | Justification |
|-----------|----------|----------------|---------------|
| T-06-01 | Tampering | New proto messages could theoretically conflict with existing field numbers | Each proto message has independent field numbering; no conflict possible. Standard protobuf behavior. |
| T-06-06 | Denial of Service | On-the-fly retargeting map computation could be used for DoS | Computation is O(n) on bindings count, bounded by embodiment size (typically <100 bindings). Rate limiting at the gRPC transport layer provides additional protection. |

## Unregistered Flags

None. No `## Threat Flags` section found in 06-01-SUMMARY.md or 06-02-SUMMARY.md.
