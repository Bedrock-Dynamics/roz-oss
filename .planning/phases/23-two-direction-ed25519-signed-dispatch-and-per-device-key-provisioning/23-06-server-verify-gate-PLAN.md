---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 06
type: execute
wave: 3
autonomous: true
objective: >
  Add the server-side verify gate (verifies worker→server NATS envelopes) and
  the server-side sign hook (signs server→worker dispatch). Verify path uses
  the moka LRU cache + atomic DB advance, writes audit rows, and publishes
  safety.signature_failure.{host_id} on failure. Enforcement branches on
  SIGNED_DISPATCH_ENFORCEMENT (Off/Audit/Strict). Sign path attaches the
  roz-sig-v1 header to every outbound dispatch in crates/roz-nats/src/dispatch.rs.
depends_on:
  - "23-04"
files_modified:
  - crates/roz-server/src/signing_gate.rs
  - crates/roz-server/src/lib.rs
  - crates/roz-nats/src/dispatch.rs
  - crates/roz-nats/src/lib.rs
  - crates/roz-nats/Cargo.toml
  - crates/roz-server/src/routes/tasks.rs
requirements:
  - FS-04
task_count: 3

must_haves:
  truths:
    - "Server signs every outbound server→worker NATS publish with the roz-sig-v1 header (D-01)."
    - "Server verifies every inbound worker→server NATS envelope before deserializing the payload."
    - "Enforcement=Strict: missing/invalid sig → drop + audit + publish safety.signature_failure.{host_id}."
    - "Enforcement=Audit: missing/invalid sig → log warn + audit row; do NOT drop."
    - "Enforcement=Off: log warn only; accept."
    - "Verifying-key cache hit path returns in <100 µs (RESEARCH.md D-11 target); cache miss falls through to DB with synchronous upsert back into cache."
    - "Replay detection is layered: cache high-water-mark check + DB atomic advance_verify_offset (defense in depth)."
  artifacts:
    - path: crates/roz-server/src/signing_gate.rs
      provides: "verify_inbound + sign_outbound orchestration (async, wraps the roz-core::signing primitives with DB/cache access)"
      exports: ["verify_inbound", "sign_outbound", "SigningGateError"]
    - path: crates/roz-nats/src/dispatch.rs
      provides: "publish_signed() wrapper that attaches roz-sig-v1 header"
      contains: "publish_signed"
  key_links:
    - from: crates/roz-server/src/signing_gate.rs
      to: crates/roz-core/src/signing/verify.rs
      via: "calls verify_envelope + check_replay"
      pattern: "roz_core::signing"
    - from: crates/roz-nats/src/dispatch.rs
      to: async_nats::HeaderMap
      via: "publish_with_headers"
      pattern: "publish_with_headers"
    - from: crates/roz-server/src/routes/tasks.rs
      to: crates/roz-nats/src/dispatch.rs
      via: "TaskDispatch::publish now uses publish_signed"
      pattern: "publish_signed"
---

<objective>
Wire the server-side sign path (outbound dispatch gains a `roz-sig-v1` header) and the verify path (every inbound NATS message is checked before its payload is deserialized). The verify path is the enforcement point for replay protection, revocation, and the Off/Audit/Strict rollout gate.

Purpose: Close the server half of the two-direction loop. Without this, Plan 23-07 (worker) would have nothing to verify against on the inbound side and no failure to observe on the outbound side.
Output: New `signing_gate.rs` module in `roz-server`; `publish_signed` helper in `roz-nats::dispatch`; `routes/tasks.rs` updated to call `publish_signed` in place of raw `publish`.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@.planning/research/DEEP-SIGN.md
@crates/roz-server/src/state.rs
@crates/roz-server/src/lib.rs
@crates/roz-server/src/routes/tasks.rs
@crates/roz-nats/src/dispatch.rs
@crates/roz-nats/src/subjects.rs
@crates/roz-core/src/signing/mod.rs
@crates/roz-db/src/device_keys.rs
@crates/roz-db/src/server_signing_state.rs
@crates/roz-db/src/safety_audit.rs

