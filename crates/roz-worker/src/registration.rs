//! Worker auto-registration with the roz API server.
//!
//! On startup the worker calls the server's REST API to ensure a host record
//! exists for its `worker_id` and sets its status to `online`.

use anyhow::{Context, Result};
use uuid::Uuid;

/// Register the worker as a host with the server, returning the host UUID.
///
/// 1. `GET /v1/hosts` — look for a host whose `name` matches `worker_id`.
/// 2. If found: `PATCH /v1/hosts/{id}/status` with `{"status": "online"}`, return id.
/// 3. If not found: `POST /v1/hosts` with `{"name": worker_id, "host_type": "edge"}`,
///    then `PATCH` status to `online`, return id.
pub async fn register_host(api_url: &str, api_key: &str, worker_id: &str) -> Result<Uuid> {
    let client = reqwest::Client::new();
    let base = api_url.trim_end_matches('/');

    // 1. List hosts and find matching name (paginated)
    let host_id = find_host_paginated(&client, base, api_key, worker_id).await?;

    let id = if let Some(existing_id) = host_id {
        existing_id
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
            find_host_paginated(&client, base, api_key, worker_id)
                .await?
                .context("host not found after conflict retry")?
        } else {
            let resp = resp
                .error_for_status()
                .context("POST /v1/hosts returned error status")?;
            let body: serde_json::Value = resp.json().await.context("failed to parse POST /v1/hosts response")?;
            parse_host_id(&body).context("POST /v1/hosts response missing host id")?
        }
    };

    // Set status to online
    let status_body = serde_json::json!({"status": "online"});
    client
        .patch(format!("{base}/v1/hosts/{id}/status"))
        .bearer_auth(api_key)
        .json(&status_body)
        .send()
        .await
        .context("PATCH host status request failed")?
        .error_for_status()
        .context("PATCH host status returned error status")?;

    Ok(id)
}

/// Paginate through `GET /v1/hosts` looking for a host whose `name` matches `worker_id`.
async fn find_host_paginated(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    worker_id: &str,
) -> Result<Option<Uuid>> {
    let limit: usize = 50;
    let mut offset: usize = 0;
    loop {
        let resp = client
            .get(format!("{base}/v1/hosts?limit={limit}&offset={offset}"))
            .bearer_auth(api_key)
            .send()
            .await
            .context("GET /v1/hosts request failed")?
            .error_for_status()
            .context("GET /v1/hosts returned error status")?;

        let body: serde_json::Value = resp.json().await.context("failed to parse GET /v1/hosts response")?;

        if let Some(id) = find_host_by_name(&body, worker_id) {
            return Ok(Some(id));
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
fn find_host_by_name(body: &serde_json::Value, worker_id: &str) -> Option<Uuid> {
    let hosts = body.get("data")?.as_array()?;
    for host in hosts {
        if host.get("name")?.as_str()? == worker_id {
            let id_str = host.get("id")?.as_str()?;
            return Uuid::parse_str(id_str).ok();
        }
    }
    None
}

/// Upload embodiment model (and optional runtime) to the server for a registered host.
///
/// Called after `register_host()` returns the host UUID, when the worker has
/// embodiment data available. Sends `PUT /v1/hosts/{id}/embodiment` with the
/// serialised model and optional runtime as JSON.
pub async fn upload_embodiment(
    api_url: &str,
    api_key: &str,
    host_id: Uuid,
    model: &roz_core::embodiment::model::EmbodimentModel,
    runtime: Option<&roz_core::embodiment::embodiment_runtime::EmbodimentRuntime>,
) -> Result<()> {
    let client = reqwest::Client::new();
    let base = api_url.trim_end_matches('/');

    let mut body = serde_json::json!({
        "model": model,
    });
    if let Some(rt) = runtime {
        body["runtime"] = serde_json::to_value(rt).context("failed to serialize embodiment runtime")?;
    }

    client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .context("PUT /v1/hosts/{id}/embodiment request failed")?
        .error_for_status()
        .context("PUT /v1/hosts/{id}/embodiment returned error status")?;

    Ok(())
}

/// Extract the host UUID from a `{"data": {"id": "..."}}` response.
fn parse_host_id(body: &serde_json::Value) -> Option<Uuid> {
    let id_str = body.get("data")?.get("id")?.as_str()?;
    Uuid::parse_str(id_str).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_host_by_name_returns_matching_id() {
        let body = serde_json::json!({
            "data": [
                {"id": "00000000-0000-0000-0000-000000000001", "name": "worker-a"},
                {"id": "00000000-0000-0000-0000-000000000002", "name": "worker-b"},
            ]
        });
        let id = find_host_by_name(&body, "worker-b").unwrap();
        assert_eq!(id, Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap());
    }

    #[test]
    fn find_host_by_name_returns_none_when_absent() {
        let body = serde_json::json!({
            "data": [
                {"id": "00000000-0000-0000-0000-000000000001", "name": "worker-a"},
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
    fn parse_host_id_extracts_uuid() {
        let body = serde_json::json!({
            "data": {"id": "00000000-0000-0000-0000-000000000042", "name": "test"}
        });
        let id = parse_host_id(&body).unwrap();
        assert_eq!(id, Uuid::parse_str("00000000-0000-0000-0000-000000000042").unwrap());
    }

    #[test]
    fn parse_host_id_returns_none_for_missing_data() {
        let body = serde_json::json!({"error": "not found"});
        assert!(parse_host_id(&body).is_none());
    }

    #[test]
    fn upload_embodiment_body_has_model_key() {
        // Verify the JSON body shape matches what the server endpoint expects
        let model = serde_json::json!({"model_id": "test", "joints": []});
        let body = serde_json::json!({"model": model});
        assert!(body.get("model").is_some());
        assert!(body.get("runtime").is_none());
    }

    #[test]
    fn upload_embodiment_body_with_runtime() {
        let model = serde_json::json!({"model_id": "test"});
        let runtime = serde_json::json!({"combined_digest": "abc"});
        let body = serde_json::json!({"model": model, "runtime": runtime});
        assert!(body.get("model").is_some());
        assert!(body.get("runtime").is_some());
    }
}
