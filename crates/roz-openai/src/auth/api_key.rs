//! Static API-key [`AuthProvider`] for OpenAI-compatible endpoints.
//!
//! Holds a [`SecretString`] inside an [`Arc`] so the provider is cheap to
//! clone across request builders. The bearer token is returned by cloning
//! the underlying secret (which is itself reference-counted).

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};

use super::{AuthError, AuthProvider};

/// API-key auth: returns a static bearer token forever.
#[derive(Clone)]
pub struct ApiKeyAuth {
    key: Arc<SecretString>,
}

impl ApiKeyAuth {
    /// Wrap a [`SecretString`] as an [`AuthProvider`].
    #[must_use]
    pub fn new(key: SecretString) -> Self {
        Self { key: Arc::new(key) }
    }
}

// Manual Debug to avoid leaking the API key.
impl std::fmt::Debug for ApiKeyAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuth").field("key", &"<redacted>").finish()
    }
}

#[async_trait]
impl AuthProvider for ApiKeyAuth {
    async fn bearer_token(&self) -> Result<SecretString, AuthError> {
        // Clone via the inner String — SecretString does not impl Clone.
        Ok(SecretString::from(self.key.expose_secret().to_string()))
    }

    fn account_id(&self) -> Option<&str> {
        None
    }

    fn is_chatgpt_backend(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn api_key_auth_returns_token() {
        let auth = ApiKeyAuth::new(SecretString::from("sk-test-123".to_string()));
        let token = auth.bearer_token().await.expect("token");
        assert_eq!(token.expose_secret(), "sk-test-123");
    }

    #[tokio::test]
    async fn api_key_auth_not_chatgpt_backend() {
        let auth = ApiKeyAuth::new(SecretString::from("sk-test-123".to_string()));
        assert!(!auth.is_chatgpt_backend());
        assert!(auth.account_id().is_none());
    }

    #[test]
    fn api_key_auth_debug_redacts() {
        let auth = ApiKeyAuth::new(SecretString::from("sk-LIVE-VALUE-XYZ".to_string()));
        let dbg = format!("{auth:?}");
        assert!(!dbg.contains("sk-LIVE-VALUE-XYZ"), "got: {dbg}");
        assert!(dbg.contains("<redacted>"), "got: {dbg}");
    }
}