<interfaces>
<!-- Existing NATS publish site (to be wrapped): -->
<!-- crates/roz-server/src/routes/tasks.rs — uses nats_client.publish(subject, payload).await -->

<!-- From 23-04 (AppState): -->
pub struct AppState {
    pub verifying_key_cache: Cache<(Uuid, Uuid, u32), VerifyingKey>,
    pub key_provider: Arc<StaticKeyProvider>,
    pub signed_dispatch_enforcement: SignedDispatchEnforcement,
    // ... plus existing db, nats, etc.
}

<!-- From 23-02 (signing primitives): -->
pub fn sign_envelope(&SignedFields, &SigningKey) -> Result<SignatureEnvelope, _>;
pub fn verify_envelope(&SignedFields, &[u8; 64], &VerifyingKey) -> Result<(), _>;
pub fn check_replay(new_seq, cached_seq, envelope_ts, now) -> Result<(), _>;
pub fn payload_sha256_hex(&[u8]) -> String;

<!-- From 23-03 (DB): -->
device_keys::get_device_key(pool, host_id, key_version) -> Option<Row>;
device_keys::advance_verify_offset(&mut tx, host_id, key_version, new_offset) -> Option<i64>;
server_signing_state::get_active(pool, tenant_id, host_id) -> Option<Row>;
server_signing_state::advance_sequence(pool, tenant_id, host_id, key_version) -> i64;
</interfaces>
</context>

<tasks>

<task type="auto">
  <name>Task 1: Create signing_gate.rs with verify_inbound + sign_outbound (async orchestration)</name>
  <files>crates/roz-server/src/signing_gate.rs, crates/roz-server/src/lib.rs</files>
  <action>
Create `crates/roz-server/src/signing_gate.rs` that orchestrates the async layers around the synchronous `roz-core::signing` primitives. This is the server-side seam between the crypto library and the DB/cache/audit infrastructure.

