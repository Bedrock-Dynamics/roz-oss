# Phase 23: Two-direction Ed25519 signed dispatch and per-device key provisioning — Research

**Researched:** 2026-04-17
**Domain:** Cryptographic message signing (Ed25519 + JCS) wired into an existing async-nats task-dispatch pipeline
**Confidence:** HIGH on design decisions (lifted from DEEP-SIGN.md); HIGH on file impact map (read the referenced Rust sources); MEDIUM on exact JCS crate choice (CONTEXT.md names `jcs`, which does not exist on crates.io — see §Dependency diff).

## Summary

Every substantive design decision for Phase 23 is already locked — envelope shape, carriage (NATS header), canonicalization (JCS / RFC 8785), replay protection (per-tuple monotonic `sequence_number` + ±5 s skew), key storage (AES-GCM-encrypted file via the existing `KeyProvider`), bootstrap (`POST /v1/device/provision-key` gated by the per-host `ROZ_API_KEY` issued during Phase 17 registration), rotation (worker-polled, 90 d), revocation (server-side, fail-closed), and rollout gate (`SIGNED_DISPATCH_ENFORCEMENT=off|audit|strict`). See `.planning/research/DEEP-SIGN.md` §§1–6 and `23-CONTEXT.md` D-01..D-12 for the authoritative spec.

This document is the **implementation-research** layer on top of that spec. It catalogs Rust-specific concerns (crate pinning, sync-vs-async signer shape, fail-closed error propagation, deterministic keypair test fixtures, JCS round-trip structure), maps every locked decision to the concrete file(s) the planner will touch, calls out integration-point pitfalls at the four wiring sites (`crates/roz-server/src/routes/task_dispatch.rs`, `crates/roz-nats` — new signing module, `crates/roz-worker/src/main.rs`, `crates/roz-worker/src/wal.rs`), and flags three open questions the planner must resolve before task breakdown. Plan-writers must read DEEP-SIGN.md §§1–6 and CONTEXT.md verbatim — this file supplements, never restates.

**Key correction to flag up-front:** CONTEXT.md references the env var `ROZ_KEY_ENCRYPTION_KEY` (D-05) and the crate `jcs = "0.1"` (D-02). Both are inaccurate relative to what actually exists. The deployed env var that already gates AES-GCM encryption is `ROZ_ENCRYPTION_KEY` (see `crates/roz-core/src/key_provider.rs:138`), not `ROZ_KEY_ENCRYPTION_KEY`. The `jcs` crate does not exist on crates.io; the active equivalent is `serde_json_canonicalizer = "0.3"`. These two are picked up in §Dependency diff and §Open questions.

**Primary recommendation:** Introduce a new top-level crate `roz-sig` (or a `signing` module in `roz-nats`) that owns `SignatureEnvelope`, `SigningKey`/`VerifyingKey` wrappers, the JCS serializer, the `sign_outbound`/`verify_inbound` helpers, and the `SignatureError` enum. Wire it at the four integration points without changing any existing serde struct. Planner decides `roz-sig` crate-vs-module in the first task.

## User Constraints (from CONTEXT.md)

### Locked Decisions

- **D-01** Envelope carriage: NATS message header `roz-sig-v1` (base64 of JCS envelope fields + 64-byte raw signature). No payload shape change.
- **D-02** Canonicalization: RFC 8785 JCS of signed-fields bundle before SHA-256 + Ed25519.
- **D-03** Signed-field set (every envelope): `direction` ∈ `{server→worker, worker→server}`, `tenant_id` (UUID), `host_id` (UUID), `task_id` or `stream_id` (UUID), `timestamp` (RFC3339 microsecond UTC), `sequence_number` (u64), `payload_hash` (SHA-256 hex of full NATS payload), `key_version` (u32). Signature binds the JCS encoding of this 8-field map.
- **D-04** Replay protection: reject when `sequence_number ≤ cached_seq` for the `(direction, host_id, tenant_id, key_version)` tuple OR when `|timestamp - now| > 5 s`. New `key_version` starts counter at 0.
- **D-05** Per-device key storage: worker persists private key at `/etc/roz/device-key.pem` (prod, mode 0600) or `${ROZ_DATA_DIR}/device-key.pem` (dev/sim), AES-GCM-encrypted at rest via the existing `KeyProvider` (master key from env). See §Open questions re: env-var name.
- **D-06** Bootstrap: `POST /v1/device/provision-key` with `Authorization: Bearer ${ROZ_API_KEY}` (per-host key issued in Phase 17 registration). Returns the private key **exactly once**. Rate-limited to 1 successful provision per host per hour.
- **D-07** Rotation: worker polls `created_at` at startup and every 24 h; rotates via `POST /v1/device/rotate-key` (signed with the current key) when age > 90 days. Old and new keys both valid for a 24 h overlap. `roz device rotate-key` CLI forces immediate rotation.
- **D-08** Revocation: operator sets `revoked_at`; next envelope signed with that key is rejected; worker re-enrolls via the bootstrap endpoint. No grace period at the verification gate.
- **D-09** Signing failures: **worker-side** hard-stops if its key is missing/corrupt/undecryptable; **server-side** rejects + logs audit event + publishes `safety.signature_failure.{host_id}`.
- **D-10** Sim/CI: uses the same enrollment path, ephemeral keys destroyed at container exit. No test-only stubs on the production enrollment flow.
- **D-11** Performance: server verification uses an in-memory LRU cache keyed by `(tenant_id, host_id, key_version)`, 60 s TTL, synchronously invalidated on revocation. Target hot-path <100 µs.
- **D-12** Migration: `SIGNED_DISPATCH_ENFORCEMENT ∈ {off, audit, strict}` server env var; workers always sign once provisioned. Default `strict` for fresh installs.

### Claude's Discretion

