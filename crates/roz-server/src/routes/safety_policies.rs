use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::extractors::pagination::ValidatedPagination;
use crate::middleware::tx::Tx;

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
// Signed policy push (FS-01, D-04) — TDD RED stub for Plan 24-02 Task 3.
// Implementation lands in the GREEN commit.
// ===========================================================================

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
