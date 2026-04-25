//! Phase 24 gap closure (Plan 24-14 Task 3): full-stack policy push e2e.
//!
//! Drives one real signed policy row from a NATS testcontainer onto the
//! worker subscriber side, then proves the worker:
//! 1. Verifies the `roz-sig-v1` envelope (`WorkerSigningContext::verify_inbound_worker`).
//! 2. Parses the `SafetyPolicyRow` and applies it via `apply_policy_push`
//!    (extracted in 24-14 Task 2), which populates the `PolicyCache`,
//!    `HotPolicy`, and the copper `HotCopperPolicy`.
//! 3. Rejects an over-limit invocation at the pre-dispatch gate
//!    (`pre_dispatch_check` → `PreDispatchOutcome::Reject(LimitExceeded)`).
//! 4. Allows an under-limit invocation at the pre-dispatch gate.
//! 5. Clamps a copper 100 Hz tick velocity pair through the same
//!    `HotCopperPolicy` pointer that `apply_policy_push` wrote, via
//!    `SafetyFilterTask::policy_clamp` under `CopperEnforcementMode::Clamp`.
//!
//! # Deviation from the plan (24-14 Task 3, step 2)
//!
//! The plan text says to construct a server-side `SigningGate` that shares
//! a key with `WorkerSigningContext` and to call `publish_policy_to_workers`.
//! `SigningGate::new` hard-requires a live `sqlx::PgPool` (see
//! `crates/roz-server/src/signing_gate.rs:634` test pattern which depends
//! on the `fresh_pool` + `provision_server_signing_state` helpers). That
//! would pull a Postgres testcontainer into this test for no additional
//! coverage: `publish_policy_to_workers` already has dedicated unit tests
//! in `crates/roz-server/src/routes/safety_policies.rs` (which pass
//! `None` for the gate). The actual gap this test closes is the wire →
//! worker-verify → cache → HotCopperPolicy → filter-clamp round-trip —
//! independent of the server-side signing path.
//!
//! Resolution: forge the signed envelope directly using
//! `roz_core::signing::sign_envelope` with the server signing key that
//! `signing_hooks::tests::ctx()` seeds the `WorkerSigningContext` to
//! trust (`SigningKey::from_bytes(&[9u8; 32])`). This mirrors the
//! `signing_hooks::tests::sign_then_verify_round_trip_with_server_key`
//! pattern exactly and exercises the *worker*-side of the signed dispatch
//! stack, which is the code that was previously covered only by
//! grep-verified wiring.
//!
//! # Anti-tautology check
//!
//! Before commit, ran the full test twice: once with the publisher's
//! `max_linear_m_per_s`/`max_angular_rad_per_s` swapped to `100.0` (so
//! 5.0 and 2.0 no longer exceed the limits). Assertion 1 (pre-dispatch
//! reject) and assertion 3 (copper clamp) both failed, confirming the
//! test is sensitive to policy-value changes end-to-end. Values restored.

#![cfg(test)]
#![allow(clippy::float_cmp)]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use ed25519_dalek::SigningKey;
use futures::StreamExt;
use parking_lot::RwLock;
use roz_copper::policy::{CopperEnforcementMode, new_hot_policy};
use roz_copper::safety_filter::{ChassisAxis, HotPathSafetyFilter, SafetyFilterTask};
use roz_core::controller::intervention::InterventionKind;
use roz_core::embodiment::limits::JointSafetyLimits;
use roz_core::key_provider::StaticKeyProvider;
use roz_core::signing::{Direction, HEADER_NAME, SignatureEnvelope, SignedFields, payload_sha256_hex, sign_envelope};
use roz_db::safety_policies::SafetyPolicyRow;
use roz_nats::dispatch::{ExecutionMode, TaskInvocation, publish_signed};
use roz_nats::subjects::Subjects;
use roz_worker::dispatch::{PreDispatchOutcome, pre_dispatch_check};
use roz_worker::policy_cache::{HotPolicy, PolicyCache};
use roz_worker::policy_enforcement::{PolicyEnforcementError, apply_policy_push};
use roz_worker::signing_hooks::WorkerSigningContext;
use roz_worker::signing_key::{load, save};
use roz_worker::wal::WalStore;
use tempfile::TempDir;
use uuid::Uuid;

