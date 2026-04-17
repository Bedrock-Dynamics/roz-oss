# Phase 23: Two-direction Ed25519 signed dispatch - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-04-17
**Phase:** 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
**Mode:** Auto (`--auto` flag — recommended defaults selected without user interaction)
**Areas discussed:** Envelope carriage, Canonicalization, Replay protection, Private key storage, Bootstrap attestation, Rotation, Signing-failure behavior, Revocation, Sim/CI story, Performance, Migration

---

## A. Envelope Carriage

| Option | Description | Selected |
|--------|-------------|----------|
| **NATS message header** | Single `roz-sig-v1` header carrying base64(JCS envelope + 64-byte sig). Payload unchanged. | ✓ |
| Payload wrapper struct | `SignedTaskInvocation { invocation, envelope }` — breaks existing consumers. | |
| NATS subject suffix | Add `.signed` variant subjects; dual-publish during transition. | |

**Auto-selection rationale:** Research doc §2 explicitly recommends NATS header. Zero payload-shape change for existing consumers. Logged as D-01.

## B. Canonicalization

| Option | Description | Selected |
|--------|-------------|----------|
| **JCS (RFC 8785) via `jcs` crate** | Deterministic serialization across Rust versions. | ✓ |
| Bincode fixed layout | Faster but Rust-only, no cross-language interop if we ever need it. | |
| JSON via `serde_json` | Map ordering is unspecified — will break. | |

**Auto-selection rationale:** Research doc §2 locks JCS. Logged as D-02.

## C. Replay Protection Scope

| Option | Description | Selected |
|--------|-------------|----------|
| **`(direction, host_id, tenant_id, key_version)`** | Counter per tuple, resets on rotation. | ✓ |
| `(host_id, tenant_id)` only | Simpler but mixes directions. |  |
| Timestamp only | Doesn't survive clock skew on embedded devices. | |

**Auto-selection rationale:** Research doc §2 + v2.2 PROJECT.md design decision both call for per-direction scope. Counter-reset-on-rotation is the cleanest invariant. Logged as D-03, D-04.

## D. Private Key Storage

| Option | Description | Selected |
|--------|-------------|----------|
| **Encrypted file at `/etc/roz/device-key.pem`** | AES-GCM with `ROZ_KEY_ENCRYPTION_KEY`. Mode 0600. | ✓ |
| OS keyring (`keyring` crate) | Works on Linux w/ dbus, headless compatibility questionable on SBCs. | |
| TPM / secure enclave | Pixhawk companion boards typically lack TPM. | |

**Auto-selection rationale:** Pragmatic minimum for Pixhawk-class deployment (Phase 28). TPM is a future hardening phase. Logged as D-05.

## E. Bootstrap Attestation

| Option | Description | Selected |
|--------|-------------|----------|
| **Reuse per-host `ROZ_API_KEY`** | Already issued at worker registration. Bearer-auth the provision endpoint. | ✓ |
| One-time bootstrap token | New infra: mint short-lived token at registration, consume on first key provision. | |
| mTLS for provision endpoint | New cert infrastructure; out of v3.0 scope. | |

**Auto-selection rationale:** No new credential infra needed; piggybacks on Phase 17 registration. Rate-limited to prevent stolen-key abuse. Logged as D-06.

## F. Rotation Trigger

| Option | Description | Selected |
|--------|-------------|----------|
| **Worker-polled expiry at startup + every 24h heartbeat** | 90-day default lifetime; mid-task rotations defer to next task boundary. | ✓ |
| Server-pushed via NATS signal | Adds a new signaling surface and coordination complexity. | |
| Strictly manual via CLI | No ambient safety net if operator forgets. | |

**Auto-selection rationale:** Worker-poll is simpler; 24h detection lag is fine for a 90-day rotation cadence. Manual CLI override still available. Logged as D-07.

## G. Signing-Failure Behavior

| Option | Description | Selected |
|--------|-------------|----------|
| **Fail-closed, hard-stop** | Missing/corrupt key → worker crash-loops; operator re-enrolls. No buffering. | ✓ |
| Buffer-and-retry | Buffer unsigned outgoing to WAL, retry after re-enrollment. | |
| Fall back to unsigned | Silently degrades security; unacceptable. | |

**Auto-selection rationale:** Safety-first default per v3.0 design principle. Silent retry masks security issues. Logged as D-09.

## H. Server Rejection Handling

| Option | Description | Selected |
|--------|-------------|----------|
| **Publish `safety.signature_failure.{host_id}` + explicit worker error** | Worker observes error, triggers re-enrollment. | ✓ |
| Server-side silent drop | Worker blind to the rejection; no recovery path. | |
| Auto-issue new keypair | Bypasses attestation; a stolen key could auto-replace itself. | |

**Auto-selection rationale:** Explicit error + observable subject for ops. Aligns with fail-closed principle. Logged as D-09.

## I. Revocation Semantics

| Option | Description | Selected |
|--------|-------------|----------|
| **Immediate rejection at verification, no grace** | `revoked_at` set → next verify fails. In-flight envelopes timeout. | ✓ |
| Grace period (e.g., 1h) | Allows in-flight to complete but keeps a revoked key valid for that window. | |
| Scheduled rotation only | Doesn't handle emergency compromise. | |

**Auto-selection rationale:** Safety-first. Research doc §3 recommends no auto-recovery on revocation. Logged as D-08.

## J. Sim/CI Worker Keys

| Option | Description | Selected |
|--------|-------------|----------|
| **Auto-gen ephemeral via same `/v1/device/provision-key` endpoint** | Fresh keypair per sim container / CI run, destroyed on exit. | ✓ |
| Baked-in dev key in repo | Secrets in git; fingerprint trivial to leak. | |
| Skip signing in dev/CI entirely | Tests wouldn't exercise the real enrollment flow. | |

**Auto-selection rationale:** Production-realistic test path, no committed secrets. Logged as D-10.

## K. Performance Cache

| Option | Description | Selected |
|--------|-------------|----------|
| **In-memory LRU cache, 60 s TTL, invalidated on revocation** | Verify hot-path < 100 µs. | ✓ |
| No cache | DB lookup per verify (~1-5 ms) dominates; unacceptable at dispatch rate. | |
| Redis cache | New infra; overkill for per-server scope. | |

**Auto-selection rationale:** Research doc §4 recommends LRU with TTL. Logged as D-11.

## L. Migration Strategy

| Option | Description | Selected |
|--------|-------------|----------|
| **`SIGNED_DISPATCH_ENFORCEMENT` env (off / audit / strict)** | Rollout window; strict default for fresh deployments. | ✓ |
| Big-bang cutover | Breaks any in-fleet worker that hasn't upgraded. | |
| Always-strict | Same issue; no upgrade path. | |

**Auto-selection rationale:** Gives operators a rollout window without leaving a permanent escape hatch. Logged as D-12.

---

## Claude's Discretion (planner decides)

- Exact Rust module layout (`roz-sig` new crate vs extending `roz-nats` / `roz-core`)
- LRU cache crate choice (`lru` / `moka` / `quick_cache`)
- Error type hierarchy (`SignatureError` placement and wrapping)
- DB migration number for `roz_device_keys` table
- Single vs split HTTP routes for provision + rotate

## Deferred Ideas

- Hardware-backed keys (TPM / secure enclave) — future hardening phase.
- Per-tenant master key derivation (HKDF) — multi-tenant hardening phase.
- Signed firmware manifest delivery unification with dispatch signing — post-v3.0.
- NATS operator-JWT root-of-trust integration — post-v3.0.
- Append-only signing-counter journal — robustness improvement, not required for correctness.
