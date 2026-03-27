use axum::Json;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;

/// How long device codes remain valid (seconds).
const DEVICE_CODE_TTL_SECS: i64 = 600;

/// Polling interval the CLI should use (seconds).
const POLL_INTERVAL_SECS: u64 = 5;

/// Characters used to generate human-readable user codes.
const USER_CODE_CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Generate a cryptographically random device code (32 bytes, URL-safe base64).
fn generate_device_code() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a human-readable user code in XXXX-XXXX format.
///
/// Uses an alphabet that excludes easily confused characters (0, O, 1, I).
fn generate_user_code() -> String {
    let mut rng = rand::thread_rng();
    let mut code = String::with_capacity(9);
    for i in 0..8 {
        if i == 4 {
            code.push('-');
        }
        let idx = (rng.next_u32() as usize) % USER_CODE_CHARS.len();
        code.push(USER_CODE_CHARS[idx] as char);
    }
    code
}

// -- Request / Response types ------------------------------------------------

#[derive(Deserialize)]
pub struct PollTokenRequest {
    pub device_code: String,
}

#[derive(Deserialize)]
pub struct CompleteRequest {
    pub user_code: String,
}

// -- Handlers ----------------------------------------------------------------

/// POST /v1/auth/device/code
///
/// Initiates a device authorization flow (RFC 8628). Returns a device code
/// for CLI polling and a user code for browser entry.
pub async fn request_code(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let device_code = generate_device_code();
    let user_code = generate_user_code();
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEVICE_CODE_TTL_SECS);

    roz_db::device_codes::create_device_code(&state.pool, &device_code, &user_code, expires_at)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to create device code");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
        })?;

    let studio_url = std::env::var("ROZ_STUDIO_URL").unwrap_or_else(|_| "https://bedrockdynamics.studio".into());
    let verification_uri = format!("{studio_url}/auth/device/verify?api_url={}", state.base_url);

    Ok(Json(json!({
        "device_code": device_code,
        "user_code": user_code,
        "verification_uri": verification_uri,
        "interval": POLL_INTERVAL_SECS,
        "expires_in": DEVICE_CODE_TTL_SECS,
    })))
}

/// POST /v1/auth/device/token
///
/// Polled by the CLI until the user completes browser authentication.
/// Returns the appropriate RFC 8628 error codes while pending.
pub async fn poll_token(
    State(state): State<AppState>,
    Json(body): Json<PollTokenRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let row = roz_db::device_codes::get_by_device_code(&state.pool, &body.device_code)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to look up device code");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
        })?
        .ok_or_else(|| (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))))?;

    // Check expiry
    if row.expires_at < chrono::Utc::now() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "expired_token"}))));
    }

    // Check if user has completed the flow
    if row.completed_at.is_none() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "authorization_pending"}))));
    }

    // Completed -- mint an API key for this tenant/user
    let user_id = row.user_id.as_deref().unwrap_or("device_flow");
    let tenant_id = row.tenant_id.ok_or_else(|| {
        tracing::error!("device code completed but missing tenant_id");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "internal server error"})),
        )
    })?;

    let default_scopes = vec![
        "ReadTasks".into(),
        "WriteTasks".into(),
        "ReadHosts".into(),
        "ReadStreams".into(),
    ];
    let key_result =
        roz_db::api_keys::create_api_key(&state.pool, tenant_id, "CLI (device flow)", &default_scopes, user_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "failed to create API key for device flow");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
            })?;

    Ok(Json(json!({
        "access_token": key_result.full_key,
        "token_type": "bearer",
    })))
}

/// POST /v1/auth/device/complete
///
/// Called when the user submits their user code in the browser.
/// Accepts form-urlencoded data (from the HTML form).
/// Requires API key authentication so we know who is authorizing the device.
pub async fn complete_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(body): Form<CompleteRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let auth_header = headers.get("authorization").and_then(|v| v.to_str().ok());
    let auth = crate::auth::extract_auth(&state.auth, &state.pool, auth_header)
        .await
        .map_err(|e| (StatusCode::UNAUTHORIZED, Json(json!({"error": e.0}))))?;

    let tenant_id = *auth.tenant_id().as_uuid();
    let user_id = "api_key".to_string();

    // Validate user code exists and is not expired
    let row = roz_db::device_codes::get_by_user_code(&state.pool, &body.user_code)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to look up user code");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
        })?
        .ok_or_else(|| (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid user code"}))))?;

    if row.expires_at < chrono::Utc::now() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "expired code"}))));
    }

    if row.completed_at.is_some() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "code already used"}))));
    }

    let completed = roz_db::device_codes::complete_device_code(&state.pool, &body.user_code, &user_id, tenant_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to complete device code");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
        })?;

    if completed {
        Ok((StatusCode::OK, Json(json!({"status": "authorized"}))))
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "failed to authorize device"})),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_code_is_url_safe_base64() {
        let code = generate_device_code();
        // 32 bytes -> 43 chars in URL-safe base64 (no padding)
        assert_eq!(code.len(), 43);
        assert!(code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn user_code_format() {
        let code = generate_user_code();
        assert_eq!(code.len(), 9); // XXXX-XXXX
        assert_eq!(&code[4..5], "-");
        // All chars except the dash should be from USER_CODE_CHARS
        for (i, ch) in code.chars().enumerate() {
            if i == 4 {
                assert_eq!(ch, '-');
            } else {
                assert!(USER_CODE_CHARS.contains(&(ch as u8)), "unexpected char: {ch}");
            }
        }
    }

    #[test]
    fn user_code_uniqueness() {
        // Generate a batch and verify no collisions (probabilistic but very unlikely)
        let codes: std::collections::HashSet<String> = (0..100).map(|_| generate_user_code()).collect();
        assert_eq!(codes.len(), 100);
    }
}
