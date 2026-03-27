use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Args, Subcommand};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::config::CliConfig;

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_SCOPES: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// Authentication commands.
#[derive(Debug, Args)]
pub struct AuthArgs {
    /// The auth subcommand to execute.
    #[command(subcommand)]
    pub command: AuthCommands,
}

/// Available authentication subcommands.
#[derive(Debug, Subcommand)]
pub enum AuthCommands {
    /// Log in to the Roz platform.
    Login {
        /// Use device code flow (for headless environments without a browser).
        #[arg(long = "device-code", alias = "browserless")]
        device_code: bool,
        /// Provider to authenticate with (e.g., "openai"). Default: roz cloud.
        provider: Option<String>,
    },
    /// Log out of the current session.
    Logout,
    /// Display the current authenticated user.
    Whoami,
    /// Print the current access token.
    Token,
}

/// Execute an authentication subcommand.
pub async fn execute(cmd: &AuthCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        AuthCommands::Login { device_code, provider } => login(config, *device_code, provider.as_deref()).await,
        AuthCommands::Logout => logout(config),
        AuthCommands::Whoami => whoami(config).await,
        AuthCommands::Token => token(config),
    }
}

async fn login(config: &CliConfig, device_code: bool, provider: Option<&str>) -> anyhow::Result<()> {
    match provider {
        None | Some("cloud" | "roz") => {
            if device_code {
                login_device_code(config).await
            } else {
                login_localhost(config).await
            }
        }
        Some("openai") => login_openai(config).await,
        Some(other) => anyhow::bail!("Unsupported auth provider: {other}. Supported: cloud, openai"),
    }
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

async fn login_localhost(config: &CliConfig) -> anyhow::Result<()> {
    let state = generate_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let studio_url = std::env::var("ROZ_STUDIO_URL").unwrap_or_else(|_| "https://bedrockdynamics.studio".into());
    let auth_url = format!(
        "{studio_url}/auth/cli?callback_port={port}&state={state}&api_url={}",
        config.api_url
    );

    if webbrowser::open(&auth_url).is_err() {
        eprintln!("Open this URL in your browser to authenticate:\n  {auth_url}");
    }

    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_message("Waiting for authentication...");
    spinner.enable_steady_tick(Duration::from_millis(100));

    // Accept with 120s timeout
    let (stream, _) = tokio::time::timeout(Duration::from_secs(120), listener.accept())
        .await
        .map_err(|_| anyhow::anyhow!("Timed out waiting for browser callback. Run `roz auth login` again."))??;

    let mut reader = tokio::io::BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    let (code, received_state) = parse_callback_query(&request_line);

    // Drain remaining headers so the browser doesn't stall
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
    }

    let mut stream = reader.into_inner();

    if received_state != state {
        send_http_response_async(&mut stream, 400, "CSRF state mismatch. Please try again.").await;
        spinner.finish_with_message("error");
        anyhow::bail!("CSRF state mismatch in auth callback");
    }

    if code.is_empty() {
        send_http_response_async(&mut stream, 400, "Missing authorization code. Please try again.").await;
        spinner.finish_with_message("error");
        anyhow::bail!("Missing authorization code in auth callback");
    }

    // Exchange code for API key (fully async — no runtime conflict)
    let client = reqwest::Client::new();
    let token_resp: serde_json::Value = client
        .post(format!("{}/v1/auth/device/token", config.api_url))
        .json(&serde_json::json!({"device_code": code}))
        .send()
        .await?
        .json()
        .await?;

    if let Some(error) = token_resp.get("error").and_then(|e| e.as_str()) {
        send_http_response_async(&mut stream, 400, &format!("Auth failed: {error}")).await;
        spinner.finish_with_message("error");
        anyhow::bail!("Token exchange failed: {error}");
    }

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Server did not return an access token"))?;

    store_token(&config.profile, access_token)?;

    send_http_response_async(
        &mut stream,
        200,
        "<!DOCTYPE html><html><body><h1>Authorized!</h1><p>You can close this window.</p></body></html>",
    )
    .await;

    spinner.finish_with_message("authenticated");
    eprintln!("Logged in successfully.");
    Ok(())
}

