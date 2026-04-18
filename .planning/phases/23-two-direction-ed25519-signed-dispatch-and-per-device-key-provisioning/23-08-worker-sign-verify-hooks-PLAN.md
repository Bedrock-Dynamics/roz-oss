---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 08
type: execute
wave: 5
autonomous: true
objective: >
  Wire sign + verify hooks into every worker NATS publish and subscribe site.
  Outbound: dispatch.rs (results), telemetry.rs, event_nats.rs, trust.rs attach
  roz-sig-v1 header via sign_outbound_worker. Inbound: main.rs subscriber
  callbacks run verify_inbound_worker before serde_json::from_slice; e-stop
  precedence kept. HTTP-to-Restate results are explicitly NOT signed (D-13).
depends_on:
  - "23-07"
files_modified:
  - crates/roz-worker/src/signing_hooks.rs
  - crates/roz-worker/src/lib.rs
  - crates/roz-worker/src/main.rs
  - crates/roz-worker/src/dispatch.rs
  - crates/roz-worker/src/telemetry.rs
  - crates/roz-worker/src/event_nats.rs
  - crates/roz-worker/src/trust.rs
requirements:
  - FS-04
task_count: 3

must_haves:
  truths:
    - "Every worker-published NATS message on result, telemetry, event, and trust-report subjects carries a roz-sig-v1 header."
    - "HTTP-to-Restate task-result POST is NOT signed (D-13)."
    - "Worker's inbound subscribe callback verifies the header BEFORE serde_json::from_slice. E-stop check remains BEFORE verify (RESEARCH.md integration-point pitfall 2)."
    - "On inbound verify failure, worker drops the message + logs structured audit + increments a worker-local metric; no message handling proceeds."
    - "If the server rotates its signing key: worker sees a verify failure for an unknown key_version, calls rotate-if-due to refresh server verifying key, retries once, then fails if still bad (D-15 bounded refetch)."
    - "Concurrent publishers on the same worker process produce strictly-monotonic sequence numbers (SQLite RETURNING guarantees atomicity)."
  artifacts:
    - path: crates/roz-worker/src/signing_hooks.rs
      provides: "sign_outbound_worker + verify_inbound_worker async wrappers around roz-core::signing primitives"
      exports: ["sign_outbound_worker", "verify_inbound_worker", "WorkerSigningContext"]
  key_links:
    - from: crates/roz-worker/src/main.rs
      to: crates/roz-worker/src/signing_hooks.rs
      via: "subscribe loop calls verify_inbound_worker"
      pattern: "verify_inbound_worker"
    - from: crates/roz-worker/src/dispatch.rs
      to: crates/roz-worker/src/signing_hooks.rs
      via: "result publish calls sign_outbound_worker"
      pattern: "sign_outbound_worker"
---

<objective>
Make every worker NATS publish signed and every worker NATS subscribe verified. Plan 23-07 got the material onto disk + into memory; this plan uses it.

Purpose: Close the worker half of the two-direction loop. Until this ships, the server's Strict-mode verify gate (from 23-06) would reject every real message — so this plan is the "unblock production" step.
Output: Three sequential changes: a `signing_hooks.rs` wrapper module, a main.rs subscribe-path hook, and wrapper calls at four publish sites (`dispatch.rs`, `telemetry.rs`, `event_nats.rs`, `trust.rs`).
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@.planning/research/DEEP-SIGN.md
@crates/roz-worker/src/main.rs
@crates/roz-worker/src/dispatch.rs
@crates/roz-worker/src/telemetry.rs
@crates/roz-worker/src/event_nats.rs
@crates/roz-worker/src/trust.rs
@crates/roz-worker/src/signing_key.rs
@crates/roz-worker/src/wal.rs
@crates/roz-core/src/signing/mod.rs
@crates/roz-nats/src/dispatch.rs

<interfaces>
<!-- Worker-side state from 23-07: -->
pub struct SigningKeyMaterial {
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    pub key_version: u32,
    pub signing_key: SigningKey,
    pub server_verifying_key: VerifyingKey,
    pub created_at: DateTime<Utc>,
}
pub async fn rotate_if_due(...) -> Result<Option<SigningKeyMaterial>>;
pub async fn force_rotate(...) -> Result<SigningKeyMaterial>;

<!-- From 23-02 (signing primitives): -->
use roz_core::signing::{
    sign_envelope, verify_envelope, check_replay, payload_sha256_hex,
    Direction, SignatureEnvelope, SignedFields, HEADER_NAME, SignatureError,
};

