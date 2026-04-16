//! Authentication commands.
//!
//! Plan 19-15 collapsed the OpenAI login + refresh helpers into thin callers
//! over [`roz_openai::auth::oauth`]. The Roz-cloud OAuth flow
//! (`login_localhost`) and device-code flow (`login_device_code`) remain here
//! because they target Roz-cloud endpoints, not OpenAI's OAuth.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Args, Subcommand};
use rand::RngCore;
use secrecy::{ExposeSecret, SecretString};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::config::CliConfig;

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
        Some("openai") => login_openai().await,
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

/// Thin caller into [`roz_openai::auth::oauth::run_oauth_flow`].
///
/// Plan 19-15: PKCE + callback server + token exchange all live in roz-openai
/// now. This function just orchestrates persistence to `~/.roz/credentials.toml`
/// with the v2 schema (absolute `expires_at` + optional `account_id`).
async fn login_openai() -> anyhow::Result<()> {
    let tokens = roz_openai::auth::oauth::run_oauth_flow()
        .await
        .map_err(|e| anyhow::anyhow!("OpenAI OAuth flow failed: {e}"))?;

    let expires_at =
        chrono::Utc::now() + chrono::Duration::seconds(tokens.expires_in_secs.unwrap_or(3600).cast_signed());

    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(|jwt| roz_openai::auth::token_data::parse_chatgpt_jwt_claims(jwt).ok())
        .and_then(|info| info.chatgpt_account_id);

    CliConfig::save_provider_credential_v2(
        "openai",
        tokens.access_token.expose_secret(),
        tokens.refresh_token.as_ref().map(ExposeSecret::expose_secret),
        Some(expires_at),
        account_id.as_deref(),
    )?;

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

/// Refresh an OpenAI OAuth token using the stored `refresh_token`.
///
/// Plan 19-15: delegates the HTTP + parse to
/// [`roz_openai::auth::oauth::refresh_access_token`]. Persists the new tokens
/// back to `~/.roz/credentials.toml` with the v2 schema.
///
/// This is designed for background use — callers should log failures rather
/// than propagating them to the user.
pub async fn refresh_openai_token() -> anyhow::Result<()> {
    let stored = CliConfig::load_provider_credential_full("openai")
        .ok_or_else(|| anyhow::anyhow!("No OpenAI credentials stored"))?;
    let refresh_token = stored
        .refresh_token
        .ok_or_else(|| anyhow::anyhow!("No refresh_token stored for OpenAI"))?;

    let http = reqwest::Client::new();
    let refresh_secret = SecretString::from(refresh_token);
    let tokens = roz_openai::auth::oauth::refresh_access_token(&refresh_secret, &http)
        .await
        .map_err(|e| anyhow::anyhow!("OpenAI token refresh failed: {e}"))?;

    let expires_at =
        chrono::Utc::now() + chrono::Duration::seconds(tokens.expires_in_secs.unwrap_or(3600).cast_signed());

    // Preserve existing account_id; refresh responses typically omit id_token.
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(|jwt| roz_openai::auth::token_data::parse_chatgpt_jwt_claims(jwt).ok())
        .and_then(|info| info.chatgpt_account_id)
        .or(stored.account_id);

    CliConfig::save_provider_credential_v2(
        "openai",
        tokens.access_token.expose_secret(),
        tokens.refresh_token.as_ref().map(ExposeSecret::expose_secret),
        Some(expires_at),
        account_id.as_deref(),
    )?;
    Ok(())
}
