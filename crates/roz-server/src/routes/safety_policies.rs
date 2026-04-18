use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use roz_nats::Subjects;
use roz_nats::dispatch::publish_signed;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::extractors::pagination::ValidatedPagination;
use crate::middleware::tx::Tx;
use crate::signing_gate::SigningGate;

#[derive(Deserialize)]
pub struct CreatePolicyRequest {
    pub name: String,
    #[serde(default)]
    pub policy_json: serde_json::Value,
    #[serde(default)]
    pub limits: serde_json::Value,
    #[serde(default)]
    pub geofences: serde_json::Value,
    #[serde(default)]
    pub interlocks: serde_json::Value,
    #[serde(default)]
    pub deadman_timers: serde_json::Value,
}

#[derive(Deserialize)]
pub struct UpdatePolicyRequest {
    pub policy_json: Option<serde_json::Value>,
    pub limits: Option<serde_json::Value>,
    pub geofences: Option<serde_json::Value>,
    pub interlocks: Option<serde_json::Value>,
    pub deadman_timers: Option<serde_json::Value>,
}

/// POST /v1/safety-policies
pub async fn create(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let policy = roz_db::safety_policies::create(
        &mut **tx,
        tenant_id,
        &body.name,
        &body.policy_json,
        &body.limits,
        &body.geofences,
        &body.interlocks,
        &body.deadman_timers,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"data": policy}))))
}

/// GET /v1/safety-policies
pub async fn list(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    pagination: ValidatedPagination,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let policies = roz_db::safety_policies::list(&mut **tx, tenant_id, pagination.limit, pagination.offset).await?;
    Ok(Json(json!({"data": policies})))
}

/// GET /v1/safety-policies/:id
pub async fn get(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let policy = roz_db::safety_policies::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("safety policy not found"))?;
    if policy.tenant_id != tenant_id {
        return Err(AppError::not_found("safety policy not found"));
    }
    Ok(Json(json!({"data": policy})))
}

/// PUT /v1/safety-policies/:id
pub async fn update(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdatePolicyRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::safety_policies::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("safety policy not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("safety policy not found"));
    }
    let policy = roz_db::safety_policies::update(
        &mut **tx,
        id,
        body.policy_json.as_ref(),
        body.limits.as_ref(),
        body.geofences.as_ref(),
        body.interlocks.as_ref(),
        body.deadman_timers.as_ref(),
    )
    .await?
    .ok_or_else(|| AppError::not_found("safety policy not found"))?;
    Ok(Json(json!({"data": policy})))
}

