# Phase 23: Two-direction Ed25519 signed dispatch and per-device key provisioning - Context

**Gathered:** 2026-04-17
**Status:** Ready for planning
**Mode:** Auto-generated (`--auto` — recommended defaults anchored in `.planning/research/DEEP-SIGN.md`)

<domain>
## Phase Boundary

Every NATS hop between server and worker carries an Ed25519 signature envelope. Server signs outgoing task dispatch; worker verifies. Worker signs outgoing results/telemetry/events/trust-reports; server verifies. Every verification is fail-closed. Per-device keypairs are provisioned through a bootstrap endpoint the first time a worker starts, persisted on-device, and rotatable. Sim and CI workers use the same provisioning path as production workers.

Scope anchor: requirement **FS-04** ("two-direction signed dispatch") plus its key-lifecycle dependencies. Signed envelopes extend the existing `TaskInvocation`, result publish, telemetry publish, and trust-report publish paths — they do NOT replace them.

Out of scope:
- mTLS / transport-layer auth (NATS operator credentials remain as-is)
- Hardware attestation (TPM, secure enclave) — explicitly deferred as a separate future phase
- Per-tenant key derivation (keys are per-host; tenant is a signed field, not a key property)

</domain>

<decisions>
## Implementation Decisions

### D-01 — Envelope carriage (NATS headers, not payload wrapping)
[auto] Signature envelope rides in NATS message headers, not wrapped around the payload struct.
**Why:** Zero payload-shape change — existing consumers keep deserializing `TaskInvocation` / `TaskResult` / telemetry structs unchanged. New verifiers read the header. Matches `.planning/research/DEEP-SIGN.md §2` recommendation.
**Header name:** `roz-sig-v1` (single header, base64-encoded JCS of envelope fields + raw 64-byte signature appended).

### D-02 — Canonicalization (JCS via `jcs` crate)
[auto] Signed-fields bundle is serialized using RFC 8785 JCS before hashing. Adds `jcs = "0.1"` (or latest compatible) to workspace dependencies in `roz-core`.
**Why:** Signer and verifier may run different Rust versions / dependency SHAs. JCS removes any dependency on `serde_json`'s map ordering or whitespace. Same `.planning/research/DEEP-SIGN.md §2` recommendation.

### D-03 — Signed fields (mandatory set)
[auto] Every envelope signs the 6-field set recommended in research:
- `direction` ("server→worker" | "worker→server") — prevents cross-direction replay
- `tenant_id` (UUID)
- `host_id` (UUID) — for worker-signed envelopes, this is the sender; for server-signed, this is the recipient
- `task_id` or `stream_id` (UUID) — the correlation ID for the envelope's target
- `timestamp` (RFC3339 with microsecond precision, UTC)
- `sequence_number` (u64, monotonic, per `(direction, host_id, tenant_id, key_version)` scope)
- `payload_hash` (SHA-256 of the full NATS payload bytes, hex-encoded)
- `key_version` (u32)

Signature binds: `JCS({direction, tenant_id, host_id, task_id|stream_id, timestamp, sequence_number, payload_hash, key_version})`.

### D-04 — Replay protection (sequence + timestamp window)
[auto] Verification rejects if:
- `sequence_number ≤ cached_seq` for the `(direction, host_id, tenant_id, key_version)` tuple — sequence underflow
- `|timestamp - now| > 5s` — outside skew window (covers clocks without NTP on embedded devices; narrower than 5 minutes typical JWT windows because signing is tight-loop)

**Counter reset on rotation:** A new `key_version` starts its counter at 0. This makes rotation clean and avoids the "must persist counter across rotations" complexity.

### D-05 — Per-device key storage (encrypted file, not TPM)
[auto] Worker stores private key at `/etc/roz/device-key.pem` (mode `0600`, owner `roz-worker`) in production. Dev/sim workers use `${ROZ_DATA_DIR}/device-key.pem` (falls back to `~/.config/roz/device-key.pem`).
**Why:** Pixhawk companions (typical SBC) rarely ship with TPM exposed to userland. Encrypted file + OS access control is the pragmatic minimum. A future hardening phase can add a TPM/enclave `KeyProvider` behind the same trait surface without changing the signing code.
**Crate:** `aes-gcm` (already a workspace dep — added for Phase 17/v2.1 MCP credentials) encrypts the key at rest with a master key derived from `ROZ_KEY_ENCRYPTION_KEY` env (which already gates MCP credential storage).

