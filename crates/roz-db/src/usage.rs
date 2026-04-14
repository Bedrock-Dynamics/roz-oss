//! Usage metering — thin wrapper around the `record_usage()` PL/pgSQL
//! function defined in migration `20260408026_record_usage_invoker.sql`.
//!
//! The function is idempotent on `idempotency_key` and also updates the
//! caller's monthly `roz_billing_periods` row in the same transaction.

use sqlx::PgPool;
use sqlx::types::Uuid;

/// Record a single billable media-analysis event.
///
/// `resource_type` is a short opaque slug (e.g. `"media_analysis"`);
/// `model` should be the backend/model identifier (e.g. `"gemini-2.5-pro"`);
/// `idempotency_key` must be unique per event — a UUID4 is fine.
///
/// `session_id` is `None` for RPCs that are not session-scoped (e.g.
/// `AnalyzeMedia` is standalone).
///
/// v1 records `internal_cost = 0` (pricing wiring is a separate follow-up);
/// the function still records the event with token counts so billing can
/// backfill pricing from the events table. Cache tokens are `None` (Gemini
/// streaming does not expose cache-hit metrics today).
///
/// Errors propagate from sqlx (connection / constraint / function failures).
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the record_usage PL/pgSQL signature 1:1"
)]
pub async fn record_media_usage(
    pool: &PgPool,
    tenant_id: Uuid,
    session_id: Option<Uuid>,
    resource_type: &str,
    model: Option<&str>,
    quantity: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    idempotency_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT record_usage($1, $2, $3, $4, $5, $6, $7, NULL::BIGINT, NULL::BIGINT, 0::NUMERIC, $8)")
        .bind(tenant_id)
        .bind(session_id)
        .bind(resource_type)
        .bind(model)
        .bind(quantity)
        .bind(input_tokens)
        .bind(output_tokens)
        .bind(idempotency_key)
        .execute(pool)
        .await?;
    Ok(())
}