- Exact Rust module layout — new `roz-sig` crate vs. `signing` module in `roz-nats`. (Recommendation in §Implementation-specific findings F1.)
- LRU cache crate choice — `lru`, `moka`, or `quick_cache`. (Recommendation: `moka` — already an indirect dep via Restate; async-friendly; see F4.)
- `SignatureError` enum placement — new type in `roz-core` vs. per-crate re-wrap. (Recommendation: single type in `roz-sig` / `roz-nats::signing`; surface as `thiserror::From` into each caller's domain error. F5.)
- New migration filename — fits existing `NNN_*.sql` cadence. Next available is `021_device_keys.sql` (confirmed: migrations run through `020_session_turns.sql`).
- Bootstrap vs. rotation endpoint shape — one endpoint with `mode` discriminator vs. two endpoints. (Recommendation: two endpoints — `provision-key` is API-key-gated, `rotate-key` is current-key-signed; collapsing them requires dual-mode auth middleware for no benefit. F7.)

### Deferred Ideas (OUT OF SCOPE)

- Hardware-backed keys (TPM / secure enclave). KeyProvider-trait boundary keeps this swappable in a future phase.
- Per-tenant master-key derivation (HKDF beyond `tenant_id` as a signed field).
- Unified firmware-manifest + dispatch signing consolidation.
- NATS operator-JWT / NKeys integration with device keys.
- Append-only sequence-counter journal on the worker (current SQLite KV is sufficient for correctness).

## Phase Requirements

| ID | Description (REQUIREMENTS.md §FS-04) | Research Support |
|----|---------------------------------------|-------------------|
| FS-04 | Two-direction Ed25519 signing on every NATS hop; JCS-canonical envelope; signature in NATS header; replay protection (per-tuple `sequence_number` + ±5 s skew); `Provisional`+`Trusted` require signing, `Untrusted` blocked before signing; per-device keypairs issued via `POST /v1/device/provision-key`; failed verification → audit + `safety.signature_failure.{worker_id}` OR `safety.signature_failure.server.{tenant_id}`. | DEEP-SIGN.md §§2–6 fully specifies envelope + lifecycle. Existing `ed25519_dalek::Verifier` pattern (`crates/roz-core/src/device_trust/verify.rs:35`) is reusable. Existing `KeyProvider` (`crates/roz-core/src/key_provider.rs`) solves at-rest encryption. Existing per-host `ROZ_API_KEY` (`crates/roz-worker/src/registration.rs`) is the bootstrap auth. Existing WAL (`crates/roz-worker/src/wal.rs`) owns the sequence-counter store. |

## Authoritative References

- **`.planning/research/DEEP-SIGN.md`** — the normative design document. §1 industry landscape + rationale for per-message signing over mTLS-alone; §2 envelope fields, JCS serialization, NATS-header carriage, replay protection math; §3 bootstrap/rotation/revocation lifecycle; §4 `roz_device_keys` DDL + verification-path pseudocode; §5 trust-posture interplay (sign for `Trusted`/`Provisional`, block at `Untrusted`); §6 the FS-04.1..FS-04.5 concrete acceptance-test skeleton.
- **`.planning/phases/23-.../23-CONTEXT.md`** — locked decisions D-01..D-12 plus reusable-asset map.
- **`.planning/REQUIREMENTS.md` §FS-04** — single-source authority for acceptance criteria and fail-closed expectations. Explicitly specifies `safety.signature_failure.server.{tenant_id}` as a sibling subject to `safety.signature_failure.{worker_id}` — CONTEXT.md only mentions the worker-scoped one. Planner must surface **both** subjects.
- **`.planning/PROJECT.md` §v3.0 Design Decisions (lines 32–36)** — confirms two-direction signing is baseline for v3.0 PRs and must compose with safety policies and transport resilience (WAL store-and-forward).
- **`docs/robot-policy.md`** — Phase 22 output establishing that all v3.0+ backend PRs cite the native-vs-bridge decision rule. Signing wraps both sides and does not conflict.

## Implementation-Specific Findings

These are concerns DEEP-SIGN.md does **not** cover, grounded in the Rust codebase.

### F1. Crate boundary: prefer a new `roz-sig` crate over a module in `roz-nats`

Rationale:

- `roz-nats` currently imports `roz-core` (for auth types + phase specs). A new `roz-sig` can depend on `roz-core` (auth + errors) and be imported by `roz-nats`, `roz-server`, `roz-worker`, `roz-db`, and `roz-cli`. This matches the existing pattern (`roz-core` defines `device_trust`, not `roz-nats`).
- Placing the envelope + sign/verify helpers in `roz-sig` keeps `roz-nats` scoped to wire-format + subject-building, and avoids pulling `jcs`, `aes-gcm`, and the LRU cache into every crate that transitively imports `roz-nats`.
- An alternative is to put the types in `roz-core::signing` and keep helper surfaces in `roz-nats::signing`. Planner picks the axis; the factoring matters less than keeping the crypto code in one place with one `SignatureError`.

### F2. Sign/verify helpers must be **synchronous** and infallibly-fast

`ed25519_dalek::Signer::sign(&msg)` and `Verifier::verify(&msg, &sig)` are pure CPU operations (~30 µs verify). The existing firmware-verify helper (`crates/roz-core/src/device_trust/verify.rs:35`) is a synchronous `fn verify_firmware_signature(...) -> bool`. The new `sign_outbound` / `verify_inbound` paths should also be synchronous:

```rust
// In roz-sig (illustrative — exact shape is planner's call)
pub fn sign_envelope(
    fields: &SignedFields,         // already filled by caller
    private_key: &SigningKey,      // ed25519_dalek::SigningKey
) -> Result<[u8; 64], SignatureError> { ... }

pub fn verify_envelope(
    fields: &SignedFields,
    signature: &[u8; 64],
    public_key: &VerifyingKey,     // ed25519_dalek::VerifyingKey
) -> Result<(), SignatureError> { ... }
```

The **async surface** should live at a layer up — the key loader (reads + decrypts the on-disk key), the DB public-key lookup, and the LRU cache. Those are async. The crypto primitive must not be `async` because nothing inside it needs to await.

Signing throughput budget: worker signs every result/telemetry/event/trust-report. Telemetry can be >10 Hz. Keep the hot path allocation-free: reuse a pre-allocated `Vec<u8>` buffer for the JCS output; `ed25519_dalek::Signer::sign` does not allocate.

### F3. JCS + Ed25519 round-trip structure — two serialization stages, not one

DEEP-SIGN.md §2 specifies "JCS-canonical serialization," but does not clearly distinguish the two distinct serialization steps the signer must perform. Spelling it out:

1. **Payload hashing step:** The **original NATS payload bytes** (i.e., `serde_json::to_vec(&TaskInvocation)` or whatever struct) are hashed with SHA-256. This hash is `payload_hash` in D-03. The payload is NOT re-serialized through JCS — the receiver reads bytes-as-received, hashes, compares. This mandates that the **payload serializer is deterministic on this hop** — serde_json is deterministic enough for the byte-for-byte compare as long as signer and verifier use the same compiled binary, but to be safe, the signer MUST attach the hash computed over the exact bytes it will publish. Do not re-serialize on verify.

2. **Envelope JCS step:** The `SignedFields` struct (8 fields from D-03) is serialized via `serde_json_canonicalizer` (or whichever JCS crate is chosen). The JCS output is what gets hashed (SHA-512 internally by Ed25519) and signed. This is where determinism matters, because signer and verifier may be different binaries / Rust versions.

**Unit test strategy:** golden-vector tests on both stages. Hardcode a `SignedFields` struct with known UUIDs + timestamps, compute JCS, assert the canonical byte output matches a stored fixture. Then sign with a deterministic test keypair, assert the signature hex matches a stored fixture. This catches silent behavioral changes in the JCS library on upgrade.

### F4. LRU cache — `moka` 0.12 is the right choice

Requirements: (a) keyed by `(Uuid, Uuid, u32)` composite, (b) TTL per entry (60 s), (c) synchronous invalidate, (d) async-friendly (caller is an axum handler in Tokio). Alternatives:

| Crate | Async? | TTL per-entry? | In workspace? | Notes |
|-------|--------|----------------|---------------|-------|
| `lru` | no | no (LRU-only) | no | too low-level; would require a wrapper |
| `moka` 0.12 | yes (`future::Cache`) | yes | indirect via `restate-sdk` | best fit |
| `quick_cache` | yes | yes | no | smaller, but less battle-tested |
| `cached` | partial | yes | no | macro-heavy |

Recommendation: pin `moka = "0.12"` with features `future`. Cache entry type: `VerifyingKey` (32 bytes). Size budget: even 10 000 entries is ~320 kB. Default max capacity 10 000 is plenty for a single tenant's fleet.

### F5. Error type design — one `SignatureError`, surface as `From` into each caller's domain error

Follow the pattern of `roz_server::routes::task_dispatch::TaskDispatchError` (see `crates/roz-server/src/routes/task_dispatch.rs:32-76`): a per-boundary error enum with explicit mappings to `AppError` and `tonic::Status`. The new `SignatureError` lives in `roz-sig`, with variants for:

- `InvalidKey` (bad public-key bytes, bad private-key decryption)
- `InvalidSignature` (Ed25519 verify returned Err)
- `ReplayRejected { reason: ReplayReason }` where ReplayReason is `SequenceTooLow | TimestampSkew | KeyVersionUnknown`
- `Canonicalization(#[from] serde_json_canonicalizer::Error)`
- `KeyNotConfigured` (worker-side only; trigger for re-enrollment)
- `Revoked` (public-key row has `revoked_at IS NOT NULL`)

Then at each boundary:

- `roz-server::routes` — `impl From<SignatureError> for AppError` → `AppError::unauthorized("signature verification failed: {reason}")` for audit-safe user-facing message; detailed variant in tracing fields only.
- `roz-worker::main` — on outbound-sign failure → log + hard-stop (per D-09); on inbound-verify failure → log + drop message + increment a worker-local metric.
- `roz-server::grpc` — `impl From<SignatureError> for tonic::Status::unauthenticated(...)`

Do not flatten `SignatureError` into `AppError` or `AgentError` directly — the caller-specific context (which direction, which host) belongs in the log/audit event, not in the error type.

### F6. Worker-side sequence counter — one SQLite row, atomic UPDATE, not WAL entries

D-04 mandates a per-`(direction, host_id, tenant_id, key_version)` monotonic counter on the worker. The existing `crates/roz-worker/src/wal.rs` has three tables — `wal_entries`, `worker_state`, `idempotency_cache`. The counter should NOT go in `wal_entries` (those are task-scoped + acked). It goes in `worker_state` as a small KV, OR in a new dedicated table `signing_sequence_counter (key_version INT PRIMARY KEY, seq INTEGER NOT NULL)`.

Recommendation: new dedicated table (cleaner + queryable). Write path:

```sql
INSERT INTO signing_sequence_counter (key_version, seq) VALUES (?1, 1)
  ON CONFLICT(key_version) DO UPDATE SET seq = seq + 1
  RETURNING seq;
```

Rusqlite supports `RETURNING` as of 0.32 (the pinned version). The returned value is the sequence number to embed in the envelope. This is atomic under SQLite's default write-locking, so concurrent result/telemetry publishers in the same worker process will not produce gap-free sequences accidentally.

Counter reset on rotation (D-04): when a new key is installed, insert a new row with `key_version = N+1, seq = 0`. Old key's row stays for the 24 h overlap; the signer picks key based on the active key's `key_version`.

**Open pitfall:** if the worker crashes between signing and publishing, the counter has advanced but the envelope never went out. Server sees a gap — which is NOT a replay, since gaps are allowed ("seq must be > last seen"). Documented; no action needed. But the worker's telemetry debugging will show occasional gaps — planner should note this in an operator runbook.

### F7. Two endpoints beat one discriminator for bootstrap + rotation

CONTEXT.md Claude's-Discretion asks whether to split `provision-key` and `rotate-key`. The answer is to keep them separate:

- `POST /v1/device/provision-key` uses **API-key bearer auth** (per-host `ROZ_API_KEY`). Rate-limited to 1 success/hour.
- `POST /v1/device/rotate-key` uses **current-device-key-signed** auth (the worker signs the request body with its current Ed25519 private key; the server verifies using the active public key from `roz_device_keys`).

A single endpoint with a `mode` discriminator would need dual-mode auth middleware and a branch on "is this bearer-token or signed-body?" — more code, no benefit, and harder for auditors to reason about. Separate endpoints with separate auth layers mirror the existing `auth_keys.rs` + `device_auth.rs` split.

### F8. Sim/CI workers: ephemeral keys in `${ROZ_DATA_DIR}`, no repo commits

D-10 says the provisioning path is the same for sim/CI as for production. Implication for the planner:

- `${ROZ_DATA_DIR}` must be settable in the container / CI job config. Default falls back to `~/.config/roz/device-key.pem` on dev.
- `.gitignore` must already cover these paths (it does — `.roz/` is gitignored; confirm `/etc/roz/` and `${ROZ_DATA_DIR}` containers don't leak into the repo on accident).
- Integration tests in `crates/roz-test` (see `crates/roz-test/src/nats.rs`, `crates/roz-test/src/pg.rs`) should spawn a roz-server + a real worker + run the full `provision-key` enrollment against testcontainers Postgres + NATS. `.planning/research/DEEP-SIGN.md §6 FS-04.2` calls for exactly this.
- CI gate: every signed-dispatch test must exercise the real enrollment, not stub it. Matches the team's "test the production path" principle from `CLAUDE.md`.

### F9. Signing state is per-host, not per-tenant — plan DB indices accordingly

`roz_device_keys.host_id` is the lookup key on the server-verify hot path. Existing index from DEEP-SIGN.md §4:

```sql
CREATE INDEX idx_device_keys_active ON roz_device_keys(host_id)
    WHERE revoked_at IS NULL AND rotated_at IS NULL;
```

This is a partial index; it works for "active key lookup". The rotation 24 h overlap means the index condition must admit **both** overlapping rows during transition. Consider relaxing to `WHERE revoked_at IS NULL` only (drop the `rotated_at IS NULL` clause), and let the verifier pick by `key_version` from the envelope. Otherwise, the old row falls out of the index immediately on rotation, and the 24 h overlap silently doesn't work.

### F10. Trust-posture gate runs **before** the signing gate, not after

Per D-05-trust-posture in DEEP-SIGN.md §5: `Untrusted` hosts are blocked **before** the signing stage. Existing `crates/roz-server/src/routes/task_dispatch.rs:126-136` already calls `crate::trust::check_host_trust(...)` and returns `TaskDispatchError::TrustRejected` on failure. The signing call (`sign_outbound`) goes AFTER this check — if trust was rejected, there is no message to sign. Conversely, on the worker side receiving a task, the worker verifies the envelope BEFORE running the trust posture check — because the verification gate is what proves the dispatch came from the server at all. Symmetric inverse on each hop.

## File-Level Impact Map

| Decision | Files to create / modify | Action |
|----------|-------------------------|--------|
| **D-01** envelope in NATS header | **new:** `crates/roz-sig/src/envelope.rs` (or `crates/roz-nats/src/signing/envelope.rs`) | Define `SignedFields`, `SignatureEnvelope`, header constants (`HEADER_NAME = "roz-sig-v1"`). Encode/decode helpers for the base64 header value. |
| **D-01** publish path attaches header | `crates/roz-server/src/routes/task_dispatch.rs:214` (current `nats.publish(subject, payload.into())`) | Replace with a call to `roz_sig::publish_signed(nats, subject, payload, signed_fields, &server_signing_key)` that builds the header and calls `nats.publish_with_headers(...)`. |
| **D-01** subscribe path reads header | `crates/roz-worker/src/main.rs:1332-1346` (loop over `sub.next()`) | Replace `serde_json::from_slice(&msg.payload)` with a two-step: `roz_sig::verify_inbound(&msg.headers, &msg.payload, &cached_server_verifying_key)` then deserialize. Fail-closed: on verify error, log + emit failure event, continue. |
| **D-02** JCS crate | `Cargo.toml` workspace deps | Add `serde_json_canonicalizer = "0.3"` to workspace; reference in `crates/roz-sig/Cargo.toml`. CONTEXT.md says `jcs = "0.1"` — this crate does not exist (see §Dependency diff). |
| **D-03** signed fields | `crates/roz-sig/src/envelope.rs` | Define `SignedFields` struct with 8 fields. `Direction` is an enum with two snake_case serde values. Serialize via JCS before hashing. |
| **D-04** replay protection (server side) | **new:** `crates/roz-db/src/device_keys.rs` (DB module), migration `migrations/021_device_keys.sql` | Atomic `UPDATE ... SET sequence_number_offset = $1 WHERE sequence_number_offset < $1 RETURNING id` pattern; reject if no row returned. |
| **D-04** replay protection (worker side) | `crates/roz-worker/src/wal.rs` (add table + helper) | New `signing_sequence_counter` table. `next_seq(key_version) -> u64` via `INSERT ... ON CONFLICT ... RETURNING seq`. See F6 for the exact SQL. |
| **D-04** timestamp skew | `crates/roz-sig/src/verify.rs` | Skew constant `const TIMESTAMP_SKEW_SECS: i64 = 5;`. `chrono::Utc::now()` minus envelope timestamp, abs > 5 → reject. |
| **D-05** key storage (worker) | **new:** `crates/roz-worker/src/signing_key.rs` | Load/save private key at `/etc/roz/device-key.pem` (prod) or `${ROZ_DATA_DIR}/device-key.pem` (dev). Uses existing `roz_core::key_provider::StaticKeyProvider` for AES-GCM encryption. File mode 0600 enforced on write. |
| **D-05** key-at-rest format | `crates/roz-worker/src/signing_key.rs` | Store as JSON: `{ "ciphertext_b64": ..., "nonce_b64": ..., "key_version": 1 }`. Private-key bytes are 32 bytes (Ed25519 seed, not the full 64-byte expanded key) — matches `SigningKey::from_bytes(&[u8; 32])`. |
| **D-06** bootstrap HTTP route (server) | **new:** `crates/roz-server/src/routes/device_keys.rs` | `POST /v1/device/provision-key` handler. Reads bearer token, validates host via existing API-key-auth middleware (`auth_keys.rs` pattern), generates keypair via `SigningKey::generate(&mut OsRng)`, inserts row with `key_version = 1` via `roz_db::device_keys::insert`, returns private-key bytes base64-encoded (one-time). |
| **D-06** bootstrap client (worker) | `crates/roz-worker/src/registration.rs` (extend) | After `register_host()` returns the host UUID, if no local device key exists, call `provision_device_key(client, api_url, api_key, host_id)` → persist returned private key. Happens once, in startup, before NATS subscription begins. |
| **D-06** rate limit | `crates/roz-server/src/middleware/rate_limit.rs` (existing; add entry) | Scope key `"device-provision:{host_id}"`, 1/hour. |
| **D-07** rotation client (worker) | `crates/roz-worker/src/main.rs` (heartbeat loop, around line 1300-ish — existing periodic tasks) | Every 24 h, check `key.created_at`; if > 90 d, sign a `POST /v1/device/rotate-key` with current key, swap the on-disk private key. Both keys are retained for the 24 h overlap. |
| **D-07** rotation HTTP route (server) | `crates/roz-server/src/routes/device_keys.rs` (same module as bootstrap) | `POST /v1/device/rotate-key` — signed-body auth middleware verifies against active key; inserts new row with `key_version = N+1`; marks old row with `rotated_at = now()` but leaves it active for 24 h. |
| **D-07** CLI override | `crates/roz-cli/src/commands/` (new `device.rs`) | `roz device rotate-key` — same call as the auto-rotate, but unconditional. |
| **D-08** revocation (server) | `crates/roz-db/src/device_keys.rs` + `crates/roz-server/src/routes/device_keys.rs` | `set_revoked_at(host_id, key_version)`. Synchronously invalidate the LRU cache entry via `cache.invalidate(&(tenant_id, host_id, key_version))`. |
| **D-09** signing-failure subject | `crates/roz-nats/src/subjects.rs` (extend) | Add `safety_signature_failure(host_id)` → `safety.signature_failure.{host_id}` and, per REQUIREMENTS.md, `safety_signature_failure_server(tenant_id)` → `safety.signature_failure.server.{tenant_id}`. Both must exist. Include tests like the existing `estop_subject` + `wasm_trust_failure_subject` patterns (subjects.rs:273-306). |
| **D-09** audit-log row | `crates/roz-db/src/safety_audit.rs` (existing module for `roz_safety_audit_log`) OR `roz_audit_events` if scope demands | One row per failed verification; fields include `host_id, tenant_id, direction, reason, key_version, received_at`. DEEP-SIGN.md §6 FS-04.5 says "existing `roz_safety_audit_log` (or new `roz_audit_events` if scope demands)" — planner picks. |
| **D-10** sim/CI path | `crates/roz-test/src/` — new `device_key.rs` helper + extend integration tests in `crates/roz-server/tests/` | `provision_test_keypair(server_url, api_key, host_id) -> SigningKey`. Spun up fresh per test. Roundtrip test: sign a known payload, verify it server-side. |
| **D-11** LRU cache | `crates/roz-server/src/state.rs` (AppState gets a `moka::future::Cache<(Uuid, Uuid, u32), VerifyingKey>` field) | Initialized in `main.rs`. 60 s TTL, 10 000 entries max. Invalidation API wired into `roz-db::device_keys::set_revoked_at`. |
| **D-12** rollout gate | `crates/roz-server/src/config.rs` — add `signed_dispatch_enforcement: SignedDispatchEnforcement` | Enum with `Off | Audit | Strict`. Read from `SIGNED_DISPATCH_ENFORCEMENT` env var via the existing figment loader. Wired into the verify gate: on missing/invalid sig, `Off` logs + passes, `Audit` logs but does not reject, `Strict` rejects. Default `Strict`. |
| **D-12** enforcement gate callsite | `crates/roz-server/src/routes/task_dispatch.rs` (server sending) + server-side NATS consumers of worker-published traffic (identify during planning) | Check `enforcement` before the sign/verify call; branch behavior. |

### New crates / files summary

- **new crate:** `crates/roz-sig/` (preferred) OR new module `crates/roz-nats/src/signing/`
- **new migration:** `migrations/021_device_keys.sql` (DDL in DEEP-SIGN.md §4, verify column names match)
- **new routes:** `crates/roz-server/src/routes/device_keys.rs`
- **new DB module:** `crates/roz-db/src/device_keys.rs`
- **new CLI command:** `crates/roz-cli/src/commands/device.rs` (only `rotate-key` for now)
- **new worker module:** `crates/roz-worker/src/signing_key.rs` (loader/saver + provision client)

### Files modified in place

- `crates/roz-nats/src/subjects.rs` — add failure subjects
- `crates/roz-server/src/routes/task_dispatch.rs` — sign before publish
- `crates/roz-server/src/routes/mod.rs` — wire new route module
- `crates/roz-server/src/state.rs` — add LRU cache + signing key
- `crates/roz-server/src/config.rs` — add enforcement enum
- `crates/roz-server/src/main.rs` — initialize cache + load server signing key
- `crates/roz-worker/src/main.rs` — verify before deserialize (lines 1332–1352), sign before result publish
- `crates/roz-worker/src/registration.rs` — add provision-key call after host registration
- `crates/roz-worker/src/wal.rs` — new `signing_sequence_counter` table + `next_seq`
- `crates/roz-worker/src/dispatch.rs` or wherever `signal_result` publishes result back to Restate (line 385) — must also be signed per FS-04 "worker → server: task results"
- `crates/roz-worker/src/telemetry.rs` — every published telemetry must be signed
- `crates/roz-worker/src/event_nats.rs` — every event published must be signed
- `crates/roz-worker/src/trust.rs` — trust-reports published must be signed
- `Cargo.toml` — workspace dep add (`serde_json_canonicalizer`, `moka`)

## Integration-Point Pitfalls

### `crates/roz-server/src/routes/task_dispatch.rs` (dispatch hook)

Current state: line 214 — `nats.publish(subject, payload.into()).await`. Bare publish, no headers.

Pitfalls for the planner:

1. **Transaction ordering.** The dispatch function mutates the DB (`tasks::create`, `tasks::assign_host`, `update_status`) and starts a Restate workflow BEFORE the NATS publish. If the sign step fails, the signing error must **not** leave the task in a corrupted state — recommend inserting `update_status(task.id, "failed")` on the signing-failure path just like the existing publish-failure path does (line 215).
2. **Signing gate vs. trust gate order.** Trust gate at line 126 runs first — correct per F10. Don't reorder.
3. **`sequence_number` source on server side.** Server holds a single monotonic counter per `(direction=server→worker, host_id, tenant_id, key_version)`. Where does it live? Options: (a) same `roz_device_keys` table, a `last_sent_sequence` column on the **server's own signing key row**; (b) separate `roz_server_signing_state` table. DEEP-SIGN.md §4 schema has a `sequence_number_offset` on the device-keys row — but that's for verifying RECEIVED messages from workers, not sending. Planner must add either a parallel column or a parallel table. Flagged in §Open Questions as Q2.
4. **Restate workflow URL forwarding.** The `restate_url` is in the `TaskInvocation` payload; the sign hash covers the full payload, so changing `restate_url` post-sign is not possible. Fine — `restate_url` is set before serialization (line 204). No action.

### `crates/roz-nats` (new signing module or `roz-sig` crate)

Pitfalls:

1. **Header encoding discipline.** `roz-sig-v1` header MUST be ASCII-only. CONTEXT.md D-01 says "base64 of envelope fields + raw 64-byte signature". Use `base64::engine::general_purpose::STANDARD_NO_PAD` or `URL_SAFE_NO_PAD` — the code already imports `URL_SAFE_NO_PAD` (device_auth.rs:5). Pick one and stick with it.
2. **`async-nats::HeaderMap` API.** Header values are `HeaderValue`; build via `HeaderValue::from_str(&b64_string)`. The value must be valid UTF-8; base64 output is always ASCII so this is safe. But beware: `async_nats::Message::headers` is `Option<HeaderMap>` — a message with no headers is possible (legacy unsigned traffic during D-12 rollout). Verifier must handle `None` per the enforcement mode.
3. **JCS serde struct field order.** JCS sorts keys lexicographically after serialization, so struct-field declaration order does NOT matter for correctness — but canonicalization deterministically sorts by Unicode code point. Use flat `#[derive(Serialize)]` with primitive / string fields; don't nest enums with non-default tagging. Specifically: `Direction` should serialize as a bare string (`"server_to_worker"` / `"worker_to_server"` — snake-case through `#[serde(rename_all = "snake_case")]`), not as a tagged enum — see DeviceTrustPosture in `crates/roz-core/src/device_trust/mod.rs:13-19` for the proven pattern.
4. **Clock source.** `chrono::Utc::now()` on both ends. Microsecond precision requires `%Y-%m-%dT%H:%M:%S%.6fZ` format. `chrono`'s default `to_rfc3339()` gives nanoseconds, which round-trips. Fine.

### `crates/roz-worker/src/main.rs` (subscribe / verify)

Pitfalls:

1. **Verify-before-deserialize ordering.** Current line 1346 — `serde_json::from_slice(&msg.payload)` — allocates + parses before any auth check. In the new design, `roz_sig::verify_inbound` MUST run first (on the raw bytes + header). If it fails, drop the message, do NOT attempt to parse. Matches fail-closed doctrine.
2. **E-stop precedence.** Line 1335 (`if *estop_rx.borrow()`) is also before deserialize today. Keep it — e-stop should short-circuit BEFORE signature verify, because an e-stopped worker must not do crypto work that wastes cycles. Order: estop → verify → deserialize.
3. **Server verifying-key discovery.** The worker needs the server's **public** key (for verifying inbound dispatch). DEEP-SIGN.md doesn't explicitly specify delivery — implicit in "Worker caches the server's verifying key locally after enrollment" (REQUIREMENTS.md §FS-04 line 26). Options: (a) bake public key into worker config (`ROZ_SERVER_VERIFYING_KEY` env var); (b) include it in the `provision-key` response body. (b) is simpler — single delivery moment, survives rotation by re-enrolling. Flagged as Q3.
4. **Signed outbound paths.** `main.rs` publishes result, telemetry, status. Line 1394–1399 builds a `TaskResult`; `signal_result` POSTs to Restate (HTTP, not NATS — does it need signing too?). REQUIREMENTS.md §FS-04 says "Worker → server: task results" is signed. If `signal_result` is HTTP-to-Restate, the signing wrapper must also apply — but this is a different carriage (HTTP body header, not NATS header). Planner must decide: is this in scope, or is only NATS-carried traffic signed? Flagged as Q1.
5. **Concurrent publishers.** The per-task spawn (line 1366+) means multiple Tokio tasks may sign concurrently. `SigningKey` is `Clone + Send`; the SQLite sequence-counter UPDATE is atomic; no lock needed as long as each publish does its own RETURNING.

### `crates/roz-worker/src/wal.rs` (counter persistence)

Pitfalls:

1. **Don't mix counter with `wal_entries` or `worker_state`.** New dedicated table. See F6.
2. **SQLite connection pooling.** `WalStore::open` currently creates one `Connection`. Concurrent publishers will contend on writes. SQLite WAL-mode serializes writes but allows concurrent reads — the `next_seq` path is always a write. Fine up to ~1 kHz; at higher rates, consider `r2d2_sqlite` or a mpsc channel fronting one writer. Document as known limitation.
3. **`signing_sequence_counter` rows never deleted.** On rotation, old key's row stays. After many rotations, the table grows slowly (8 bytes/row). Acceptable; ignore.
4. **Migration on worker startup.** `WalStore::open` uses `CREATE TABLE IF NOT EXISTS`. Add the new counter table in the same batch (line 25–44). New deployments get it; upgrades get it on first run. No data migration needed (counter starts at 0 for every `key_version`).

## Test Fixture Strategy

Tests should exist at three scopes. DEEP-SIGN.md §6 gives the acceptance-test matrix; this section breaks it into Rust fixtures.

### Unit fixtures in `crates/roz-sig` (or `crates/roz-nats/src/signing`)

- **`test_keypair_pair() -> (SigningKey, VerifyingKey)`** — deterministic via `SigningKey::from_bytes(&[0u8; 32])` (or `[7u8; 32]` like `StaticKeyProvider::provider()` in key_provider.rs:214). NEVER use `OsRng` in tests — determinism makes golden-vector tests possible.
- **`sample_fields(direction, seq) -> SignedFields`** — fills every D-03 field with fixed UUIDs and a fixed timestamp (`"2026-04-17T12:00:00.000000Z"`).
- **Golden JCS vector:** `let canonical = jcs(&sample_fields)` → assert against a hex-encoded fixture byte string. Checks library upgrades don't silently change output.
- **Golden signature vector:** `let sig = sign(fields, test_keypair.0)` → assert equal to a known hex. Guards against algorithm library changes.
- **Round-trip:** sign + verify with a mutated field → Err; sign + verify original → Ok.
- **Replay tests:** seq N+1 after seq N → Ok; seq N after seq N+1 → `ReplayRejected(SequenceTooLow)`; timestamp + 10 s → `ReplayRejected(TimestampSkew)`; timestamp − 10 s → same.
- **Tampered payload:** build envelope, flip one byte of payload, assert `verify` → `InvalidSignature`.
- **Tampered signature:** flip one byte of the 64-byte sig → `InvalidSignature`.
- **Tampered header:** flip one byte of the base64 header → base64 decode error OR `InvalidSignature`.
- **Wrong key version:** envelope signed with key v1, verifier looks up v2 public key → `InvalidKey` or `InvalidSignature`.

### Integration fixtures in `crates/roz-test/src/device_key.rs` (new) + `crates/roz-server/tests/`

- **`bootstrap_and_sign_fixture`** — spin up Postgres + NATS via testcontainers (existing pattern in `crates/roz-test/src/pg.rs` / `nats.rs`), start a `roz-server` binary, POST `/v1/device/provision-key` with a test `ROZ_API_KEY`, get back a keypair, assert a row exists in `roz_device_keys` with `key_version=1`.
- **`rotate_key_fixture`** — after bootstrap, POST `/v1/device/rotate-key` signed with the current key, assert new row with `key_version=2, rotated_at IS NULL`, old row updated to `rotated_at IS NOT NULL`. Then sign a message with the old key within 24 h → still accepted; fast-forward (inject `chrono::Utc::now()` via a test clock) past 24 h → old key rejected.
- **`revoke_fixture`** — set `revoked_at` on an active key; next message with that key → `SignatureError::Revoked` + audit row written + NATS event on `safety.signature_failure.{host_id}`.
- **`roundtrip_dispatch_e2e`** — POST `/v1/tasks` on server, assert a signed NATS message lands on `invoke.{worker}.{task}`, a stub worker verifies it successfully, and then publishes a signed result back, and server accepts + transitions the task state.
- **`replay_rejection_e2e`** — capture a signed NATS message, re-publish it verbatim → verification rejects (seq replay).
- **`enforcement_audit_mode`** — set `SIGNED_DISPATCH_ENFORCEMENT=audit`, send an unsigned message; the server accepts but emits a warning + audit row. With `strict`, same test → rejection.

### Worker-side harness (`crates/roz-worker/tests/`)

- **`worker_signs_every_published_subject`** — start worker, capture published NATS messages on `telemetry.*`, `events.*`, `safety.*`, `session.*`; assert every message has the `roz-sig-v1` header.
- **`worker_hardstops_on_missing_key`** — start worker with no on-disk device key, no `ROZ_API_KEY` → startup fails with clear error. Start worker with corrupted key file → same fail-closed.
- **`worker_recovers_after_rotation`** — start worker, force rotation, assert subsequent messages signed with new key version.

## Dependency Diff

Exact crate additions and modifications to `Cargo.toml` workspace dependencies:

| Crate | Current | Target | Verification |
|-------|---------|--------|--------------|
| `ed25519-dalek` | `"2"` (features `rand_core`) — already present | keep `"2"`; actual max stable is **2.2.0** as of 2026-04-17 | `curl https://crates.io/api/v1/crates/ed25519-dalek` returns `max_stable_version: 2.2.0`. [VERIFIED: crates.io registry query 2026-04-17] |
| `aes-gcm` | `"0.10"` — already present | keep | [VERIFIED: already used by key_provider.rs] |
| `sha2` | `"0.10"` — already present | keep | [VERIFIED] |
| `base64` | `"0.22"` — already present | keep | [VERIFIED] |
| `rusqlite` | `"0.32"` (features `bundled`) — already present | keep; `RETURNING` clause works at 0.32 | [VERIFIED: rusqlite 0.32 adds `Connection::query_row` + supports SQLite's RETURNING (SQLite 3.35+, SQLite 3.44 is bundled)] |
| `chrono` | `"0.4"` (features `serde`) — already present | keep | [VERIFIED] |
| **`serde_json_canonicalizer`** | NOT present | **ADD: `"0.3"` (0.3.2 is current)** | CONTEXT.md says `jcs = "0.1"` — **this crate does not exist on crates.io** (returns "crate does not exist"). The real, actively maintained RFC 8785 implementation is `serde_json_canonicalizer = "0.3"`, last updated 2026-02-03. There is also `serde_jcs = "0.2"` but upstream notes it is less actively maintained. [VERIFIED: crates.io registry 2026-04-17] |
| **`moka`** | NOT a direct workspace dep (indirect via `restate-sdk`) | **ADD: `"0.12"` (feature `future`)** | For the server-side verifying-key LRU. Async-friendly. [VERIFIED: widely-used crate; `future::Cache` API is stable.] |
| **`getrandom`** | transitively present via `ed25519-dalek` | no action | [VERIFIED] |

Additional crate-specific manifest changes:

- **`crates/roz-sig/Cargo.toml`** (new): depends on `roz-core`, `ed25519-dalek`, `sha2`, `base64`, `serde`, `serde_json`, `serde_json_canonicalizer`, `chrono`, `uuid`, `thiserror`, `async-nats` (for header type), `tracing`.
- **`crates/roz-server/Cargo.toml`**: add `roz-sig`, `moka = { workspace = true, features = ["future"] }`.
- **`crates/roz-worker/Cargo.toml`**: add `roz-sig`.
- **`crates/roz-db/Cargo.toml`**: add `roz-sig` (only for envelope types used in `device_keys.rs` return types — may not need, depending on planner split).
- **`crates/roz-cli/Cargo.toml`**: add `roz-sig` for the `rotate-key` command.

### Verifying dep versions before writing the plan

Planner should run:

```bash
cargo update -p serde_json_canonicalizer --dry-run
cargo update -p moka --dry-run
cargo tree -p roz-core -e normal | head -20   # confirm no AES-GCM conflicts
```

If CI is red on a dep version mismatch after this phase, the first thing to check is whether `serde_json_canonicalizer` pulls in a `serde_json` minor bump that conflicts with the currently-resolved version (unlikely — both crates depend on `serde_json = "1"` with wide ranges).

## Open Questions for Planner

### Q1. Is HTTP-to-Restate signing in scope for Phase 23?

**Context:** REQUIREMENTS.md §FS-04 line 27 says "Worker → server (task results, telemetry, session events): worker signs…". Task results currently flow **via HTTP to Restate**, not via NATS (see `crates/roz-worker/src/dispatch.rs:signal_result` — POSTs JSON to `/TaskWorkflow/{task_id}/deliver_result/send`). Restate then runs the workflow handler on the server.

**Option A — scope HTTP too:** Every Restate result POST carries a `roz-sig-v1` HTTP header; the server's Restate workflow handler verifies. Requires injecting signing/verifying into the Restate handler at `crates/roz-server/src/restate/task_workflow.rs`.

**Option B — limit to NATS:** Task results that flow NATS-native (on `tasks.{task_id}.status` or similar) are signed; the HTTP-to-Restate path is **trusted** because Restate is on the server side of the trust boundary. In-flight task results that do not cross the server-worker NATS boundary are never unsigned-over-the-wire in a way that crosses tenant boundaries.

**Recommendation:** Option B. The HTTP-to-Restate call is same-cluster, egress-controlled. The stated requirement "every NATS hop" (CONTEXT.md Phase Boundary) does not cover HTTP. But Option B needs explicit confirmation — because FS-04 line 27 says "task results" without qualifier. Decide in discuss-phase step before breakdown.

### Q2. Where does the server's own `sequence_number` counter live?

**Context:** See Integration-Point Pitfalls §`task_dispatch.rs` item 3. The `roz_device_keys` table has `sequence_number_offset` — but that's for verifying incoming worker messages. The server also needs its own per-(direction=server→worker, host_id, tenant_id, key_version) counter for the messages it sends.

**Option A — add `server_sent_seq` column to `roz_device_keys`:** one table, two counters. Simple but denormalized (server→worker seq is not really a property of the device's key).

**Option B — new `roz_server_signing_state` table:** clean separation; one row per `(tenant_id, host_id, key_version)` for server-outbound; parallel to the worker's SQLite table.

**Option C — use the target host's row `sequence_number_offset` but bump it on publish:** collapses the two counters; requires the verifier to cross-check direction. Fragile — recommended against.

**Recommendation:** Option B. Matches "single-writer, per-direction" principle. Adds one table to migration `021`.

### Q3. How does the worker receive the server's verifying public key?

**Context:** Worker needs to verify inbound dispatch. The server has its own signing key (separate from any device key). Where does the worker get the matching public key?

**Option A — env var:** `ROZ_SERVER_VERIFYING_KEY` (base64). Baked into worker config. Rotation = redeploy.

**Option B — returned from `provision-key` response body:** `{ private_key, server_verifying_key }`. Worker caches. Rotation = trigger by the operator via `POST /v1/device/provision-key` again (supported by D-06 anyway for re-enrollment).

**Option C — new endpoint `GET /v1/server/verifying-key`:** called at worker startup, cached. Rotation-friendly.

**Recommendation:** Option B for v3.0 (simplicity; ties verifying-key delivery to the already-authenticated provisioning handshake), with a known limitation that server-side key rotation requires a worker re-enrollment. If that becomes painful, add Option C in a later hardening phase.

### Minor open items

- **Q4.** DEEP-SIGN.md references `roz_safety_audit_log` for FS-04.5 but says "or new `roz_audit_events` if scope demands." Which? — planner picks in the task breakdown. Recommendation: use existing `roz_safety_audit_log` to avoid scope creep.
- **Q5.** Should the worker's hard-stop on missing key (D-09) produce a distinct process exit code for ops dashboards? Recommendation: exit code 78 (`EX_CONFIG`), matching systemd convention; update container healthcheck if one exists.
- **Q6.** `SIGNED_DISPATCH_ENFORCEMENT` default for dev/sim: `strict` is the stated default for fresh deployments, but the dev loop will hit this constantly. Recommendation: `audit` default for `ROZ_ENVIRONMENT=development` (there's already a check for this env in `crates/roz-server/src/main.rs`), `strict` for production.
- **Q7.** Revocation cache invalidation — the LRU cache is per-server-process. In a multi-replica deployment, revoking a key on replica A does not invalidate replica B's cache for up to 60 s. Acceptable? Or do we need a NATS broadcast on `safety.key_revoked.{host_id}` so every server process invalidates synchronously? For v3.0 single-replica, this is fine. Flag for multi-replica phase.

## Sources

### Primary (HIGH confidence)

- `.planning/research/DEEP-SIGN.md` (in-repo) — normative design doc for FS-04
- `.planning/phases/23-.../23-CONTEXT.md` (in-repo) — locked decisions D-01..D-12
- `.planning/REQUIREMENTS.md` §FS-04 (in-repo) — acceptance criteria
- `.planning/PROJECT.md` (in-repo) — v3.0 design decisions
- `crates/roz-core/src/device_trust/verify.rs` (in-repo) — reference Ed25519 verify pattern
- `crates/roz-core/src/key_provider.rs` (in-repo) — reference AES-GCM encryption pattern
- `crates/roz-worker/src/wal.rs` (in-repo) — SQLite WAL pattern
- `crates/roz-worker/src/registration.rs` (in-repo) — per-host `ROZ_API_KEY` source
- `crates/roz-server/src/routes/task_dispatch.rs` (in-repo) — dispatch hook site
- `crates/roz-server/src/routes/device_auth.rs` (in-repo) — rate-limit + response patterns
- `crates/roz-nats/src/dispatch.rs` (in-repo) — `TaskInvocation` / `TaskResult` wire types
- `crates/roz-nats/src/subjects.rs` (in-repo) — subject builder pattern
- `Cargo.toml` (in-repo) — workspace dep versions as of 2026-04-17

### Secondary (MEDIUM-HIGH confidence)

- crates.io registry query for `serde_json_canonicalizer` (0.3.2, updated 2026-02-03) and `serde_jcs` (0.2.0, updated 2026-03-25), 2026-04-17
- crates.io registry query for `ed25519-dalek` (2.2.0 max stable), 2026-04-17
- [RFC 8785 — JSON Canonicalization Scheme](https://www.rfc-editor.org/rfc/rfc8785) — algorithm reference for JCS determinism
- [serde_json_canonicalizer docs](https://docs.rs/serde_json_canonicalizer/) — the actively-maintained RFC 8785 impl

### Tertiary (lower confidence — flagged for validation)

- `moka` crate recommendation — widely used, but planner should confirm `future::Cache` is still the correct axis (vs `sync::Cache`). API has been stable through 0.12.
- Q-level open items — ecosystem conventions informed, not verified against operator expectations.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | `jcs = "0.1"` in CONTEXT.md is shorthand for "the JCS crate", not a literal crate name. The real crate is `serde_json_canonicalizer`. | Dependency diff | Plan references a non-existent crate; `cargo add` fails immediately. Mitigation: this research explicitly substitutes the real crate name. |
| A2 | `ROZ_KEY_ENCRYPTION_KEY` in CONTEXT.md D-05 refers to the existing `ROZ_ENCRYPTION_KEY` env var (the only AES-GCM master-key env in the repo today). | User Constraints / D-05 | Plan introduces a new env var and breaks the single-key-provider pattern. Mitigation: reconfirm with discuss-phase that `ROZ_ENCRYPTION_KEY` is what's meant. |
| A3 | Rotation requires keeping the 24 h overlap active at the verifier — the DEEP-SIGN.md index (`WHERE rotated_at IS NULL`) is too restrictive; relax to `WHERE revoked_at IS NULL`. | F9 | Rotation silently fails mid-flight (the old key gets excluded from the active-key index the moment rotation happens). Mitigation: this research flags it; planner must correct the index DDL or add a parallel active-key index. |
| A4 | Worker's existing `ROZ_API_KEY` is long-lived per-host and suffices for the one-shot provisioning bootstrap without any additional attestation. | D-06 User Constraints | If the API key is actually session-scoped or rotating, bootstrap breaks on re-enrollment. Mitigation: verified by reading `crates/roz-worker/src/registration.rs` — API key is loaded from config and is stable per-host. |
| A5 | Sim/CI workers can write their private key to `${ROZ_DATA_DIR}` without any additional container-mount magic. | D-10 / F8 | Private key ends up in the wrong path; test hangs on startup. Mitigation: existing tests (`crates/roz-test/src/*`) already use `$CARGO_MANIFEST_DIR`-relative temp dirs; extend that pattern. |
| A6 | Moka `future::Cache` with `time_to_live(Duration::from_secs(60))` and `max_capacity(10_000)` is correct shape for the verifying-key LRU. | F4 | API churn since last workspace integration (`moka` is indirect via `restate-sdk`). Mitigation: planner runs `cargo doc -p moka` and verifies before writing task code. |
| A7 | HTTP-to-Restate task-result delivery (`worker → /TaskWorkflow/.../deliver_result/send`) is out of scope for Phase 23 signing — only NATS hops are signed. | Q1 | If HTTP IS in scope, plan undersizes the work and leaves a major gap in FS-04 coverage. Mitigation: must be resolved in discuss-phase before plan accept. |
| A8 | Every NATS-carried subject the worker publishes (telemetry, events, session, webrtc, camera, safety, capabilities, task status) requires signing — not just task results. | Files modified in place | Plan scope-creeps if every publish site needs a sign wrapper; plan scope-under-cuts if the wrong subset is signed. Mitigation: CONTEXT.md Phase Boundary is explicit about "every NATS hop"; this is a true assumption but the concrete subject enumeration happens during plan. |

---

*Phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning*
*Researched: 2026-04-17*
*Valid until: 2026-05-17 (30 days; no expected near-term churn in `ed25519-dalek` or `serde_json_canonicalizer`)*