### D-06 — Bootstrap attestation (reuse per-host `ROZ_API_KEY`)
[auto] First-time enrollment: worker posts `POST /v1/device/provision-key` with `Authorization: Bearer ${ROZ_API_KEY}` header (the per-host key the worker already has from Phase 17 registration). Server validates the API key belongs to a registered host, generates an Ed25519 keypair, stores public key + `key_version=1` in new `roz_device_keys` table, returns private key **once** in the response.
**Why:** No new credential infrastructure needed. Per-host API keys are already how worker authenticates to the control plane. Key is long-lived but scoped to one host — adequate for the bootstrap one-shot.
**Rate-limit:** Single successful provision per host per hour prevents abuse of stolen API keys (real worker would re-enroll via operator CLI, not a retry storm).

### D-07 — Rotation (worker-polled, 90-day default, mid-task safe)
[auto] Worker checks its key's `created_at` against current time at startup and once per 24 h heartbeat. If age > `rotation_interval_seconds` config (default `90 × 86400`), worker fetches a new keypair via `POST /v1/device/rotate-key` (signed with the current key, not the API key). Both old + new keys remain valid for 24 h — in-flight tasks finish with the old key, new sends use the new key.
**Why:** Worker-polled is simpler than a server-push signal (no new subject). 24 h detection lag is fine for a 90-day rotation cadence.
**Manual override:** `roz device rotate-key` CLI triggers immediate rotation.

### D-08 — Revocation (server-side, fail-closed at verification)
[auto] Operator sets `revoked_at` in `roz_device_keys`. Next envelope signed with that key is rejected at verification. Worker sees the rejection and re-enrolls via the bootstrap endpoint. In-flight signed-but-not-yet-verified envelopes timeout naturally (fail-closed is the safety default — no automatic acceptance, no grace).

### D-09 — Signing-failure behavior (fail-closed, no buffering)
[auto] **Worker-side** (signing outgoing messages): If local key is missing, corrupted, or decryption fails at runtime, worker hard-stops with a clear log message. Operator re-enrolls. No buffering of "would-have-signed" messages — fail-closed means silent.
**Server-side** (verifying incoming worker messages): Reject, log audit event, emit to `safety.signature_failure.{host_id}` NATS subject (new, for ops observability). Worker observes the rejection via its normal error-return path and treats it as re-enrollment trigger.

### D-10 — Sim/CI worker keys (same provisioning path, ephemeral)
[auto] Sim and CI workers auto-generate keypairs on first run via the same `POST /v1/device/provision-key` endpoint. Private key lives in the container/test workspace and is destroyed when the worker exits. No secrets committed to the repo. CI runs exercise the real enrollment flow as part of startup.
**Why:** Forces the test suite to validate the production enrollment path. Avoids "it works in prod but not in CI because we stubbed it" surprises.

### D-11 — Performance (verifying-key cache with TTL)
[auto] Server-side verification uses an in-memory LRU cache keyed by `(tenant_id, host_id, key_version)` with 60 s TTL. Ed25519 verify is ~30 µs; the DB lookup for the public key is 1-5 ms. Caching brings the typical hot-path verification under 100 µs end-to-end.
**Cache invalidation:** Revocation clears the cache for that host synchronously. Rotation cache entries expire naturally within 60 s.