```rust
//! Server-side sign / verify orchestration for Phase 23 two-direction dispatch.
//!
//! Wraps the synchronous primitives in `roz_core::signing` with async DB access,
//! moka LRU cache lookup, enforcement-mode gating, and audit-row writes.

use async_nats::HeaderMap;
use chrono::Utc;
use ed25519_dalek::{SigningKey, VerifyingKey};
use roz_core::signing::{
    payload_sha256_hex, sign_envelope, verify_envelope, check_replay,
    Direction, ReplayReason, SignatureEnvelope, SignatureError, SignedFields,
    HEADER_NAME,
};
use roz_db::{device_keys, server_signing_state};
use roz_nats::Subjects;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::config::SignedDispatchEnforcement;
use crate::state::AppState;

#[derive(Debug, Error)]
pub enum SigningGateError {
    #[error(transparent)]
    Signature(#[from] SignatureError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("server signing state not initialized for tenant={tenant_id} host={host_id}")]
    ServerStateMissing { tenant_id: Uuid, host_id: Uuid },
    #[error("decrypt server signing seed: {0}")]
    Decrypt(String),
    #[error("missing roz-sig-v1 header")]
    MissingHeader,
}

/// Outbound: build + sign envelope for a server→worker NATS message.
/// Returns the `roz-sig-v1` header value the caller should attach.
pub async fn sign_outbound(
    state: &AppState,
    tenant_id: Uuid,
    host_id: Uuid,
    correlation_id: Uuid,
    payload: &[u8],
) -> Result<String, SigningGateError> {
    let active = server_signing_state::get_active(&state.db, tenant_id, host_id)
        .await?
        .ok_or(SigningGateError::ServerStateMissing { tenant_id, host_id })?;

    // Decrypt server signing seed (AES-256-GCM).
    let nonce_12: [u8; 12] = active.signing_key_nonce.as_slice().try_into()
        .map_err(|_| SigningGateError::Decrypt("nonce length".into()))?;
    let seed_bytes = state.key_provider
        .decrypt(tenant_id, &active.signing_key_bytes_encrypted, &nonce_12)
        .await
        .map_err(|e| SigningGateError::Decrypt(e.to_string()))?;
    let seed: [u8; 32] = seed_bytes.as_slice().try_into()
        .map_err(|_| SigningGateError::Decrypt("seed length".into()))?;
    let signing_key = SigningKey::from_bytes(&seed);

    // Atomic monotonic sequence bump.
    let seq = server_signing_state::advance_sequence(
        &state.db, tenant_id, host_id, active.key_version
    ).await?;

    let fields = SignedFields {
        direction: Direction::ServerToWorker,
        tenant_id,
        host_id,
        correlation_id,
        timestamp: Utc::now(),
        sequence_number: seq as u64,
        payload_hash: payload_sha256_hex(payload),
        key_version: active.key_version as u32,
    };

    let envelope = sign_envelope(&fields, &signing_key)?;
    Ok(envelope.encode_header()?)
}

/// Inbound: verify a worker→server NATS envelope. Returns `Ok(())` if
/// verification passed OR enforcement mode allowed it through.
///
/// Failure side-effects (on reject or audit):
///  - tracing::error! with structured fields
///  - roz_safety_audit_log row inserted (reuse existing table per Q4)
///  - safety.signature_failure.{host_id} published on NATS
pub async fn verify_inbound(
    state: &AppState,
    headers: Option<&HeaderMap>,
    payload: &[u8],
) -> Result<(), SigningGateError> {
    let header_value = headers
        .and_then(|h| h.get(HEADER_NAME))
        .and_then(|v| v.iter().next().map(|s| s.as_str().to_owned()));

    match (header_value, state.signed_dispatch_enforcement) {
        (None, SignedDispatchEnforcement::Off) => {
            tracing::warn!("inbound NATS msg unsigned; enforcement=Off, accepting");
            return Ok(());
        }
        (None, SignedDispatchEnforcement::Audit) => {
            tracing::warn!("inbound NATS msg unsigned; enforcement=Audit, accepting");
            write_audit_and_publish_failure(state, None, "missing_header").await;
            return Ok(());
        }
        (None, SignedDispatchEnforcement::Strict) => {
            write_audit_and_publish_failure(state, None, "missing_header").await;
            return Err(SigningGateError::MissingHeader);
        }
        (Some(hv), _) => verify_with_enforcement(state, &hv, payload).await,
    }
}

async fn verify_with_enforcement(
    state: &AppState,
    header_value: &str,
    payload: &[u8],
) -> Result<(), SigningGateError> {
    let result: Result<SignedFields, SigningGateError> = async {
        let envelope = SignatureEnvelope::decode_header(header_value)
            .map_err(SigningGateError::from)?;

        // Recompute payload hash.
        let expected = payload_sha256_hex(payload);
        if envelope.fields.payload_hash != expected {
            return Err(SigningGateError::Signature(SignatureError::InvalidSignature));
        }

        // Fetch verifying key (cache-first).
        let cache_key = (
            envelope.fields.tenant_id,
            envelope.fields.host_id,
            envelope.fields.key_version,
        );
        let verifying = match state.verifying_key_cache.get(&cache_key).await {
            Some(k) => k,
            None => {
                let row = device_keys::get_device_key(
                    &state.db, envelope.fields.host_id, envelope.fields.key_version as i32
                ).await?
                .ok_or(SigningGateError::Signature(SignatureError::KeyVersionUnknown {
                    got: envelope.fields.key_version,
                }))?;
                let pk: [u8; 32] = row.public_key_bytes.as_slice().try_into()
                    .map_err(|_| SigningGateError::Signature(SignatureError::InvalidKey("pk length".into())))?;
                let key = VerifyingKey::from_bytes(&pk)
                    .map_err(|_| SigningGateError::Signature(SignatureError::InvalidKey("pk format".into())))?;
                state.verifying_key_cache.insert(cache_key, key).await;
                key
            }
        };

        // Signature + replay.
        verify_envelope(&envelope.fields, &envelope.signature, &verifying)?;

        // Atomic DB advance — defense in depth beyond in-memory cache.
        let mut tx = state.db.begin().await?;
        let row = device_keys::get_device_key(&state.db, envelope.fields.host_id, envelope.fields.key_version as i32)
            .await?.ok_or(SigningGateError::Signature(SignatureError::KeyVersionUnknown {
                got: envelope.fields.key_version,
            }))?;
        check_replay(
            envelope.fields.sequence_number,
            row.sequence_number_offset as u64,
            envelope.fields.timestamp,
            Utc::now(),
        )?;
        let advanced = device_keys::advance_verify_offset(
            &mut tx, envelope.fields.host_id, envelope.fields.key_version as i32,
            envelope.fields.sequence_number as i64,
        ).await?;
        if advanced.is_none() {
            tx.rollback().await?;
            return Err(SigningGateError::Signature(SignatureError::ReplayRejected {
                reason: ReplayReason::SequenceTooLow { got: 0, cached: 0 },
            }));
        }
        tx.commit().await?;

        Ok(envelope.fields)
    }.await;

    match (result, state.signed_dispatch_enforcement) {
        (Ok(_fields), _) => Ok(()),
        (Err(e), SignedDispatchEnforcement::Off) => {
            tracing::warn!(err = %e, "signature verification failed; enforcement=Off, accepting");
            Ok(())
        }
        (Err(e), SignedDispatchEnforcement::Audit) => {
            tracing::warn!(err = %e, "signature verification failed; enforcement=Audit, accepting");
            write_audit_and_publish_failure(state, None, &e.to_string()).await;
            Ok(())
        }
        (Err(e), SignedDispatchEnforcement::Strict) => {
            tracing::error!(err = %e, "signature verification failed; enforcement=Strict, rejecting");
            write_audit_and_publish_failure(state, None, &e.to_string()).await;
            Err(e)
        }
    }
}

async fn write_audit_and_publish_failure(
    state: &AppState,
    host_id: Option<Uuid>,
    reason: &str,
) {
    // Reuse roz_safety_audit_log (Planner's Discretion Q4).
    if let Err(e) = roz_db::safety_audit::insert_signature_failure(&state.db, host_id, reason).await {
        tracing::error!(err = %e, "failed to write signature-failure audit row");
    }
    // Publish NATS event if we know the host.
    if let Some(host) = host_id {
        if let Ok(subject) = Subjects::safety_signature_failure_worker(&host.to_string()) {
            if let Err(e) = state.nats.publish(subject, reason.as_bytes().to_vec().into()).await {
                tracing::error!(err = %e, "failed to publish safety.signature_failure");
            }
        }
    }
}
```