async fn login_openai(_config: &CliConfig) -> anyhow::Result<()> {
    let mut verifier_bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = generate_state();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let auth_url = format!(
        "{OPENAI_AUTHORIZE}?response_type=code&client_id={OPENAI_CLIENT_ID}\
         &redirect_uri=http://localhost:{port}/auth/callback\
         &code_challenge={challenge}&code_challenge_method=S256\
         &scope={}&state={state}&originator=roz_cli",
        OPENAI_SCOPES.replace(' ', "+")
    );

    if webbrowser::open(&auth_url).is_err() {
        eprintln!("Open this URL in your browser:\n  {auth_url}");
    }

    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_message("Waiting for OpenAI authentication...");
    spinner.enable_steady_tick(Duration::from_millis(100));

    let (stream, _) = tokio::time::timeout(Duration::from_secs(120), listener.accept())
        .await
        .map_err(|_| anyhow::anyhow!("Timed out waiting for OpenAI callback."))??;

    let mut reader = tokio::io::BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let (code, received_state) = parse_callback_query(&request_line);

    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
    }
    let mut stream = reader.into_inner();

    if received_state != state {
        send_http_response_async(&mut stream, 400, "CSRF state mismatch.").await;
        spinner.finish_with_message("error");
        anyhow::bail!("CSRF state mismatch in OpenAI callback");
    }

    if code.is_empty() {
        send_http_response_async(&mut stream, 400, "Missing authorization code.").await;
        spinner.finish_with_message("error");
        anyhow::bail!("Missing authorization code in OpenAI callback");
    }

    let client = reqwest::Client::new();
    let token_resp: serde_json::Value = client
        .post(OPENAI_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &*code),
            ("redirect_uri", &format!("http://localhost:{port}/auth/callback")),
            ("client_id", OPENAI_CLIENT_ID),
            ("code_verifier", &*verifier),
        ])
        .send()
        .await?
        .json()
        .await?;

    if let Some(error) = token_resp.get("error").and_then(|e| e.as_str()) {
        send_http_response_async(&mut stream, 400, &format!("Auth failed: {error}")).await;
        spinner.finish_with_message("error");
        anyhow::bail!("OpenAI token exchange failed: {error}");
    }

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("OpenAI did not return access_token"))?;
    let refresh_token = token_resp["refresh_token"].as_str();
    let expires_in = token_resp["expires_in"].as_u64();

    CliConfig::save_provider_credential("openai", access_token, refresh_token, expires_in)?;

    send_http_response_async(
        &mut stream,
        200,
        "<!DOCTYPE html><html><body><h1>Authorized!</h1><p>You can close this window.</p></body></html>",
    )
    .await;

    spinner.finish_with_message("authenticated");
    eprintln!("Logged in to OpenAI successfully.");
    Ok(())
}

fn parse_callback_query(request_line: &str) -> (String, String) {
    let mut code = String::new();
    let mut state = String::new();

    // Extract the path from "GET /path?query HTTP/1.1"
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return (code, state);
    }

    if let Some(query_start) = parts[1].find('?') {
        let query = &parts[1][query_start + 1..];
        for param in query.split('&') {
            if let Some((key, value)) = param.split_once('=') {
                match key {
                    "code" => code = value.to_string(),
                    "state" => state = value.to_string(),
                    _ => {}
                }
            }
        }
    }

    (code, state)
}