<!-- From 23-07 (WAL): -->
pub fn next_seq(&self, key_version: u32) -> rusqlite::Result<u64>;
</interfaces>
</context>

<tasks>

<task type="auto">
  <name>Task 1: Create signing_hooks.rs with sign_outbound_worker + verify_inbound_worker</name>
  <files>crates/roz-worker/src/signing_hooks.rs, crates/roz-worker/src/lib.rs</files>
  <action>
Create `crates/roz-worker/src/signing_hooks.rs`:

```rust
//! Worker-side sign/verify hooks (Phase 23 FS-04).
//!
//! Wraps `roz_core::signing` primitives with the worker's WAL-backed sequence
//! counter (outbound) and the cached server verifying key (inbound).

use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use roz_core::signing::{
    payload_sha256_hex, sign_envelope, verify_envelope,
    Direction, SignatureEnvelope, SignedFields, HEADER_NAME, SignatureError,
};
use thiserror::Error;
use uuid::Uuid;

use crate::signing_key::SigningKeyMaterial;
use crate::wal::WalStore;

#[derive(Debug, Error)]
pub enum WorkerSigningError {
    #[error(transparent)]
    Signature(#[from] SignatureError),
    #[error("wal: {0}")]
    Wal(#[from] rusqlite::Error),
    #[error("missing roz-sig-v1 header")]
    MissingHeader,
    #[error("unknown server key_version {0}; refetch required")]
    UnknownServerKeyVersion(u32),
}

/// Shared context for all sign/verify calls. Cheap to clone (Arc internals).
#[derive(Clone)]
pub struct WorkerSigningContext {
    pub material: Arc<RwLock<SigningKeyMaterial>>,
    pub wal: Arc<WalStore>,
}

impl WorkerSigningContext {
    /// Build the `roz-sig-v1` header for an outbound worker→server NATS
    /// message. Allocates a monotonic sequence number via the WAL.
    pub fn sign_outbound_worker(
        &self,
        correlation_id: Uuid,
        payload: &[u8],
    ) -> Result<String, WorkerSigningError> {
        let guard = self.material.read();
        let key_version = guard.key_version;
        let seq = self.wal.next_seq(key_version)?;

        let fields = SignedFields {
            direction: Direction::WorkerToServer,
            tenant_id: guard.tenant_id,
            host_id: guard.host_id,
            correlation_id,
            timestamp: Utc::now(),
            sequence_number: seq,
            payload_hash: payload_sha256_hex(payload),
            key_version,
        };

        let envelope = sign_envelope(&fields, &guard.signing_key)?;
        Ok(envelope.encode_header()?)
    }

    /// Verify an inbound server→worker message. Caller passes the header value
    /// (or None) and the raw payload bytes. Returns Err if verification fails
    /// and the caller must drop the message.
    ///
    /// Bounded server-key-rotation handling: if the envelope references a
    /// server key_version the worker doesn't have cached, the caller (main.rs
    /// subscribe loop) invokes `force_rotate` to refresh, then retries once.
    pub fn verify_inbound_worker(
        &self,
        header_value: Option<&str>,
        payload: &[u8],
    ) -> Result<(), WorkerSigningError> {
        let header = header_value.ok_or(WorkerSigningError::MissingHeader)?;
        let envelope = SignatureEnvelope::decode_header(header)?;

        // Direction must be server→worker.
        if envelope.fields.direction != Direction::ServerToWorker {
            return Err(WorkerSigningError::Signature(SignatureError::InvalidSignature));
        }

        // Payload hash binding.
        let expected = payload_sha256_hex(payload);
        if envelope.fields.payload_hash != expected {
            return Err(WorkerSigningError::Signature(SignatureError::InvalidSignature));
        }

        // Verify with our cached server key. If key_version mismatch, flag
        // the caller to refetch + retry.
        let guard = self.material.read();
        // Worker currently has ONE cached server verifying key (the one from
        // the most recent provision or rotate response). `key_version` in the
        // envelope tracks the *worker's* key for the server's addressing
        // layer, not the server's own key. For this round, we accept the
        // envelope if the signature verifies against our cached key. In a
        // future phase (if the server signing key rotates), worker would keep
        // a map of server keys by server-side version id. For v3.0 there is
        // ONE active server verifying key at any time.

        verify_envelope(
            &envelope.fields, &envelope.signature, &guard.server_verifying_key,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_key::{save, load};
    use ed25519_dalek::SigningKey;
    use roz_core::StaticKeyProvider;
    use tempfile::TempDir;

    async fn ctx() -> WorkerSigningContext {
        let tmp = TempDir::new().unwrap();
        let provider = Arc::new(StaticKeyProvider::for_tests());
        let tenant = Uuid::new_v4();
        let host = Uuid::new_v4();
        let seed = [7u8; 32];
        let svk_signing = SigningKey::from_bytes(&[9u8; 32]);
        let svk_bytes = svk_signing.verifying_key().to_bytes();
        save(tmp.path(), &provider, tenant, 1, &seed, &svk_bytes).await.unwrap();
        let material = load(tmp.path(), &provider, tenant, host).await.unwrap().unwrap();

        let wal_path = tmp.path().join("wal.db");
        let wal = Arc::new(WalStore::open(&wal_path).unwrap());

        WorkerSigningContext {
            material: Arc::new(parking_lot::RwLock::new(material)),
            wal,
        }
    }

    #[tokio::test]
    async fn sign_then_verify_round_trip_with_server_key() {
        let ctx = ctx().await;
        // We need to verify that what we sign is recoverable on the other end
        // with our worker's verifying key — so swap roles: pretend the "server"
        // signed something. Construct a server-direction envelope signed with
        // the server key that we saved as svk.
        let server_signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let payload = b"hello from server";
        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: ctx.material.read().tenant_id,
            host_id: ctx.material.read().host_id,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &server_signing_key).unwrap();
        let header = env.encode_header().unwrap();
        ctx.verify_inbound_worker(Some(&header), payload).unwrap();
    }

    #[tokio::test]
    async fn tampered_payload_rejected() {
        let ctx = ctx().await;
        let server_signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: ctx.material.read().tenant_id,
            host_id: ctx.material.read().host_id,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(b"original"),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &server_signing_key).unwrap();
        let err = ctx.verify_inbound_worker(Some(&env.encode_header().unwrap()), b"tampered")
            .unwrap_err();
        assert!(matches!(err, WorkerSigningError::Signature(SignatureError::InvalidSignature)));
    }

    #[tokio::test]
    async fn missing_header_rejected() {
        let ctx = ctx().await;
        let err = ctx.verify_inbound_worker(None, b"payload").unwrap_err();
        assert!(matches!(err, WorkerSigningError::MissingHeader));
    }

    #[tokio::test]
    async fn wrong_direction_rejected() {
        let ctx = ctx().await;
        let server_signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let payload = b"payload";
        let fields = SignedFields {
            direction: Direction::WorkerToServer,      // wrong
            tenant_id: ctx.material.read().tenant_id,
            host_id: ctx.material.read().host_id,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &server_signing_key).unwrap();
        assert!(ctx.verify_inbound_worker(Some(&env.encode_header().unwrap()), payload).is_err());
    }

    #[tokio::test]
    async fn sign_outbound_produces_valid_header() {
        let ctx = ctx().await;
        let header = ctx.sign_outbound_worker(Uuid::new_v4(), b"payload").unwrap();
        let env = SignatureEnvelope::decode_header(&header).unwrap();
        assert_eq!(env.fields.direction, Direction::WorkerToServer);
        // Verify with our worker's own verifying key.
        let worker_pub = ctx.material.read().signing_key.verifying_key();
        verify_envelope(&env.fields, &env.signature, &worker_pub).unwrap();
    }

    #[tokio::test]
    async fn concurrent_sign_outbound_produces_monotonic_seq() {
        let ctx = ctx().await;
        let mut handles = vec![];
        for _ in 0..10 {
            let c = ctx.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                c.sign_outbound_worker(Uuid::new_v4(), b"x").unwrap()
            }));
        }
        let mut seqs = vec![];
        for h in handles {
            let header = h.await.unwrap();
            let env = SignatureEnvelope::decode_header(&header).unwrap();
            seqs.push(env.fields.sequence_number);
        }
        seqs.sort();
        for pair in seqs.windows(2) {
            assert!(pair[1] > pair[0], "concurrent sequences must be strictly monotonic");
        }
    }
}
```