Notes:
- The `insert_signature_failure` helper on `roz_db::safety_audit` may need a thin wrapper added — if the existing `safety_audit` module doesn't have a generic "insert a row" surface, add a 10-line helper function there as a sub-step (treat as an inline edit to `crates/roz-db/src/safety_audit.rs`; if that file doesn't exist, add to this plan's `files_modified` and create it following the `crates/roz-db/src/hosts.rs` pattern). Scope is one function — does not warrant a separate plan.
- Register the new module in `crates/roz-server/src/lib.rs`: `pub mod signing_gate;`.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo check -p roz-server && cargo clippy -p roz-server --no-deps -- -D warnings 2>&1 | tail -20</automated>
  </verify>
  <done>signing_gate.rs compiles clean; no clippy warnings; module registered in lib.rs; audit helper exists.</done>
</task>

<task type="auto">
  <name>Task 2: Add publish_signed() helper to roz-nats::dispatch + wire tasks.rs</name>
  <files>crates/roz-nats/src/dispatch.rs, crates/roz-nats/src/lib.rs, crates/roz-nats/Cargo.toml, crates/roz-server/src/routes/tasks.rs</files>
  <action>
1. In `crates/roz-nats/Cargo.toml`, add (if not already present):
   ```toml
   [dependencies]
   async-nats = { workspace = true }   # confirm
   ```

