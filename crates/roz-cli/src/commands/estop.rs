use crate::config::CliConfig;

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

async fn resolve_host_id(client: &reqwest::Client, api_url: &str, host: &str) -> anyhow::Result<String> {
    if uuid::Uuid::parse_str(host).is_ok() {
        return Ok(host.to_string());
    }
    let url = format!("{api_url}/v1/hosts");
    let resp = client.get(&url).send().await?;
    let body: serde_json::Value = resp.json().await?;
    let hosts = body["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unexpected response format"))?;
    for h in hosts {
        if h["name"].as_str() == Some(host)
            && let Some(id) = h["id"].as_str()
        {
            return Ok(id.to_string());
        }
    }
    anyhow::bail!("host '{host}' not found. Run `roz host list` to see available hosts.")
}