/// DELETE /v1/safety-policies/:id
pub async fn delete(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let policy = roz_db::safety_policies::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("safety policy not found"))?;
    if policy.tenant_id != tenant_id {
        return Err(AppError::not_found("safety policy not found"));
    }
    roz_db::safety_policies::delete(&mut **tx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ===========================================================================
// Signed policy push (FS-01, D-04)
// ===========================================================================

/// All failure modes surfaced by [`publish_policy_to_workers`].
#[derive(Debug, thiserror::Error)]
pub enum PublishPolicyError {
    /// The policy row could not be serialized for transport.
    #[error("serialize policy row: {0}")]
    Serialize(#[from] serde_json::Error),
    /// Caller supplied workers to fan out to but no NATS client is configured.
    /// Operator reconciles by re-CRUD once the transport is reachable (D-04).
    #[error("NATS client not configured; policy push unavailable")]
    NatsClientMissing,
    /// Caller supplied workers to fan out to but no SigningGate is available.
    /// Every policy push MUST be signed (D-12) — no unsigned fallback.
    #[error("SigningGate not configured; policy push cannot be signed")]
    SigningGateMissing,
}

/// Publish a [`roz_db::safety_policies::SafetyPolicyRow`] to each bound worker
/// via the Phase 23 signed envelope.
///
/// - Subject: `roz.policy.{worker_id}` (FS-01 D-04, subject builder
///   `Subjects::policy` added in Plan 24-01).
/// - Signing: [`SigningGate::sign_outbound`] with `direction=ServerToWorker`
///   (D-12). There is no unsigned fallback.
///
/// The helper is idempotent per worker — duplicate pushes are suppressed at
/// the worker cache layer (30 s TTL + version check, added in Plan 24-02's
/// `PolicyCache`).
///
/// Per-worker failures are logged via `tracing::warn!` but do NOT abort the
/// overall fan-out; the operator reconciles stragglers via re-CRUD or the
/// pull-at-task-start path (D-04).
///
/// This helper is deliberately NOT yet wired into the `create` / `update`
/// handlers — that lands in Plan 24-05 Task 4 (deferred).
///
/// # Errors
///
/// - [`PublishPolicyError::Serialize`] when the row cannot be serialized.
/// - [`PublishPolicyError::NatsClientMissing`] / [`PublishPolicyError::SigningGateMissing`]
///   when callers supply workers to fan out to but the transport / signer
///   dependency is absent.
#[tracing::instrument(
    level = "info",
    skip(nats, gate, policy),
    fields(policy_id = %policy.id, tenant_id = %policy.tenant_id, worker_count = worker_ids.len())
)]
pub async fn publish_policy_to_workers(
    nats: Option<&async_nats::Client>,
    gate: Option<&SigningGate>,
    policy: &roz_db::safety_policies::SafetyPolicyRow,
    worker_ids: &[(Uuid, String)],
) -> Result<(), PublishPolicyError> {
    if worker_ids.is_empty() {
        tracing::debug!("no workers bound to tenant; policy push skipped");
        return Ok(());
    }

    let nats = nats.ok_or(PublishPolicyError::NatsClientMissing)?;
    let gate = gate.ok_or(PublishPolicyError::SigningGateMissing)?;

    let payload = serde_json::to_vec(policy)?;
    let tenant_id = policy.tenant_id;

    for (host_id, worker_id_str) in worker_ids {
        let subject = match Subjects::policy(worker_id_str) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(worker_id = %worker_id_str, error = %e, "invalid worker_id; skipping policy push");
                continue;
            }
        };
        let header = match gate.sign_outbound(tenant_id, *host_id, policy.id, &payload).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    worker_id = %worker_id_str,
                    error = %e,
                    "failed to sign policy push; skipping worker"
                );
                continue;
            }
        };
        if let Err(e) = publish_signed(nats, subject, payload.clone(), &header).await {
            tracing::warn!(
                worker_id = %worker_id_str,
                error = %e,
                "failed to publish policy to worker; operator must re-CRUD"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod publish_policy_tests {
    use super::*;
    use uuid::Uuid;

    fn sample_policy_row(tenant_id: Uuid) -> roz_db::safety_policies::SafetyPolicyRow {
        roz_db::safety_policies::SafetyPolicyRow {
            id: Uuid::new_v4(),
            tenant_id,
            name: "test-policy".into(),
            version: 1,
            policy_json: serde_json::json!({"policy_id": Uuid::new_v4(), "version": 1}),
            limits: serde_json::json!({}),
            geofences: serde_json::json!([]),
            interlocks: serde_json::json!([]),
            deadman_timers: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// Empty worker slice short-circuits before touching the NATS client. This
    /// makes the unit test runnable with a `None` client — the full signed-push
    /// integration test lives in Plan 24-05 where a testcontainers NATS + a
    /// provisioned `SigningGate` are already on hand.
    #[tokio::test]
    async fn publish_policy_to_workers_skips_when_no_workers() {
        let tenant = Uuid::new_v4();
        let row = sample_policy_row(tenant);
        let worker_ids: Vec<(Uuid, String)> = Vec::new();
        let result = publish_policy_to_workers(None, None, &row, &worker_ids).await;
        assert!(
            matches!(result, Ok(())),
            "empty worker list must short-circuit with Ok, got {result:?}"
        );
    }

    /// When NATS is configured out (None) and there IS a worker to fan out to,
    /// the helper should surface a structured error rather than panic.
    #[tokio::test]
    async fn publish_policy_to_workers_errors_when_nats_missing_but_workers_present() {
        let tenant = Uuid::new_v4();
        let row = sample_policy_row(tenant);
        let worker_ids = vec![(Uuid::new_v4(), "worker-abc".to_string())];
        let result = publish_policy_to_workers(None, None, &row, &worker_ids).await;
        assert!(matches!(result, Err(PublishPolicyError::NatsClientMissing)));
    }
}
