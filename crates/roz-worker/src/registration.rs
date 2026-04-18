//! Worker auto-registration with the roz API server.
//!
//! On startup the worker calls the server's REST API to ensure a host record
//! exists for its `worker_id`, captures the host's `tenant_id` from the
//! response body, and sets its status to `online`. The returned
//! `(host_id, tenant_id)` pair is consumed downstream by
//! [`bootstrap_device_key`] for Phase 23 signed-dispatch enrollment.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use roz_core::key_provider::StaticKeyProvider;
use uuid::Uuid;

use crate::signing_key::{self, SigningKeyMaterial};

/// Identity of the registered host as returned by the server.
#[derive(Debug, Clone, Copy)]
pub struct HostIdentity {
    pub host_id: Uuid,
    pub tenant_id: Uuid,
}

/// Register the worker as a host with the server, returning the host UUID
/// and the tenant UUID it belongs to.
///
/// 1. `GET /v1/hosts` — look for a host whose `name` matches `worker_id`.
/// 2. If found: `PATCH /v1/hosts/{id}/status` with `{"status": "online"}`, return identity.
/// 3. If not found: `POST /v1/hosts` with `{"name": worker_id, "host_type": "edge"}`,
///    then `PATCH` status to `online`, return identity.
///
/// The `tenant_id` field is required downstream by Phase 23 signed-dispatch
/// (`roz-sig-v1` envelopes carry `tenant_id` as a signed field); capturing it
/// at registration avoids a second round-trip.
pub async fn register_host(
    client: &reqwest::Client,
    api_url: &str,
    api_key: &str,
    worker_id: &str,
) -> Result<HostIdentity> {
    let base = api_url.trim_end_matches('/');

    // 1. List hosts and find matching name (paginated)
    let existing = find_host_paginated(client, base, api_key, worker_id).await?;

    let identity = if let Some(id) = existing {
        id
    } else {
        // 3. Create host
        let create_body = serde_json::json!({
            "name": worker_id,
            "host_type": "edge",
        });
        let resp = client
            .post(format!("{base}/v1/hosts"))
            .bearer_auth(api_key)
            .json(&create_body)
            .send()
            .await
            .context("POST /v1/hosts request failed")?;

        // Handle 409 Conflict: another worker registered the same name concurrently.
        if resp.status() == reqwest::StatusCode::CONFLICT {
            find_host_paginated(client, base, api_key, worker_id)
                .await?
                .context("host not found after conflict retry")?
        } else {
            let resp = resp
                .error_for_status()
                .context("POST /v1/hosts returned error status")?;
            let body: serde_json::Value = resp.json().await.context("failed to parse POST /v1/hosts response")?;
            parse_host_identity(&body).context("POST /v1/hosts response missing host id or tenant_id")?
        }
    };

    // Set status to online
    let status_body = serde_json::json!({"status": "online"});
    client
        .patch(format!("{base}/v1/hosts/{}/status", identity.host_id))
        .bearer_auth(api_key)
        .json(&status_body)
        .send()
        .await
        .context("PATCH host status request failed")?
        .error_for_status()
        .context("PATCH host status returned error status")?;

    Ok(identity)
}

/// Bootstrap the worker's Phase 23 device key material.
///
/// Called after [`register_host`] returns the `(host_id, tenant_id)` pair.
/// On first run, calls `POST /v1/device/provision-key` to mint a fresh
/// Ed25519 keypair and persists the returned seed + server verifying key
/// locally. On subsequent runs, loads the existing key from disk.
///
/// Additionally performs a D-07 age check: if the active key is older than
/// 90 days, calls `POST /v1/device/rotate-key` to rotate it. Rotation
/// failures are logged but non-fatal — the current key remains valid for
/// up to 24 h of overlap window.
///
/// The returned [`SigningKeyMaterial`] is consumed by Plan 23-08 when
/// wiring signing into every worker publish site.
pub async fn bootstrap_device_key(
    http: &reqwest::Client,
    api_url: &str,
    api_key: &str,
    key_provider: &Arc<StaticKeyProvider>,
    identity: HostIdentity,
    dir: &Path,
) -> Result<SigningKeyMaterial> {
    let current = signing_key::load_or_enroll(
        dir,
        http,
        api_url,
        api_key,
        key_provider,
        identity.tenant_id,
        identity.host_id,
    )
    .await
    .context("device key enrollment failed")?;

    // D-07: 90-day age check + rotate.
    match signing_key::rotate_if_due(&current, dir, http, api_url, key_provider).await {
        Ok(Some(new_mat)) => {
            tracing::info!(
                old_version = current.key_version,
                new_version = new_mat.key_version,
                host_id = %identity.host_id,
                "device key rotated (age > 90d)"
            );
            Ok(new_mat)
        }
        Ok(None) => Ok(current),
        Err(e) => {
            tracing::error!(err = %e, "rotate-if-due failed; keeping current key");
            Ok(current)
        }
    }
}

