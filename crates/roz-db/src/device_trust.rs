//! Device trust loader (ENF-01).
//!
//! Read-only helper over `roz_device_trust` (migration 011). The server-side
//! trust gate calls `get_by_host_id` before dispatching tasks. The loader is
//! intentionally RLS-independent: it queries by `host_id` alone so the
//! caller (`crates/roz-server/src/trust.rs`) can apply an explicit tenant
//! check as defense-in-depth beyond RLS (Pitfall 3).

use chrono::{DateTime, Utc};
use roz_core::device_trust::{DeviceTrust, DeviceTrustPosture, FirmwareManifest};
use uuid::Uuid;

/// Load the `roz_device_trust` row for the given host, if any.
///
/// Returns `Ok(None)` when no row exists (fail-closed at the caller).
/// The `firmware` JSONB column is decoded into `FirmwareManifest`; malformed
/// JSON surfaces as `sqlx::Error::ColumnDecode`, which callers MUST treat as
/// rejection.
pub async fn get_by_host_id<'e, E>(executor: E, host_id: Uuid) -> Result<Option<DeviceTrust>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    type Row = (
        Uuid,                      // tenant_id
        Uuid,                      // host_id
        String,                    // posture
        Option<serde_json::Value>, // firmware JSONB
        Option<String>,            // sbom_hash
        Option<DateTime<Utc>>,     // last_attestation
        DateTime<Utc>,             // created_at
        DateTime<Utc>,             // updated_at
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT tenant_id, host_id, posture, firmware, sbom_hash, last_attestation, created_at, updated_at \
         FROM roz_device_trust WHERE host_id = $1",
    )
    .bind(host_id)
    .fetch_optional(executor)
    .await?;

    let Some((tenant_id, host_id, posture, firmware, sbom_hash, last_attestation, created_at, updated_at)) = row else {
        return Ok(None);
    };

    let posture = match posture.as_str() {
        "trusted" => DeviceTrustPosture::Trusted,
        "provisional" => DeviceTrustPosture::Provisional,
        _ => DeviceTrustPosture::Untrusted,
    };

    let firmware = firmware
        .map(serde_json::from_value::<FirmwareManifest>)
        .transpose()
        .map_err(|e| sqlx::Error::ColumnDecode {
            index: "firmware".into(),
            source: Box::new(e),
        })?;

    Ok(Some(DeviceTrust {
        host_id,
        tenant_id: tenant_id.to_string(),
        posture,
        firmware,
        sbom_hash,
        last_attestation,
        created_at,
        updated_at,
    }))
}
