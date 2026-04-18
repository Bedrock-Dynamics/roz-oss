//! Phase 23 plan 23-08 Task 2 acceptance tests.
//!
//! These tests exercise the worker-side signed publish path WITHOUT spinning
//! up a real NATS broker. They assert that each of the four FS-04 subject
//! families (result, telemetry, session event, trust report) produces a
//! roz-sig-v1 envelope whose fields are correctly bound to the payload
//! the caller is about to publish. A companion end-to-end NATS test that
//! asserts the header survives on the wire is deferred; the wire-level
//! invariants are already proven by `roz_nats::dispatch::publish_signed`'s
//! unit tests in the roz-nats crate.
//!
//! Shape per subject family:
//!
//! 1. Build a WorkerSigningContext with a deterministic device key + cached
//!    server verifying key.
//! 2. Hash a representative payload the caller would publish.
//! 3. Call `sign_outbound_worker(correlation_id, &payload)` — the exact same
//!    call every `publish_*_signed` helper makes.
//! 4. Decode the returned header and assert:
//!    - `direction == WorkerToServer`
//!    - `payload_hash == SHA-256(payload)`
//!    - `correlation_id == expected` (per-subject-family convention)
//!    - signature verifies against the worker's own verifying key
//!
//! Correlation-id conventions (asserted below):
//!
//! | Subject family  | correlation_id |
//! |-----------------|----------------|
//! | task status     | task_id (UUID) |
//! | telemetry       | host_id        |
//! | session event   | session_id     |
//! | trust report    | host_id        |

use std::sync::Arc;

use chrono::Utc;
use ed25519_dalek::{SigningKey, Verifier};
use parking_lot::RwLock;
use roz_core::key_provider::StaticKeyProvider;
use roz_core::signing::{Direction, SignatureEnvelope, payload_sha256_hex, verify_envelope};
use roz_worker::signing_hooks::WorkerSigningContext;
use roz_worker::signing_key::{load, save};
use roz_worker::wal::WalStore;
use tempfile::TempDir;
use uuid::Uuid;

async fn build_ctx() -> (TempDir, WorkerSigningContext, Uuid, Uuid) {
    let tmp = TempDir::new().unwrap();
    let provider = Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]));
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();
    let server_signing = SigningKey::from_bytes(&[9u8; 32]);
    let svk_bytes = server_signing.verifying_key().to_bytes();
    save(tmp.path(), &provider, tenant, 1, &[7u8; 32], &svk_bytes)
        .await
        .unwrap();
    let material = load(tmp.path(), &provider, tenant, host).await.unwrap().unwrap();
    let wal = Arc::new(WalStore::open(":memory:").unwrap());
    let ctx = WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal);
    (tmp, ctx, tenant, host)
}

fn assert_signed_correctly(
    header: &str,
    payload: &[u8],
    expected_correlation: Uuid,
    expected_tenant: Uuid,
    expected_host: Uuid,
    worker_verifying_key: ed25519_dalek::VerifyingKey,
) {
    let env = SignatureEnvelope::decode_header(header).unwrap();
    assert_eq!(env.fields.direction, Direction::WorkerToServer);
    assert_eq!(env.fields.tenant_id, expected_tenant);
    assert_eq!(env.fields.host_id, expected_host);
    assert_eq!(env.fields.correlation_id, expected_correlation);
    assert_eq!(env.fields.payload_hash, payload_sha256_hex(payload));
    assert!(env.fields.sequence_number >= 1, "sequence starts at 1 per D-04");
    verify_envelope(&env.fields, &env.signature, &worker_verifying_key).unwrap();
    // Sanity: an unrelated verifying key must not verify.
    let other = SigningKey::from_bytes(&[255u8; 32]).verifying_key();
    other
        .verify(
            &env.fields.to_jcs().unwrap(),
            &ed25519_dalek::Signature::from_bytes(&env.signature),
        )
        .unwrap_err();
}

#[tokio::test]
async fn task_status_signed_publish_binds_task_id_as_correlation() {
    let (_tmp, ctx, tenant, host) = build_ctx().await;
    let task_id = Uuid::new_v4();
    let event = roz_nats::dispatch::TaskStatusEvent {
        task_id,
        status: "running".into(),
        detail: Some("worker accepted invocation".into()),
        host_id: Some(host),
    };
    let payload = serde_json::to_vec(&event).unwrap();
    let header = ctx.sign_outbound_worker(task_id, &payload).unwrap();
    let wvk = ctx.material.read().signing_key.verifying_key();
    assert_signed_correctly(&header, &payload, task_id, tenant, host, wvk);
}