2. In `crates/roz-nats/src/dispatch.rs`, add a transport-layer helper that takes the already-built header value and attaches it. This keeps `roz-nats` transport-focused; building the header value is the caller's job (done by `roz-server::signing_gate::sign_outbound`).

   ```rust
   use async_nats::{Client, HeaderMap, HeaderValue};

   /// Publish a payload with a `roz-sig-v1` header attached. The header value
   /// must be pre-built by the signing layer (see
   /// `roz_server::signing_gate::sign_outbound`).
   ///
   /// Header name constant is imported from `roz_core::signing::HEADER_NAME`.
   pub async fn publish_signed(
       client: &Client,
       subject: String,
       payload: Vec<u8>,
       header_value: &str,
   ) -> Result<(), async_nats::Error> {
       let mut headers = HeaderMap::new();
       headers.insert(
           roz_core::signing::HEADER_NAME,
           HeaderValue::from_str(header_value).map_err(|e| {
               async_nats::Error::from(format!("invalid header value: {e}"))
           })?,
       );
       client.publish_with_headers(subject, headers, payload.into()).await
   }
   ```

   Re-export from `crates/roz-nats/src/lib.rs`:
   ```rust
   pub use dispatch::publish_signed;
   ```

3. In `crates/roz-server/src/routes/tasks.rs`, locate the existing `nats.publish(subject, payload.into()).await` call (per RESEARCH.md around line 214 of the current dispatch flow — verify exact line at execution time) and replace with:

   ```rust
   let header_value = crate::signing_gate::sign_outbound(
       &state, tenant_id, host_id, task.id, &payload_bytes
   ).await.map_err(|e| {
       tracing::error!(err = %e, "sign_outbound failed");
       crate::error::AppError::internal("sign_outbound failed".into())
   })?;

   roz_nats::publish_signed(&state.nats, subject, payload_bytes.clone(), &header_value)
       .await
       .map_err(|e| crate::error::AppError::internal(format!("publish_signed: {e}")))?;
   ```

   Verify that the trust-gate (`check_host_trust`) runs BEFORE this block per RESEARCH.md F10.

   On sign failure in `Strict` mode, update the task to `failed` status (parallel to existing publish-failure handling around line 215 per RESEARCH.md).
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo check -p roz-nats && cargo check -p roz-server 2>&1 | tail -20</automated>
  </verify>
  <done>`publish_signed` helper compiles; `tasks.rs` calls `sign_outbound` before `publish_signed`; no bare `nats.publish` left on the server→worker dispatch path; trust-gate order preserved.</done>
</task>

<task type="auto">
  <name>Task 3: Integration tests — sign→verify round-trip, tamper rejection, replay rejection, enforcement modes</name>
  <files>crates/roz-server/src/signing_gate.rs</files>
  <action>
Append an integration test module at the bottom of `signing_gate.rs` (or create `crates/roz-server/tests/signing_gate.rs` — either matches existing precedent). Use `roz_test::pg::test_pool()` and construct a minimal `AppState` fixture (helper function in the test module — some existing test uses this pattern; see `crates/roz-server/tests/` for examples).

Tests:
1. `sign_outbound_then_verify_inbound_round_trip` — provision a worker, fabricate a worker→server envelope with the provisioned key, call `verify_inbound` on an inline-built `HeaderMap` + payload → Ok.
2. `tampered_payload_rejected_strict` — round-trip but flip a byte of payload before `verify_inbound` → Err.
3. `tampered_signature_rejected_strict` — flip a signature byte → Err.
4. `replay_detection_sequence_rollback` — send seq N, then send seq N again → second call Err `ReplayRejected(SequenceTooLow)`.
5. `replay_detection_timestamp_skew` — build envelope with timestamp 10 s ago → Err `ReplayRejected(TimestampSkew)`.
6. `enforcement_off_accepts_unsigned` — set `state.signed_dispatch_enforcement = Off`, call verify_inbound with None headers → Ok + warn log.
7. `enforcement_audit_logs_but_accepts` — Audit mode, unsigned → Ok + audit row inserted.
8. `enforcement_strict_rejects_unsigned` — Strict mode, unsigned → Err + audit row + NATS subject publish.
9. `revoked_key_rejected` — provision, then `device_keys::set_revoked`, then send signed message → Err (key lookup returns None → KeyVersionUnknown).
10. `cache_warms_on_first_verify` — sequence-check: first verify populates cache; second verify (same version) does zero DB reads for the verifying key (instrument via sqlx logger or a counter helper).
11. `cache_invalidates_on_revocation` — pre-populate cache, then rotate (from 23-05 flow) or revoke → subsequent verify either re-fetches or fails.