async fn send_http_response_async(stream: &mut tokio::net::TcpStream, status: u16, body: &str) {
    let status_text = if status == 200 { "OK" } else { "Bad Request" };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn login_device_code(config: &CliConfig) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(format!("{}/v1/auth/device/code", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let device_code_value = resp["device_code"].as_str().unwrap_or_default();
    let user_code = resp["user_code"].as_str().unwrap_or_default();
    let verification_uri = resp["verification_uri"].as_str().unwrap_or_default();
    let interval = resp["interval"].as_u64().unwrap_or(5);
    let expires_in = resp["expires_in"].as_u64().unwrap_or(600);

    eprintln!("Enter code: {user_code}");
    eprintln!("Verification URL: {verification_uri}");

    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_message("Waiting for authentication...");
    spinner.enable_steady_tick(Duration::from_millis(100));

    let deadline = std::time::Instant::now() + Duration::from_secs(expires_in);

    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;

        if std::time::Instant::now() >= deadline {
            spinner.finish_with_message("timed out");
            anyhow::bail!("Device code expired. Run `roz auth login` again.");
        }

        let token_resp: serde_json::Value = client
            .post(format!("{}/v1/auth/device/token", config.api_url))
            .json(&serde_json::json!({"device_code": device_code_value}))
            .send()
            .await?
            .json()
            .await?;

        if let Some(error) = token_resp.get("error").and_then(serde_json::Value::as_str) {
            match error {
                "authorization_pending" => continue,
                "expired_token" => {
                    spinner.finish_with_message("expired");
                    anyhow::bail!("Device code expired. Run `roz auth login` again.");
                }
                other => {
                    spinner.finish_with_message("error");
                    anyhow::bail!("Auth error: {other}");
                }
            }
        }

        // Success -- store token
        if let Some(access_token) = token_resp["access_token"].as_str() {
            store_token(&config.profile, access_token)?;
            spinner.finish_with_message("authenticated");
            eprintln!("Logged in successfully.");
            return Ok(());
        }
    }
}

fn store_token(profile: &str, token: &str) -> anyhow::Result<()> {
    // Always persist to credentials.toml (reliable across invocations)
    CliConfig::save_global_api_key(profile, token)?;

    // Also try keyring as a fast-path cache (best-effort)
    if let Ok(entry) = keyring::Entry::new("roz", profile) {
        let _ = entry.set_password(token);
    }

    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn logout(config: &CliConfig) -> anyhow::Result<()> {
    // Clear keyring entry if it exists.
    if let Ok(entry) = keyring::Entry::new("roz", &config.profile) {
        let _ = entry.delete_credential();
    }

    // Clear credentials file if it exists.
    if let Ok(config_dir) = CliConfig::config_dir() {
        let cred_path = config_dir.join("credentials.toml");
        if cred_path.exists() {
            let _ = std::fs::remove_file(&cred_path);
        }
    }

    eprintln!("Logged out successfully.");
    Ok(())
}

async fn whoami(config: &CliConfig) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/auth/whoami", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

fn token(config: &CliConfig) -> anyhow::Result<()> {
    match &config.access_token {
        Some(t) => {
            println!("{t}");
            Ok(())
        }
        None => anyhow::bail!("No access token found. Run `roz auth login` first."),
    }
}

/// Refresh an `OpenAI` OAuth token using the stored `refresh_token`.
///
/// Reads the current credentials from `~/.roz/credentials.toml`, exchanges
/// the refresh token for a new access token via `OpenAI`'s token endpoint,
/// and persists the updated credentials back to disk.
///
/// This is designed for background use -- callers should log failures rather
/// than propagating them to the user.
pub async fn refresh_openai_token() -> anyhow::Result<()> {
    let cred_path = CliConfig::config_dir()?.join("credentials.toml");
    let refresh_token = read_openai_refresh_token(&cred_path)?;

    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(OPENAI_TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", OPENAI_CLIENT_ID),
            ("refresh_token", &*refresh_token),
        ])
        .send()
        .await?
        .json()
        .await?;

    if let Some(error) = resp.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("OpenAI token refresh failed: {error}");
    }

    let access_token = resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("OpenAI refresh did not return access_token"))?;
    let new_refresh = resp["refresh_token"].as_str();
    let expires_in = resp["expires_in"].as_u64();

    // TODO: Add file-lock protection for concurrent refresh across multiple roz instances.
    // OpenAI handles concurrent refresh requests gracefully, so this is not critical.
    CliConfig::save_provider_credential("openai", access_token, new_refresh, expires_in)?;
    Ok(())
}

/// Extract the `refresh_token` from a credentials file at the given path.
///
/// Returns the refresh token string, or an error if the file is missing,
/// has no `[openai]` section, or lacks a `refresh_token` key.
fn read_openai_refresh_token(cred_path: &std::path::Path) -> anyhow::Result<String> {
    let contents = std::fs::read_to_string(cred_path)?;
    let table: toml::Table = contents.parse()?;
    let section = table
        .get("openai")
        .and_then(|v| v.as_table())
        .ok_or_else(|| anyhow::anyhow!("No OpenAI credentials stored"))?;
    let refresh_token = section
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("No refresh_token stored for OpenAI"))?;
    Ok(refresh_token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_requires_credentials_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let missing = dir.path().join("nonexistent.toml");
        let result = read_openai_refresh_token(&missing);
        assert!(result.is_err(), "should fail when credentials file is missing");
    }

    #[test]
    fn refresh_requires_openai_section() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cred_path = dir.path().join("credentials.toml");
        std::fs::write(&cred_path, "[default]\napi_key = \"roz_sk_test\"\n").unwrap();

        let result = read_openai_refresh_token(&cred_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No OpenAI credentials"));
    }

    #[test]
    fn refresh_requires_refresh_token() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cred_path = dir.path().join("credentials.toml");
        std::fs::write(&cred_path, "[openai]\naccess_token = \"tok_test\"\n").unwrap();

        let result = read_openai_refresh_token(&cred_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No refresh_token"));
    }

    #[test]
    fn refresh_reads_valid_token() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cred_path = dir.path().join("credentials.toml");
        std::fs::write(
            &cred_path,
            "[openai]\naccess_token = \"at_test\"\nrefresh_token = \"rt_test_123\"\n",
        )
        .unwrap();

        let token = read_openai_refresh_token(&cred_path).expect("should read refresh token");
        assert_eq!(token, "rt_test_123");
    }
}
