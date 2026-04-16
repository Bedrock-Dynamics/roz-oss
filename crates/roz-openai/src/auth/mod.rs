//! Authentication for OpenAI-compatible endpoints.
//!
//! Two auth modes are supported by Phase 19 Plan 19-05:
//!
//! - **`ApiKeyAuth`** ([`api_key`]) — static bearer token (OpenAI API key,
//!   vLLM-style key, or any opaque secret).
//! - **`OAuthAuth`** ([`oauth`]) — ChatGPT-style OAuth with refresh-token
//!   rotation (Tasks 2–3 populate this).
//!
//! Both implement the [`AuthProvider`] trait so callers can be auth-mode
//! agnostic at the request-construction boundary.
//!
//! [`null_key::NullKeyProvider`] is a `KeyProvider` (NOT `AuthProvider`)
//! test helper: it lives here so Plan 19-11 can import it as the bootstrap
//! fallback when `ROZ_ENCRYPTION_KEY` is unset.

pub mod api_key;
pub mod null_key;
pub mod oauth;
pub mod pkce;
pub mod server;
pub mod token_data;

use async_trait::async_trait;
use secrecy::SecretString;

/// Errors surfaced by all auth providers in this module.
///
/// Variants are coarse to avoid leaking secrets into error messages or logs.
#[derive(thiserror::Error, Debug)]
pub enum AuthError {
    #[error("token refresh failed: {0}")]
    TokenRefreshFailed(String),
    #[error("invalid JWT: {0}")]
    InvalidJwt(String),
    #[error("HTTP error: {0}")]
    HttpError(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("operation timed out")]
    Timeout,
    #[error("OAuth callback error: {0}")]
    CallbackError(String),
}

/// Source of bearer credentials for an OpenAI-compatible endpoint.
///
/// Implementations MUST be cheap-clone (typically by storing keys in
/// `Arc<SecretString>` or behind an internal `Arc<RwLock>`).
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Return the current bearer token. Implementations should NOT trigger a
    /// refresh as a side effect; orchestrate refresh via dedicated methods on
    /// the concrete impl (e.g. [`oauth::OAuthAuth::refresh_if_needed`]).
    async fn bearer_token(&self) -> Result<SecretString, AuthError>;

    /// Optional ChatGPT account id, surfaced for the `chatgpt-account-id`
    /// header on ChatGPT-backend Responses calls. `None` for API-key auth.
    fn account_id(&self) -> Option<&str>;

    /// `true` when the upstream is the ChatGPT backend (Codex CLI parity);
    /// `false` for plain OpenAI API key, vLLM/Ollama, etc.
    fn is_chatgpt_backend(&self) -> bool;
}