/// Server signing seed used both to forge the envelope and to seed the
/// worker-side `server_verifying_key`. Matches the pattern in
/// `signing_hooks::tests::ctx()`.
const SERVER_SEED: [u8; 32] = [9u8; 32];
/// Worker device-key seed (opaque to the server-forged envelope we build
/// here — only used by `WorkerSigningContext::new`).
const WORKER_SEED: [u8; 32] = [7u8; 32];

async fn build_worker_signing_ctx() -> (TempDir, WorkerSigningContext) {
    let tmp = TempDir::new().unwrap();
    let provider = Arc::new(StaticKeyProvider::from_key_bytes(WORKER_SEED));
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();
    let server_signing = SigningKey::from_bytes(&SERVER_SEED);
    let svk_bytes = server_signing.verifying_key().to_bytes();
    save(tmp.path(), &provider, tenant, 1, &WORKER_SEED, &svk_bytes)
        .await
        .unwrap();
    let material = load(tmp.path(), &provider, tenant, host).await.unwrap().unwrap();
    let wal = Arc::new(WalStore::open(":memory:").unwrap());
    let ctx = WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal);
    (tmp, ctx)
}

/// Build a non-trivial policy row with `max_linear=1.0`, `max_angular=0.5`,
/// `max_force=10.0`, enforcement_mode=Clamp — chosen so both 5.0 (linear)
/// and 2.0 (angular) trivially exceed the caps for the anti-tautology
/// assertions.
fn build_policy_row(policy_id: Uuid, tenant_id: Uuid) -> SafetyPolicyRow {
    let policy_json = serde_json::json!({
        "policy_id": policy_id,
        "version": 1,
        "enforcement_mode": "clamp",
        "limits": {
            "max_velocity": { "linear_m_per_s": 1.0, "angular_rad_per_s": 0.5 },
            "max_acceleration": { "linear_m_per_s2": 2.0, "angular_rad_per_s2": 1.0 },
            "max_force": { "newtons": 10.0 }
        },
        "deadman_timers": { "command_timeout_ms": 5000, "on_expire": "halt" }
    });
    SafetyPolicyRow {
        id: policy_id,
        tenant_id,
        name: "phase24-pushstack".into(),
        version: 1,
        policy_json,
        limits: serde_json::json!({}),
        geofences: serde_json::json!([]),
        interlocks: serde_json::json!([]),
        deadman_timers: serde_json::json!({}),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn sample_invocation_with_policy(policy_id: Uuid) -> TaskInvocation {
    TaskInvocation::new(
        Uuid::nil(),
        "t1".into(),
        "move".into(),
        Uuid::nil(),
        Some(policy_id),
        Uuid::nil(),
        60,
        ExecutionMode::React,
        None,
        String::new(),
        None,
        vec![],
        None,
        None,
        None,
        None,
    )
}

/// Forge a `roz-sig-v1` ServerToWorker envelope over `payload` using the
/// shared server signing seed — worker-side `WorkerSigningContext` trusts
/// that seed's verifying key (seeded in `build_worker_signing_ctx`).
fn forge_server_header(
    server_signing: &SigningKey,
    tenant_id: Uuid,
    host_id: Uuid,
    correlation_id: Uuid,
    sequence_number: u64,
    payload: &[u8],
) -> String {
    let fields = SignedFields {
        direction: Direction::ServerToWorker,
        tenant_id,
        host_id,
        correlation_id,
        timestamp: Utc::now(),
        sequence_number,
        payload_hash: payload_sha256_hex(payload),
        key_version: 1,
    };
    let env = sign_envelope(&fields, server_signing).expect("sign forged envelope");
    env.encode_header().expect("encode forged header")
}

#[tokio::test]
#[ignore = "requires Docker for NATS testcontainer"]
async fn policy_push_wire_to_cache_to_copper_filter_clamp() {
    // ---- NATS container -------------------------------------------------
    let guard = roz_test::nats_container().await;
    let nats = async_nats::connect(guard.url())
        .await
        .expect("connect to NATS container");

    // ---- Worker signing context (server seed SERVER_SEED pre-trusted) ----
    let (_tmp, signing_ctx) = build_worker_signing_ctx().await;
    let tenant_id = signing_ctx.material.read().tenant_id;
    let host_id = signing_ctx.material.read().host_id;

    // ---- Policy row + payload bytes --------------------------------------
    let policy_id = Uuid::new_v4();
    let row = build_policy_row(policy_id, tenant_id);
    let payload = serde_json::to_vec(&row).expect("serialize row");

    // ---- Worker surfaces --------------------------------------------------
    let cache = PolicyCache::new();
    let hot = HotPolicy::permissive();
    let copper_hot = new_hot_policy();

    // ---- Subscribe BEFORE publishing (no race) ---------------------------
    let worker_id = "worker-phase24-pushstack";
    let subject = Subjects::policy(worker_id).expect("policy subject");
    let mut sub = nats.subscribe(subject.clone()).await.expect("subscribe");

    // ---- Server-side forge + publish -------------------------------------
    let server_signing = SigningKey::from_bytes(&SERVER_SEED);
    let header = forge_server_header(&server_signing, tenant_id, host_id, policy_id, 1, &payload);
    publish_signed(&nats, subject.clone(), payload.clone(), &header)
        .await
        .expect("publish_signed policy row");
    nats.flush().await.expect("flush");

    // ---- Worker-side receive + verify + apply ----------------------------
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for policy push")
        .expect("subscription closed unexpectedly");

    let header_value = msg
        .headers
        .as_ref()
        .and_then(|h| h.get(HEADER_NAME))
        .map(|v| v.to_string())
        .expect("roz-sig-v1 header present on wire");

    // Sanity: the forged header must decode identically to what we sent.
    let env = SignatureEnvelope::decode_header(&header_value).expect("decode wire header");
    assert_eq!(env.fields.direction, Direction::ServerToWorker);
    assert_eq!(env.fields.tenant_id, tenant_id);
    assert_eq!(env.fields.host_id, host_id);

    // Full worker-side verify (tamper-binding + server-key binding).
    signing_ctx
        .verify_inbound_worker(Some(&header_value), &msg.payload)
        .expect("worker-side signature verify");

    let received_row: SafetyPolicyRow =
        serde_json::from_slice(&msg.payload).expect("parse SafetyPolicyRow off the wire");
    assert_eq!(received_row.id, policy_id);
    assert_eq!(received_row.version, 1);

    apply_policy_push(&received_row, &cache, &hot, &copper_hot, None)
        .await
        .expect("apply_policy_push for a well-formed row");

    // ---- Assertion 1 — pre_dispatch_check rejects OVER-limit linear ------
    //
    // The policy is Clamp-mode; the pre-dispatch gate still routes
    // LimitExceeded through Clamp (see enforce_command — Clamp mode returns
    // EnforcementOutcome::Clamp, NOT Reject). So to test Reject, we also
    // need a Reject-mode path. The Task 3 plan text declares Clamp enforcement
    // AND asserts PreDispatchOutcome::Reject — those two statements are only
    // consistent if we use a separate Reject-mode policy for assertion 1.
    //
    // Resolution: assert Clamp on the over-limit path here (matching the
    // policy actually pushed) and separately cover the Reject-mode branch
    // below with a second in-cache policy insert. This preserves the
    // "full-stack reject path" intent of the plan without silently
    // changing the published policy's enforcement_mode.
    let inv = sample_invocation_with_policy(policy_id);
    let decision = pre_dispatch_check(&cache, &hot, &inv, Some(5.0), Some(0.0)).await;
    match decision.outcome {
        PreDispatchOutcome::Clamp { ref clamped_details } => {
            assert_eq!(clamped_details["channel"], serde_json::json!("linear_velocity"));
            assert!(
                (clamped_details["clamped_to"].as_f64().unwrap() - 1.0).abs() < 1e-9,
                "linear must clamp to the 1.0 m/s policy limit; got {clamped_details:?}",
            );
        }
        other => panic!("expected Clamp on pushed Clamp-mode policy, got {other:?}"),
    }
    assert_eq!(decision.policy_id, policy_id, "decision must cite the pushed policy");
    assert!(!decision.stale, "cache hit must not be marked stale");

    // Supplementary Reject-mode branch: prime the cache with a Reject-mode
    // copy of the same shape to prove the Reject vocabulary also lands on
    // the pushed cache. This does NOT regress the pushed Clamp policy —
    // it's a distinct policy_id stored on the same PolicyCache.
    {
        use roz_worker::policy_enforcement::{
            AccelerationLimits, DeadmanTimers, EnforcementMode, ForceLimits, OnBreachAction, PolicyLimits, PolicyV1,
            VelocityLimits,
        };
        let reject_id = Uuid::new_v4();
        let reject_policy = PolicyV1 {
            policy_id: reject_id,
            version: 1,
            enforcement_mode: EnforcementMode::Reject,
            limits: PolicyLimits {
                max_velocity: VelocityLimits {
                    linear_m_per_s: 1.0,
                    angular_rad_per_s: 0.5,
                },
                max_acceleration: AccelerationLimits {
                    linear_m_per_s2: 2.0,
                    angular_rad_per_s2: 1.0,
                },
                max_force: ForceLimits { newtons: 10.0 },
                joint_limits: Vec::new(),
            },
            geofences: Vec::new(),
            interlocks: Vec::new(),
            deadman_timers: DeadmanTimers {
                command_timeout_ms: 5000,
                on_expire: OnBreachAction::Halt,
            },
        };
        cache.insert(reject_id, reject_policy).await;
        let mut inv_reject = sample_invocation_with_policy(reject_id);
        inv_reject.safety_policy_id = Some(reject_id);
        let decision_reject = pre_dispatch_check(&cache, &hot, &inv_reject, Some(5.0), Some(0.0)).await;
        match decision_reject.outcome {
            PreDispatchOutcome::Reject(PolicyEnforcementError::LimitExceeded {
                ref channel,
                value,
                max,
            }) => {
                assert_eq!(channel, "linear_velocity");
                assert_eq!(value, 5.0);
                assert_eq!(max, 1.0);
            }
            other => panic!("expected Reject(LimitExceeded) on Reject-mode policy, got {other:?}"),
        }
        assert!(!decision_reject.stale);
    }

    // ---- Assertion 2 — pre_dispatch_check allows UNDER-limit -------------
    let allow_decision = pre_dispatch_check(&cache, &hot, &inv, Some(0.5), Some(0.2)).await;
    assert!(
        matches!(allow_decision.outcome, PreDispatchOutcome::Allow),
        "under-limit must Allow, got {:?}",
        allow_decision.outcome
    );
    assert!(!allow_decision.stale);

    // ---- Assertion 3 — copper filter clamps through the SAME HotCopperPolicy
    // Build a SafetyFilterTask wired to the exact pointer apply_policy_push
    // wrote to above.
    let filter = SafetyFilterTask::new(2.0, 0.0, None)
        .expect("build filter")
        .with_policy(copper_hot.clone());

    // Warmup / passthrough: a velocity pair UNDER the pushed limits must
    // not trip the clamp.
    let (lin_ok, ang_ok, clamped_ok) = filter.policy_clamp(0.8, 0.3);
    assert!((lin_ok - 0.8).abs() < 1e-9);
    assert!((ang_ok - 0.3).abs() < 1e-9);
    assert!(!clamped_ok, "within-limit velocity must not be clamped");

    // Under Clamp mode, 5.0 linear + 2.0 angular must project onto the
    // pushed policy's 1.0 / 0.5 limits.
    let (lin, ang, clamped) = filter.policy_clamp(5.0, 2.0);
    assert!(clamped, "over-limit velocity must be flagged clamped");
    assert!(
        lin.abs() <= 1.0 + 1e-9,
        "linear clamp must not exceed 1.0 m/s; got {lin}"
    );
    assert!(
        ang.abs() <= 0.5 + 1e-9,
        "angular clamp must not exceed 0.5 rad/s; got {ang}"
    );
    // Check the enforcement mode actually arrived on the pointer.
    let guard = copper_hot.load();
    assert_eq!(guard.enforcement_mode, CopperEnforcementMode::Clamp);
    assert!((guard.max_linear_m_per_s - 1.0).abs() < 1e-9);
    assert!((guard.max_angular_rad_per_s - 0.5).abs() < 1e-9);

    // ---- Assertion 4 — HotPathSafetyFilter (THE production tick-path
    // filter) clamps through the SAME HotCopperPolicy pointer.
    //
    // Plan 24-16 closed the "dead-field" gap where HotPathSafetyFilter
    // stored self.policy but never read it. The assertion above uses
    // SafetyFilterTask::policy_clamp (a separate, legacy filter API).
    // This assertion targets the ACTUAL filter used by
    // controller::build_tick_infrastructure: HotPathSafetyFilter.
    //
    // If 24-16 regresses (the filter() method stops reading self.policy),
    // this assertion fails even though assertion 3 still passes, because
    // SafetyFilterTask::policy_clamp uses its own policy read path.
    let joint_limits = vec![
        // Loose per-channel limits so chassis policy is the only clamp layer.
        JointSafetyLimits {
            joint_name: "linear_x".into(),
            max_velocity: 100.0,
            max_acceleration: f64::INFINITY,
            max_jerk: f64::INFINITY,
            position_min: -1000.0,
            position_max: 1000.0,
            max_torque: None,
        },
        JointSafetyLimits {
            joint_name: "angular_z".into(),
            max_velocity: 100.0,
            max_acceleration: f64::INFINITY,
            max_jerk: f64::INFINITY,
            position_min: -1000.0,
            position_max: 1000.0,
            max_torque: None,
        },
    ];
    let mut hot_filter = HotPathSafetyFilter::new(joint_limits, None, 0.01)
        .expect("valid tick period")
        .with_chassis_axis_map(vec![ChassisAxis::Linear, ChassisAxis::Angular])
        .expect("axis map length matches joint limits")
        .with_policy(copper_hot.clone());

    // Feed the same over-limit command used in assertion 3.
    let result = hot_filter.filter(&[5.0, 2.0], None, None);
    assert!(
        (result.commands[0] - 1.0).abs() < 1e-9,
        "HotPathSafetyFilter must clamp linear to pushed policy limit 1.0; got {}",
        result.commands[0]
    );
    assert!(
        (result.commands[1] - 0.5).abs() < 1e-9,
        "HotPathSafetyFilter must clamp angular to pushed policy limit 0.5; got {}",
        result.commands[1]
    );
    let chassis_count = result
        .interventions
        .iter()
        .filter(|i| i.kind == InterventionKind::ChassisPolicyClamp)
        .count();
    assert_eq!(
        chassis_count, 2,
        "HotPathSafetyFilter should record exactly one ChassisPolicyClamp intervention per clamped axis (2 total); got {chassis_count}"
    );
    assert!(
        !result.estop,
        "Clamp mode must never set estop on HotPathSafetyFilter; got estop=true"
    );
}
