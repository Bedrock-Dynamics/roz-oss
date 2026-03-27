//! Device provisioning for first-boot robot onboarding.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct ProvisionRequest {
    #[serde(skip_serializing)]
    pub claim_token: String,
    pub hardware_id: String,
    pub robot_manifest: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProvisionResponse {
    pub api_key: String,
    pub nats_credentials: Option<String>,
    pub worker_id: String,
}

pub async fn provision_device(api_url: &str, request: &ProvisionRequest) -> anyhow::Result<ProvisionResponse> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{api_url}/v1/devices/provision"))
        .header("Authorization", format!("Bearer {}", request.claim_token))
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    let body: ProvisionResponse = resp.json().await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_request_serializes() {
        let req = ProvisionRequest {
            claim_token: "tok_abc".to_owned(),
            hardware_id: "hw-001".to_owned(),
            robot_manifest: Some("v1".to_owned()),
        };
        let json = serde_json::to_value(&req).expect("serialize");
        // claim_token is skip_serializing — it goes in the Authorization header, not the body
        assert!(
            json.get("claim_token").is_none(),
            "claim_token must not appear in serialized JSON"
        );
        assert_eq!(json["hardware_id"], "hw-001");
        assert_eq!(json["robot_manifest"], "v1");
    }

    #[tokio::test]
    async fn provision_fails_on_bad_url() {
        let req = ProvisionRequest {
            claim_token: "tok_abc".to_owned(),
            hardware_id: "hw-001".to_owned(),
            robot_manifest: None,
        };
        // Port 1 is reserved; connecting to it always results in connection refused.
        let err = provision_device("http://127.0.0.1:1", &req)
            .await
            .expect_err("should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("Connection refused")
                || msg.contains("connection refused")
                || msg.contains("error sending request"),
            "unexpected error: {msg}"
        );
    }
}