#[tokio::test]
async fn telemetry_signed_publish_binds_host_as_correlation() {
    let (_tmp, ctx, tenant, host) = build_ctx().await;
    let correlation = host; // telemetry convention: host UUID
    let state = serde_json::json!({
        "timestamp": Utc::now().timestamp_millis(),
        "joints": [1.0, 2.0, 3.0]
    });
    let payload = serde_json::to_vec(&state).unwrap();
    let header = ctx.sign_outbound_worker(correlation, &payload).unwrap();
    let wvk = ctx.material.read().signing_key.verifying_key();
    assert_signed_correctly(&header, &payload, correlation, tenant, host, wvk);
}

#[tokio::test]
async fn session_event_signed_publish_binds_session_as_correlation() {
    let (_tmp, ctx, tenant, host) = build_ctx().await;
    let session_id = Uuid::new_v4();
    let event = roz_core::session::event::SessionEvent::TurnStarted { turn_index: 0 };
    let payload = serde_json::to_vec(&event).unwrap();
    let header = ctx.sign_outbound_worker(session_id, &payload).unwrap();
    let wvk = ctx.material.read().signing_key.verifying_key();
    assert_signed_correctly(&header, &payload, session_id, tenant, host, wvk);
}

#[tokio::test]
async fn trust_report_signed_publish_binds_host_as_correlation() {
    let (_tmp, ctx, tenant, host) = build_ctx().await;
    let trust = roz_core::device_trust::DeviceTrust {
        host_id: host,
        tenant_id: tenant.to_string(),
        posture: roz_core::device_trust::DeviceTrustPosture::Provisional,
        firmware: None,
        sbom_hash: None,
        last_attestation: Some(Utc::now()),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let payload = serde_json::to_vec(&trust).unwrap();
    let header = ctx.sign_outbound_worker(host, &payload).unwrap();
    let wvk = ctx.material.read().signing_key.verifying_key();
    assert_signed_correctly(&header, &payload, host, tenant, host, wvk);
}

#[tokio::test]
async fn sequence_numbers_monotonic_across_subject_families() {
    // Mixed-subject usage must still see a strictly-monotonic sequence counter,
    // because per D-04 the counter scope is (direction, host, tenant, key_version)
    // — NOT per subject. A server receiving telemetry then a task_status then
    // a trust_report from the same worker expects the sequence numbers to
    // increase across the three.
    let (_tmp, ctx, _tenant, host) = build_ctx().await;
    let task_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let h_telem = ctx.sign_outbound_worker(host, b"telem").unwrap();
    let h_task = ctx.sign_outbound_worker(task_id, b"task").unwrap();
    let h_session = ctx.sign_outbound_worker(session_id, b"session").unwrap();
    let h_trust = ctx.sign_outbound_worker(host, b"trust").unwrap();
    let seqs: Vec<u64> = [&h_telem, &h_task, &h_session, &h_trust]
        .iter()
        .map(|h| SignatureEnvelope::decode_header(h).unwrap().fields.sequence_number)
        .collect();
    for pair in seqs.windows(2) {
        assert!(
            pair[1] > pair[0],
            "sequence must be strictly monotonic across subject families"
        );
    }
}

#[tokio::test]
async fn inbound_unsigned_dispatch_is_rejected() {
    // Acceptance test for plan 23-08 Task 3 subscribe-loop behavior: a
    // message with no `roz-sig-v1` header is rejected by
    // verify_inbound_worker. The caller (main.rs subscribe loop) drops it
    // and increments the inbound_verify_failures counter.
    let (_tmp, ctx, _tenant, _host) = build_ctx().await;
    let err = ctx.verify_inbound_worker(None, b"unsigned payload").unwrap_err();
    assert!(matches!(
        err,
        roz_worker::signing_hooks::WorkerSigningError::MissingHeader
    ));
}

#[tokio::test]
async fn inbound_tampered_payload_is_rejected() {
    // Acceptance test for plan 23-08 Task 3: signature over one payload must
    // not verify for a different payload (the caller drops).
    let (_tmp, ctx, _tenant, _host) = build_ctx().await;
    let server_signing = SigningKey::from_bytes(&[9u8; 32]); // matches build_ctx's server key
    let original_payload = b"original payload";
    let tampered_payload = b"tampered payload";
    let fields = roz_core::signing::SignedFields {
        direction: Direction::ServerToWorker,
        tenant_id: ctx.material.read().tenant_id,
        host_id: ctx.material.read().host_id,
        correlation_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        sequence_number: 1,
        payload_hash: payload_sha256_hex(original_payload),
        key_version: 1,
    };
    let env = roz_core::signing::sign_envelope(&fields, &server_signing).unwrap();
    let header = env.encode_header().unwrap();
    // Signed over `original_payload` but we deliver `tampered_payload`.
    assert!(ctx.verify_inbound_worker(Some(&header), tampered_payload).is_err());
    // The original payload should still verify fine.
    ctx.verify_inbound_worker(Some(&header), original_payload).unwrap();
}