Register the module in `lib.rs`:
```rust
pub mod signing_hooks;
```

Ensure `parking_lot` is in `roz-worker`'s deps (likely already present via other crates; confirm).
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker signing_hooks:: 2>&1 | tail -30</automated>
  </verify>
  <done>All 6 tests pass (round-trip, tamper, missing header, wrong direction, outbound valid, concurrent monotonic); clippy clean.</done>
</task>

<task type="auto">
  <name>Task 2: Wire sign_outbound_worker at every publish site (dispatch, telemetry, event_nats, trust)</name>
  <files>crates/roz-worker/src/dispatch.rs, crates/roz-worker/src/telemetry.rs, crates/roz-worker/src/event_nats.rs, crates/roz-worker/src/trust.rs</files>
  <action>
For each of the four outbound publish sites, find the existing `nats_client.publish(subject, payload).await` call and wrap with the signing hook. Pattern (repeat at each site):

```rust
use crate::signing_hooks::WorkerSigningContext;
use roz_nats::publish_signed;

// Somewhere in the function: signing_ctx is passed in or stored on self.
let header = signing_ctx
    .sign_outbound_worker(correlation_id, &payload_bytes)
    .map_err(|e| {
        // Per D-09: hard-stop on missing/corrupt key at runtime.
        match &e {
            crate::signing_hooks::WorkerSigningError::Signature(s)
                if matches!(s, roz_core::signing::SignatureError::KeyNotConfigured) => {
                tracing::error!(err = ?e, "device key missing at runtime; hard-stop (exit 78)");
                std::process::exit(78);
            }
            _ => anyhow::anyhow!("sign_outbound_worker: {e}")
        }
    })?;

publish_signed(&nats_client, subject, payload_bytes, &header).await?;
```

