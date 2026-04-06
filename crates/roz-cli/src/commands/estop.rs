use serde::Deserialize;

use crate::config::CliConfig;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct HostListEntry {
    id: String,
    name: String,
    #[serde(default = "default_host_status")]
    status: String,
}

#[derive(Debug, Deserialize)]
struct HostListResponse {
    data: Vec<HostListEntry>,
}

fn default_host_status() -> String {
    "unknown".to_string()
}

fn resolve_host_id_from_candidates(host: &str, matches: Vec<HostListEntry>) -> anyhow::Result<String> {
    match matches.as_slice() {
        [] => anyhow::bail!("host '{host}' not found. Run `roz host list` to see available hosts."),
        [single] => Ok(single.id.clone()),
        _ => {
            let candidates = matches
                .iter()
                .map(|entry| format!("  - {} | {} | status: {}", entry.id, entry.name, entry.status))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "host name '{host}' is ambiguous. Matching hosts:\n{candidates}\nUse the host UUID instead of the shared name."
            );
        }
    }
}

/// Trigger emergency stop on a robot host via REST API.
pub async fn execute(config: &CliConfig, host: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let host_id = resolve_host_id(&client, &config.api_url, host).await?;
    let url = format!("{}/v1/hosts/{}/estop", config.api_url, host_id);
    let resp = client.post(&url).send().await?;

    if resp.status().is_success() {
        eprintln!("E-STOP sent to {host}");
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("E-STOP failed ({status}): {body}");
    }
    Ok(())
}

pub async fn resolve_host_id(client: &reqwest::Client, api_url: &str, host: &str) -> anyhow::Result<String> {
    if uuid::Uuid::parse_str(host).is_ok() {
        return Ok(host.to_string());
    }

    let mut offset: u64 = 0;
    let limit: u64 = 50;
    let mut matches = Vec::new();

    loop {
        let url = format!("{api_url}/v1/hosts?limit={limit}&offset={offset}");
        let resp = client.get(&url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET /v1/hosts failed ({status}): {body}");
        }

        let body: HostListResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("unexpected response format from /v1/hosts: {e}"))?;
        let page_len = body.data.len();
        matches.extend(body.data.into_iter().filter(|entry| entry.name == host));

        // If we got fewer results than the limit, we've exhausted all pages.
        if page_len < usize::try_from(limit).unwrap_or(usize::MAX) {
            break;
        }
        offset += limit;
    }

    resolve_host_id_from_candidates(host, matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_host_id_from_candidates_returns_single_match() {
        let resolved = resolve_host_id_from_candidates(
            "reachy-dev",
            vec![HostListEntry {
                id: "00000000-0000-0000-0000-000000000111".to_string(),
                name: "reachy-dev".to_string(),
                status: "online".to_string(),
            }],
        )
        .expect("single host should resolve");
        assert_eq!(resolved, "00000000-0000-0000-0000-000000000111");
    }

    #[test]
    fn resolve_host_id_from_candidates_rejects_duplicate_names() {
        let error = resolve_host_id_from_candidates(
            "reachy-dev",
            vec![
                HostListEntry {
                    id: "00000000-0000-0000-0000-000000000111".to_string(),
                    name: "reachy-dev".to_string(),
                    status: "online".to_string(),
                },
                HostListEntry {
                    id: "00000000-0000-0000-0000-000000000222".to_string(),
                    name: "reachy-dev".to_string(),
                    status: "offline".to_string(),
                },
            ],
        )
        .expect_err("duplicate names should be rejected");
        let message = error.to_string();
        assert!(message.contains("ambiguous"));
        assert!(message.contains("00000000-0000-0000-0000-000000000111"));
        assert!(message.contains("00000000-0000-0000-0000-000000000222"));
    }

    #[test]
    fn resolve_host_id_from_candidates_rejects_missing_names() {
        let error = resolve_host_id_from_candidates("missing-host", Vec::new()).expect_err("missing host should fail");
        assert!(error.to_string().contains("host 'missing-host' not found"));
    }

    #[tokio::test]
    async fn resolve_host_id_accepts_uuid_without_querying_hosts() {
        let client = reqwest::Client::new();
        let host_id = "00000000-0000-0000-0000-000000000111";
        let resolved = resolve_host_id(&client, "http://127.0.0.1:9", host_id)
            .await
            .expect("UUID should bypass network lookup");
        assert_eq!(resolved, host_id);
    }
}
