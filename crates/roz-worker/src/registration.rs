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

    // 1. List hosts and find matching name
    let resp = client
        .get(format!("{base}/v1/hosts"))
        .bearer_auth(api_key)
        .send()
        .await
        .context("GET /v1/hosts request failed")?
        .error_for_status()
        .context("GET /v1/hosts returned error status")?;

    let body: serde_json::Value = resp.json().await.context("failed to parse GET /v1/hosts response")?;

    let host_id = find_host_by_name(&body, worker_id);

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
            .context("POST /v1/hosts request failed")?
            .error_for_status()
            .context("POST /v1/hosts returned error status")?;

        let body: serde_json::Value = resp.json().await.context("failed to parse POST /v1/hosts response")?;
        parse_host_id(&body).context("POST /v1/hosts response missing host id")?
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
}