Use `tokio::test(flavor = "multi_thread")`. Allow `#[ignore]` if CI is slow; include a CI job running `--include-ignored`.

Finally, add a targeted benchmark-ish unit test to assert cache-hit path calls zero `device_keys::get_device_key`:

```rust
#[tokio::test]
async fn verify_cache_hit_does_not_touch_db() {
    let state = build_test_appstate().await;
    let (tenant, host, key_version) = provision_worker(&state).await;
    let fields = sign_test_envelope(&state, tenant, host, 1, b"payload").await;

    // Pre-warm cache.
    verify_inbound(&state, Some(&header_map_with(fields.clone())), b"payload").await.unwrap();

    // Second call — verify the cache was hit. Instrument by temporarily
    // wrapping the pool in a counter (or just assert latency threshold).
    let before = std::time::Instant::now();
    verify_inbound(&state, Some(&header_map_with(fields)), b"payload2"))  // different payload_hash — will fail, but cache lookup still counts
        .await.ok();
    assert!(before.elapsed() < std::time::Duration::from_millis(50),
        "cache hit should be sub-ms; instead took {:?}", before.elapsed());
}
```
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-server signing_gate:: -- --include-ignored 2>&1 | tail -40</automated>
  </verify>
  <done>All 11 tests pass; coverage includes every enforcement mode + every SignatureError variant + the cache warm/invalidate path; clippy clean on the test module.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| NATS inbound payload bytes → application logic | All worker→server traffic passes through `verify_inbound` before deserialization. |
| DB advance_verify_offset atomicity | Transactional high-water mark is the defense-in-depth against distributed replay. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-22 | Tampering | NATS payload modified in flight | mitigate | `payload_hash` signed + recomputed; mismatch → InvalidSignature. |
| T-23-23 | Replay | attacker re-publishes a captured signed message | mitigate | Two-layer: in-memory cache seq check + DB atomic advance_verify_offset. |
| T-23-24 | Elevation of Privilege | attacker publishes unsigned message when enforcement=Strict | mitigate | Strict mode rejects + publishes safety.signature_failure.{host_id}. |
| T-23-25 | Denial of Service | flood of invalid sigs exhausts DB connections | mitigate | Cache-first lookup; invalid sigs short-circuit before DB if key already cached. Rate-limit on the NATS side is transport-inherent. |
| T-23-26 | Information Disclosure | failure reason leaks key material | mitigate | `SigningGateError::Display` uses opaque "signature verification failed"; detailed variant only in tracing fields. |
| T-23-27 | Repudiation | operator disputes a rejection | mitigate | Every Strict-mode rejection writes a `roz_safety_audit_log` row + publishes to NATS subject. |
</threat_model>

<verification>
- `cargo test -p roz-server signing_gate::` all pass
- `cargo clippy -p roz-server --no-deps -- -D warnings` clean
- `cargo fmt --check` clean
- Manual trace: warm a cache entry, tail `tracing` output, confirm second verify produces zero DB-lookup spans
</verification>

<success_criteria>
- `sign_outbound` attaches `roz-sig-v1` header to every server→worker publish in `routes/tasks.rs`
- `verify_inbound` enforces sig + replay + revocation with three enforcement modes
- Cache-hit path <100 µs (asserted by test); cache miss falls through to DB
- Failures write audit rows + publish `safety.signature_failure.{host_id}`
- Commit: `feat(23-06): server-side sign hook + verify gate with enforcement modes`
</success_criteria>

<output>
After completion, create `.planning/phases/23-.../23-06-SUMMARY.md` with: sign/verify orchestration entry points, enforcement branching table, cache hit/miss flow, audit fields, and any deviations from RESEARCH.md's sketch.
</output>