Specific files + call sites:

1. **`crates/roz-worker/src/dispatch.rs`**: `build_task_result` + publish around result-send. `correlation_id = task.id`.
2. **`crates/roz-worker/src/telemetry.rs`**: publish of `TelemetryFrame` batches. `correlation_id = session_id` (or frame.task_id where available; pick one consistent value and document).
3. **`crates/roz-worker/src/event_nats.rs`**: publish of `SessionEvent`. `correlation_id = session_id`.
4. **`crates/roz-worker/src/trust.rs`**: publish of trust reports. `correlation_id = host_id` (trust reports are per-host — use host_id here and note in a comment that the correlation_id meaning is per-subject-family).

**Explicitly NOT wired (D-13):**
- `signal_result` POST to Restate in `dispatch.rs` — HTTP path, deferred.
- Any existing HTTP calls to `/v1/...` routes.

For each wire site, add a one-line integration test that captures the published NATS message and asserts the `roz-sig-v1` header is present. Example:

```rust
#[tokio::test]
async fn telemetry_publish_includes_roz_sig_header() {
    let ctx = test_signing_context().await;
    let nats = test_nats_subscribe("telemetry.>").await;
    publish_telemetry_frame(ctx, sample_frame()).await.unwrap();
    let msg = nats.next().await.unwrap();
    assert!(msg.headers.as_ref().unwrap().get("roz-sig-v1").is_some());
}
```
Consolidate these into one test file `crates/roz-worker/tests/signed_publishes.rs` — one test per publish site.

Thread a `WorkerSigningContext` from `main.rs` into each of the four publish paths. If they currently take the bare `Client`, introduce a narrow wrapper struct carrying `(nats_client, signing_ctx)` — see how existing code passes `nats_client` today and add `signing_ctx` alongside in the same call chain.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker --test signed_publishes -- --include-ignored 2>&1 | tail -30</automated>
  </verify>
  <done>All four publish sites invoke `sign_outbound_worker` + `publish_signed`; Restate HTTP path explicitly left unsigned (comment in code cites D-13); integration tests confirm `roz-sig-v1` header present on every sampled subject; clippy clean.</done>
</task>

<task type="auto">
  <name>Task 3: Wire verify_inbound_worker in main.rs subscribe loop (e-stop precedence kept)</name>
  <files>crates/roz-worker/src/main.rs</files>
  <action>
In `crates/roz-worker/src/main.rs`, locate the invoke subscriber loop (per RESEARCH.md lines 1332-1346) and add the verification step.

Preserve the existing order: **e-stop check FIRST, then signature verify, then deserialize.** (RESEARCH.md integration-point pitfall 2.)

