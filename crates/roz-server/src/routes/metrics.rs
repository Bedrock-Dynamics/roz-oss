use axum::Extension;
use axum::Json;
use roz_core::auth::AuthIdentity;
use serde::Serialize;
use serde_json::json;

use crate::error::AppError;
use crate::middleware::tx::Tx;

#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(clippy::struct_field_names)]
pub struct TaskMetrics {
    pub pending_count: i64,
    pub queued_count: i64,
    pub running_count: i64,
    pub succeeded_count: i64,
    pub failed_count: i64,
    pub timed_out_count: i64,
    pub cancelled_count: i64,
    pub safety_stop_count: i64,
    pub total_count: i64,
    pub avg_duration_secs: Option<f64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(clippy::struct_field_names)]
pub struct HostMetrics {
    pub total_count: i64,
    pub online_count: i64,
    pub offline_count: i64,
}

/// GET /v1/metrics/tasks
pub async fn task_metrics(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();

    let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64, i64, Option<f64>)>(
        "SELECT \
             COUNT(*) FILTER (WHERE status = 'pending'), \
             COUNT(*) FILTER (WHERE status = 'queued'), \
             COUNT(*) FILTER (WHERE status = 'running'), \
             COUNT(*) FILTER (WHERE status = 'succeeded'), \
             COUNT(*) FILTER (WHERE status = 'failed'), \
             COUNT(*) FILTER (WHERE status = 'timed_out'), \
             COUNT(*) FILTER (WHERE status = 'cancelled'), \
             COUNT(*) FILTER (WHERE status = 'safety_stop'), \
             COUNT(*), \
             (AVG(EXTRACT(EPOCH FROM (updated_at - created_at))) \
                 FILTER (WHERE status IN ('succeeded', 'failed', 'timed_out', 'cancelled', 'safety_stop')))::float8 \
         FROM roz_tasks \
         WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&mut **tx)
    .await?;

    let metrics = TaskMetrics {
        pending_count: row.0,
        queued_count: row.1,
        running_count: row.2,
        succeeded_count: row.3,
        failed_count: row.4,
        timed_out_count: row.5,
        cancelled_count: row.6,
        safety_stop_count: row.7,
        total_count: row.8,
        avg_duration_secs: row.9,
    };

    Ok(Json(json!({ "data": metrics })))
}

/// GET /v1/metrics/hosts
pub async fn host_metrics(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();

    let metrics = sqlx::query_as::<_, HostMetrics>(
        "SELECT \
             COUNT(*) AS total_count, \
             COUNT(*) FILTER (WHERE status = 'online') AS online_count, \
             COUNT(*) FILTER (WHERE status != 'online') AS offline_count \
         FROM roz_hosts \
         WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&mut **tx)
    .await?;

    Ok(Json(json!({ "data": metrics })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_metrics_serializes_all_fields() {
        let m = TaskMetrics {
            pending_count: 5,
            queued_count: 1,
            running_count: 3,
            succeeded_count: 10,
            failed_count: 2,
            timed_out_count: 4,
            cancelled_count: 1,
            safety_stop_count: 2,
            total_count: 20,
            avg_duration_secs: Some(45.5),
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["pending_count"], 5);
        assert_eq!(json["queued_count"], 1);
        assert_eq!(json["running_count"], 3);
        assert_eq!(json["succeeded_count"], 10);
        assert_eq!(json["failed_count"], 2);
        assert_eq!(json["timed_out_count"], 4);
        assert_eq!(json["cancelled_count"], 1);
        assert_eq!(json["safety_stop_count"], 2);
        assert_eq!(json["total_count"], 20);
        assert_eq!(json["avg_duration_secs"], 45.5);
    }

    #[test]
    fn task_metrics_handles_null_avg() {
        let m = TaskMetrics {
            pending_count: 0,
            queued_count: 0,
            running_count: 0,
            succeeded_count: 0,
            failed_count: 0,
            timed_out_count: 0,
            cancelled_count: 0,
            safety_stop_count: 0,
            total_count: 0,
            avg_duration_secs: None,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert!(json["avg_duration_secs"].is_null());
    }

    #[test]
    fn host_metrics_serializes_all_fields() {
        let m = HostMetrics {
            total_count: 8,
            online_count: 6,
            offline_count: 2,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["total_count"], 8);
        assert_eq!(json["online_count"], 6);
        assert_eq!(json["offline_count"], 2);
    }
}