/// Paginate through `GET /v1/hosts` looking for a host whose `name` matches `worker_id`.
async fn find_host_paginated(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    worker_id: &str,
) -> Result<Option<HostIdentity>> {
    const MAX_PAGES: usize = 200; // 200 * 50 = 10 000 hosts
    let limit: usize = 50;
    let mut offset: usize = 0;
    let mut pages_fetched: usize = 0;
    loop {
        if pages_fetched >= MAX_PAGES {
            tracing::warn!("find_host_paginated hit max page limit without finding host");
            break;
        }
        pages_fetched += 1;
        let resp = client
            .get(format!("{base}/v1/hosts?limit={limit}&offset={offset}"))
            .bearer_auth(api_key)
            .send()
            .await
            .context("GET /v1/hosts request failed")?
            .error_for_status()
            .context("GET /v1/hosts returned error status")?;

        let body: serde_json::Value = resp.json().await.context("failed to parse GET /v1/hosts response")?;

        if let Some(identity) = find_host_by_name(&body, worker_id) {
            return Ok(Some(identity));
        }

        // Check if we've exhausted the last page.
        let page_len = body
            .get("data")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        if page_len < limit {
            break; // last page
        }
        offset += limit;
    }
    Ok(None)
}

/// Search the `{"data": [...]}` response for a host whose `name` matches `worker_id`.
fn find_host_by_name(body: &serde_json::Value, worker_id: &str) -> Option<HostIdentity> {
    let hosts = body.get("data")?.as_array()?;
    for host in hosts {
        if host.get("name")?.as_str()? == worker_id {
            let id_str = host.get("id")?.as_str()?;
            let tenant_str = host.get("tenant_id")?.as_str()?;
            return Some(HostIdentity {
                host_id: Uuid::parse_str(id_str).ok()?,
                tenant_id: Uuid::parse_str(tenant_str).ok()?,
            });
        }
    }
    None
}

/// Build the JSON body for `PUT /v1/hosts/{id}/embodiment`.
///
/// Generic over `Serialize` for testability — production callers pass
/// `&EmbodimentModel` / `&EmbodimentRuntime`; tests can pass `serde_json::Value`.
pub(crate) fn build_embodiment_body(
    model: &impl serde::Serialize,
    runtime: Option<&impl serde::Serialize>,
) -> Result<serde_json::Value> {
    let mut body = serde_json::json!({ "model": model });
    if let Some(rt) = runtime {
        body["runtime"] = serde_json::to_value(rt).context("failed to serialize embodiment runtime")?;
    }
    Ok(body)
}

/// Upload embodiment model (and optional runtime) to the server for a registered host.
///
/// Called after `register_host()` returns the host UUID, when the worker has
/// embodiment data available. Sends `PUT /v1/hosts/{id}/embodiment` with the
/// serialised model and optional runtime as JSON.
pub async fn upload_embodiment(
    client: &reqwest::Client,
    api_url: &str,
    api_key: &str,
    host_id: Uuid,
    model: &roz_core::embodiment::model::EmbodimentModel,
    runtime: Option<&roz_core::embodiment::embodiment_runtime::EmbodimentRuntime>,
) -> Result<()> {
    let base = api_url.trim_end_matches('/');
    let body = build_embodiment_body(model, runtime)?;

    client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .context(format!("PUT /v1/hosts/{host_id}/embodiment request failed"))?
        .error_for_status()
        .context(format!("PUT /v1/hosts/{host_id}/embodiment returned error status"))?;

    Ok(())
}