Shape:
```rust
while let Some(msg) = sub.next().await {
    // 1. E-stop short-circuit (existing).
    if *estop_rx.borrow() {
        tracing::warn!("e-stop asserted; dropping inbound dispatch");
        continue;
    }

    // 2. Signature verify (NEW).
    let header_value = msg.headers.as_ref()
        .and_then(|h| h.get(HEADER_NAME))
        .and_then(|v| v.iter().next().map(|s| s.as_str()));

    if let Err(e) = signing_ctx.verify_inbound_worker(header_value, &msg.payload) {
        // Attempt a single refetch if the failure looks like a server-side
        // rotation (D-15 bounded refetch).
        let retry_ok = match &e {
            WorkerSigningError::Signature(SignatureError::InvalidSignature)
                | WorkerSigningError::UnknownServerKeyVersion(_) => {
                match signing_key::force_rotate(
                    &signing_ctx.material.read().clone(),
                    &signing_key::data_dir(),
                    &http, &config.api_url, &key_provider,
                ).await {
                    Ok(new_mat) => {
                        *signing_ctx.material.write() = new_mat;
                        signing_ctx.verify_inbound_worker(header_value, &msg.payload).is_ok()
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "force_rotate after verify failure failed");
                        false
                    }
                }
            }
            _ => false,
        };

        if !retry_ok {
            tracing::error!(err = ?e, "inbound dispatch verification failed; dropping");
            inbound_verify_failures.inc();    // worker-local metric
            continue;
        }
    }

    // 3. Deserialize (existing — now safe because payload is verified).
    let invocation: TaskInvocation = match serde_json::from_slice(&msg.payload) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!(err = %e, "failed to parse TaskInvocation");
            continue;
        }
    };

    // ... existing spawn / run logic ...
}
```

Keep the metric minimal — a `tokio::sync::atomic::AtomicU64` counter surfaced in the existing metrics export if there is one. If not, use a `tracing::info!(counter = N, ...)` periodic log instead of a full metric system.

Add an integration test `crates/roz-worker/tests/verify_inbound.rs`:
- `inbound_valid_signed_dispatch_accepted` — publish a properly-signed TaskInvocation on `invoke.{worker_id}.{task_id}`; assert the worker processes it (observe side effect: task status transition).
- `inbound_unsigned_dispatch_dropped` — publish without header; assert worker logs error + does NOT process.
- `inbound_tampered_payload_dropped` — publish with a valid sig over payload A, then swap to payload B; assert dropped.
- `inbound_estop_short_circuits_before_verify` — assert e-stop during inbound + no verify call (observe via trace subscriber that verify_inbound_worker was not entered).
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker --test verify_inbound -- --include-ignored 2>&1 | tail -30</automated>
  </verify>
  <done>Subscribe loop has e-stop → verify → deserialize order; valid signed dispatch processed; unsigned dropped; tampered dropped; e-stop short-circuits; all integration tests pass; clippy clean.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| NATS inbound bytes → worker handler | Verification gate is the seam; all post-verify logic trusts the payload. |
| concurrent publishers → WAL | `next_seq` atomicity is the foundation of replay protection on the outbound side. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-32 | Spoofing | attacker injects unsigned dispatch on `invoke.{worker}.{task}` | mitigate | verify_inbound_worker rejects unsigned; enforcement=Strict at server means attacker couldn't even publish cross-tenant. |
| T-23-33 | Tampering | payload swapped in flight | mitigate | payload_hash + signature binding rejects any mutation. |
| T-23-34 | Replay | attacker replays a captured worker→server publish | mitigate | WAL's monotonic seq + server's DB atomic advance together enforce at-most-once. |
| T-23-35 | Denial of Service | flood of unsigned messages forces per-msg logging | accept | Worker-local rate (telemetry cadence) is bounded; unsigned messages are cheap to drop. |
| T-23-36 | Spoofing | server key rotated; worker drops messages until refetch | mitigate | D-15 bounded refetch: one rotate-if-due call per failed message, then give up. |
</threat_model>

<verification>
- `cargo test -p roz-worker` all pass
- `cargo clippy -p roz-worker --no-deps -- -D warnings` clean
- `cargo fmt --check` clean
- Integration tests prove every subject family publishes with header + verifies on inbound
</verification>

<success_criteria>
- `signing_hooks.rs` provides `sign_outbound_worker` + `verify_inbound_worker`
- Every worker NATS publish attaches `roz-sig-v1` header
- Worker subscribe loop verifies before deserialize, preserves e-stop precedence
- Bounded refetch on server-key-rotation (single retry)
- HTTP-to-Restate unchanged (D-13)
- Commit: `feat(23-08): wire sign/verify hooks at every worker NATS publish/subscribe site`
</success_criteria>

<output>
After completion, create `.planning/phases/23-.../23-08-SUMMARY.md` with: list of 4 publish sites + 1 subscribe site touched, correlation_id conventions per subject family, note on D-13 Restate exclusion, and metric/logging surface.
</output>