### D-12 — Migration (envelope optional during rollout)
[auto] New `SIGNED_DISPATCH_ENFORCEMENT` env var on the server: `off` (warn but accept unsigned), `audit` (require signed but don't reject), `strict` (reject unsigned — production default after v3.0 ships). Workers always sign once provisioned. This gives a rollout window where existing workers can be upgraded phase-by-phase without a fleet-wide simultaneous cutover.
**Default:** `strict` for fresh deployments; v3.0 shipped workers default to signing. The gate is only relevant for pre-v3.0 workers still in the fleet during upgrade.

### Claude's Discretion

- Exact Rust module layout (one `roz-sig` crate vs new types in `roz-nats`? — planner decides based on import-graph complexity)
- LRU cache crate choice (`lru`, `moka`, `quick_cache` — planner picks based on existing workspace deps)
- Error type hierarchy (new `SignatureError` enum, or flatten into `McpError`-style domain errors per subsystem)
- Migration file name/number for `roz_device_keys` table (fits the existing `migrations/NNN_*.sql` pattern)
- Whether to split bootstrap + rotation into separate proto RPCs or one endpoint with discriminator

</decisions>

<canonical_refs>
## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Research and policy
- `.planning/research/DEEP-SIGN.md` — Full FS-04 research: envelope design, replay protection, key lifecycle, trust-posture interplay, concrete requirements.
- `docs/robot-policy.md` — Phase 22 output; establishes that all v3.0+ backend PRs (including this signing code) cite it. Signing does not conflict with the native-vs-bridge decision rule — it wraps both.

### Existing signing / trust code
- `crates/roz-core/src/device_trust/verify.rs` — Existing `verify_firmware_signature` using `ed25519_dalek::Verifier`. Pattern to reuse for the new dispatch verifier (same crate, same key format).
- `crates/roz-core/src/device_trust/evaluator.rs` — `DeviceTrustPosture` evaluator. Signed-dispatch posture gating reads from this.
- `crates/roz-core/src/device_trust/mod.rs` — `FirmwareManifest.ed25519_signature` field pattern. Envelope design parallels this shape.

### Existing related infrastructure
- `crates/roz-worker/src/registration.rs` — Worker registration + per-host API key material; bootstrap endpoint piggybacks on this credential.
- `crates/roz-worker/src/wal.rs` — SQLite-backed worker WAL; sequence-number persistence lands here.
- `crates/roz-db/src/` — DB module pattern; `roz_device_keys` table module follows the `mcp_servers.rs` / `hosts.rs` shape.
- `crates/roz-server/src/routes/tasks.rs` — Task dispatch path; verification hook inserts before `nats.publish()`.
- `crates/roz-nats/src/subjects.rs` — NATS subject builder module; `safety.signature_failure.{host_id}` subject added here.

### Requirements anchor
- `.planning/REQUIREMENTS.md` §FS-04 — "two-direction signed dispatch on every NATS hop." All acceptance criteria derive from this entry.
- `.planning/PROJECT.md` §v3.0 Design Decisions — "Two-direction signed dispatch on every NATS hop. Replay-protected per `(direction, host_id, tenant_id)`."

### Key dependencies (new to workspace or existing)
- `ed25519-dalek` — already a workspace dep (used by `device_trust/verify.rs`)
- `aes-gcm` — already a workspace dep (added for v2.1 MCP credentials)
- `jcs` — **NEW** — add `jcs = "0.1"` (or current compatible) for RFC 8785 canonicalization
- `rusqlite` — already in `roz-worker` (WAL) — reused for worker-side counter persistence

</canonical_refs>

<code_context>
## Existing Code Insights

### Reusable Assets
- **`ed25519_dalek` verify path** (`crates/roz-core/src/device_trust/verify.rs:35`) — existing `verify_firmware_signature(data, public_key_bytes, signature_bytes) → bool` pattern. The new dispatch verifier uses the same crate + key format; sign + verify helpers can share the private-key/public-key struct layout.
- **`aes-gcm` + `ROZ_KEY_ENCRYPTION_KEY`** — Phase 17 added this for encrypting MCP OAuth / bearer credentials in `roz_mcp_server_credentials` table. Reuse for encrypting the worker's private-key-at-rest on disk. `crates/roz-core/src/key_provider.rs` is the API surface.
- **Per-host `ROZ_API_KEY`** — already issued at worker registration (`crates/roz-worker/src/registration.rs`). Reuse as the bootstrap authentication credential for `POST /v1/device/provision-key` — no new credential infrastructure.
- **`roz-worker/src/wal.rs` SQLite store** — existing WAL crate for idempotency and in-flight task persistence. Adds a `signing_sequence_counter` table for the worker-side monotonic counter.

### Established Patterns
- **`thiserror` enums at crate boundary** — `roz-core`, `roz-db`, `roz-nats` all define domain-specific error enums. Signing error type follows this (e.g., `SignatureError` in `roz-core`, surfaced through each caller as domain-wrapped).
- **`tracing` structured logs** — every module uses `tracing::info!`/`warn!`/`error!` with named fields. Audit events for signature failures follow this (`tracing::error!(host_id=%host, seq=%seq, reason=%r, "signature_verification_failed")`).
- **SQLx migrations numbered sequentially** — `migrations/001_tenants.sql` through `020_session_turns.sql`. New `roz_device_keys` table adds `021_device_keys.sql` (or next available).
- **Single-writer model for mutable state** — `roz_device_keys.sequence_number_offset` updated only via the verification gate's atomic SQL `UPDATE ... WHERE sequence_number_offset < $1 RETURNING *` pattern (same as MCP credential rotation).

### Integration Points
- **Server-side verification hook:** `crates/roz-server/src/routes/tasks.rs` — `post_task_dispatch` and its siblings call `verify_inbound_signature(headers, payload_bytes, &pool)` before any business logic. Fail-closed: return HTTP 401 + structured error on any verification failure.
- **Server-side signing hook:** `crates/roz-nats/src/dispatch.rs` — `TaskDispatch::publish()` wraps the existing publish call with a `sign_outbound(direction, envelope_fields, payload_bytes, &server_keypair)` step that produces the header to attach.
- **Worker-side signing hook:** `crates/roz-worker/src/dispatch.rs` — worker's result/telemetry publish paths each call `sign_outbound(...)` similarly.
- **Worker-side verification hook:** `crates/roz-worker/src/main.rs` — NATS subscriber callbacks for task dispatch verify the header before deserializing the payload into `TaskInvocation`.
- **New HTTP routes (server):** `POST /v1/device/provision-key`, `POST /v1/device/rotate-key` — added under `crates/roz-server/src/routes/device.rs` (or extend existing device route module).

</code_context>

<specifics>
## Specific Ideas

- **Research doc `DEEP-SIGN.md` is normative.** Planner should read §§1-6 in full before designing task breakdown. The envelope field set, replay protection strategy, bootstrap flow, and rotation/revocation semantics are all derived from it — deviations need explicit justification.
- **Failure subject `safety.signature_failure.{host_id}`** is a new NATS subject (publish-only, ops-observability). Add to `crates/roz-nats/src/subjects.rs`.
- **Migration budget:** One new migration, one new DB table, one new pair of HTTP routes, one new header, one new crate dep. Modest surface area; the bulk of the work is wiring verification into existing publish/subscribe sites.
- **Test coverage:** The research doc's §6 concrete requirements (FS-04.1/2/3) give a directly-actionable test matrix — round-trip signing, tampered payload rejection, replay rejection, revocation rejection, rotation overlap. Planner uses these as the acceptance test skeleton.

</specifics>

<deferred>
## Deferred Ideas

- **Hardware-backed keys (TPM / secure enclave).** Explicitly deferred to a future hardening phase. The `KeyProvider`-style abstraction in D-05 makes this a drop-in backend swap, not a rewrite.
- **Per-tenant master key derivation (HKDF).** Tenant-scoped crypto isolation beyond the `tenant_id` signed field. Useful long-term but out of scope for v3.0; belongs in a separate multi-tenant hardening phase.
- **Signed firmware manifest delivery.** The existing firmware-sig verification in `device_trust/verify.rs` already signs firmware; a separate unified "sign all attestation material including firmware" consolidation can wait until after v3.0 ships.
- **NATS operator-JWT integration.** Tighter integration with NATS's own NKeys/JWT model (use the operator account as the root-of-trust for device keys). Research territory for a post-v3.0 transport-hardening phase.
- **Sequence-counter journal.** Worker currently persists the last-sent counter in SQLite. A full append-only journal (like WAL for task state) is a larger robustness win but not required for correctness — the counter is recoverable by asking the server for the last-seen value.

</deferred>

---

*Phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning*
*Context gathered: 2026-04-17*
*Mode: auto (recommended defaults from DEEP-SIGN.md)*