/// Extract the `(host_id, tenant_id)` pair from a
/// `{"data": {"id": "...", "tenant_id": "..."}}` response.
fn parse_host_identity(body: &serde_json::Value) -> Option<HostIdentity> {
    let data = body.get("data")?;
    let id_str = data.get("id")?.as_str()?;
    let tenant_str = data.get("tenant_id")?.as_str()?;
    Some(HostIdentity {
        host_id: Uuid::parse_str(id_str).ok()?,
        tenant_id: Uuid::parse_str(tenant_str).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_host_by_name_returns_matching_identity() {
        let body = serde_json::json!({
            "data": [
                {"id": "00000000-0000-0000-0000-000000000001",
                 "tenant_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                 "name": "worker-a"},
                {"id": "00000000-0000-0000-0000-000000000002",
                 "tenant_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                 "name": "worker-b"},
            ]
        });
        let identity = find_host_by_name(&body, "worker-b").unwrap();
        assert_eq!(
            identity.host_id,
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap()
        );
        assert_eq!(
            identity.tenant_id,
            Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap()
        );
    }

    #[test]
    fn find_host_by_name_returns_none_when_absent() {
        let body = serde_json::json!({
            "data": [
                {"id": "00000000-0000-0000-0000-000000000001",
                 "tenant_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                 "name": "worker-a"},
            ]
        });
        assert!(find_host_by_name(&body, "missing-worker").is_none());
    }

    #[test]
    fn find_host_by_name_handles_empty_list() {
        let body = serde_json::json!({"data": []});
        assert!(find_host_by_name(&body, "any").is_none());
    }

    #[test]
    fn find_host_by_name_returns_none_when_tenant_id_missing() {
        let body = serde_json::json!({
            "data": [
                {"id": "00000000-0000-0000-0000-000000000001", "name": "worker-a"},
            ]
        });
        assert!(find_host_by_name(&body, "worker-a").is_none());
    }

    #[test]
    fn parse_host_identity_extracts_both_fields() {
        let body = serde_json::json!({
            "data": {
                "id": "00000000-0000-0000-0000-000000000042",
                "tenant_id": "cccccccc-cccc-cccc-cccc-cccccccccccc",
                "name": "test",
            }
        });
        let identity = parse_host_identity(&body).unwrap();
        assert_eq!(
            identity.host_id,
            Uuid::parse_str("00000000-0000-0000-0000-000000000042").unwrap()
        );
        assert_eq!(
            identity.tenant_id,
            Uuid::parse_str("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap()
        );
    }

    #[test]
    fn parse_host_identity_returns_none_for_missing_data() {
        let body = serde_json::json!({"error": "not found"});
        assert!(parse_host_identity(&body).is_none());
    }

    #[test]
    fn parse_host_identity_returns_none_for_missing_tenant_id() {
        let body = serde_json::json!({
            "data": {"id": "00000000-0000-0000-0000-000000000042", "name": "test"}
        });
        assert!(parse_host_identity(&body).is_none());
    }

    #[test]
    fn upload_embodiment_body_omits_runtime_key_when_none() {
        let model = serde_json::json!({"model_id": "test", "model_digest": "abc"});
        let body = build_embodiment_body(&model, Option::<&serde_json::Value>::None).unwrap();
        assert!(body.get("model").is_some(), "body must contain model key");
        assert!(
            body.get("runtime").is_none(),
            "body must not contain runtime key when None"
        );
    }

    #[test]
    fn upload_embodiment_body_includes_runtime_key_when_some() {
        let model = serde_json::json!({"model_id": "test", "model_digest": "abc"});
        let runtime = serde_json::json!({"combined_digest": "abc"});
        let body = build_embodiment_body(&model, Some(&runtime)).unwrap();
        assert!(body.get("model").is_some(), "body must contain model key");
        assert!(body.get("runtime").is_some(), "body must contain runtime key when Some");
    }
}
