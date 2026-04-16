use chrono::{DateTime, Duration, Utc};
use oauth2::TokenResponse;
use rmcp::transport::auth::{AuthError, AuthorizationManager, AuthorizationSession};
use secrecy::SecretString;
use serde_json::Value;

pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 300;
const DEFAULT_REDIRECT_URI: &str = "https://roz.invalid/mcp/oauth/callback";

pub struct PendingOAuthFlow {
    session: AuthorizationSession,
    pub authorization_url: String,
}

#[derive(Debug, Clone)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct OAuthTokenMaterial {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthFlowError {
    #[error("missing OAuth callback payload")]
    MissingCallback,
    #[error("OAuth callback payload must be a JSON object")]
    InvalidCallbackShape,
    #[error("OAuth callback is missing a non-empty `{0}` field")]
    MissingCallbackField(&'static str),
    #[error("MCP server does not advertise OAuth support")]
    NoAuthorizationSupport,
    #[error("OAuth flow failed: {0}")]
    Auth(String),
}

impl From<AuthError> for OAuthFlowError {
    fn from(value: AuthError) -> Self {
        match value {
            AuthError::NoAuthorizationSupport => Self::NoAuthorizationSupport,
            other => Self::Auth(other.to_string()),
        }
    }
}

pub async fn begin_authorization(
    server_url: &str,
    scopes: &[String],
    client_name: Option<&str>,
    client_metadata_url: Option<&str>,
) -> Result<PendingOAuthFlow, OAuthFlowError> {
    let mut auth_manager = AuthorizationManager::new(server_url).await?;
    let metadata = auth_manager.discover_metadata().await?;
    auth_manager.set_metadata(metadata);

    let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let session = AuthorizationSession::new(
        auth_manager,
        &scope_refs,
        DEFAULT_REDIRECT_URI,
        client_name,
        client_metadata_url,
    )
    .await?;
    let authorization_url = session.get_authorization_url().to_string();

    Ok(PendingOAuthFlow {
        session,
        authorization_url,
    })
}

pub fn callback_from_modifier(modifier: Option<Value>) -> Result<OAuthCallback, OAuthFlowError> {
    let modifier = modifier.ok_or(OAuthFlowError::MissingCallback)?;
    let object = modifier.as_object().ok_or(OAuthFlowError::InvalidCallbackShape)?;
    let code = object
        .get("code")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(OAuthFlowError::MissingCallbackField("code"))?;
    let state = object
        .get("state")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(OAuthFlowError::MissingCallbackField("state"))?;

    Ok(OAuthCallback {
        code: code.to_string(),
        state: state.to_string(),
    })
}

pub async fn exchange_callback(
    flow: &PendingOAuthFlow,
    callback: &OAuthCallback,
) -> Result<OAuthTokenMaterial, OAuthFlowError> {
    let token = flow.session.handle_callback(&callback.code, &callback.state).await?;
    let expires_at = token
        .expires_in()
        .and_then(|duration| Duration::from_std(duration).ok())
        .map(|duration| Utc::now() + duration);

    Ok(OAuthTokenMaterial {
        access_token: SecretString::new(token.access_token().secret().to_string().into_boxed_str()),
        refresh_token: token
            .refresh_token()
            .map(|token| SecretString::new(token.secret().to_string().into_boxed_str())),
        expires_at,
    })
}
