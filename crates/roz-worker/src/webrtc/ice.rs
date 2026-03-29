/// ICE/TURN configuration for WebRTC peer connections.
///
/// Provides STUN and optional TURN server configuration loaded
/// from environment variables or sensible defaults.
#[derive(Debug, Clone)]
pub struct IceConfig {
    /// STUN server URL (e.g. `"stun:stun.l.google.com:19302"`).
    pub stun_url: Option<String>,
    /// TURN server URL (e.g. `"turn:turn.example.com:3478"`).
    pub turn_url: Option<String>,
    /// TURN username credential.
    pub turn_username: Option<String>,
    /// TURN password credential.
    pub turn_credential: Option<String>,
}

const DEFAULT_STUN_URL: &str = "stun:stun.l.google.com:19302";

impl Default for IceConfig {
    fn default() -> Self {
        Self {
            stun_url: Some(DEFAULT_STUN_URL.to_string()),
            turn_url: None,
            turn_username: None,
            turn_credential: None,
        }
    }
}

impl IceConfig {
    /// Load ICE config from environment variables.
    ///
    /// Reads:
    /// - `ROZ_STUN_URL` (falls back to Google STUN)
    /// - `ROZ_TURN_URL`
    /// - `ROZ_TURN_USERNAME`
    /// - `ROZ_TURN_CREDENTIAL`
    pub fn from_env() -> Self {
        Self::from_env_vars(
            std::env::var("ROZ_STUN_URL").ok(),
            std::env::var("ROZ_TURN_URL").ok(),
            std::env::var("ROZ_TURN_USERNAME").ok(),
            std::env::var("ROZ_TURN_CREDENTIAL").ok(),
        )
    }

    /// Construct from explicit env var values (testable without `set_var`).
    ///
    /// If `stun_url` is `None`, falls back to the default Google STUN server.
    fn from_env_vars(
        stun_url: Option<String>,
        turn_url: Option<String>,
        turn_username: Option<String>,
        turn_credential: Option<String>,
    ) -> Self {
        Self {
            stun_url: stun_url.or_else(|| Some(DEFAULT_STUN_URL.to_string())),
            turn_url,
            turn_username,
            turn_credential,
        }
    }

    /// Whether a TURN relay server is configured.
    #[must_use]
    pub const fn has_turn(&self) -> bool {
        self.turn_url.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_google_stun() {
        let config = IceConfig::default();
        assert_eq!(config.stun_url.as_deref(), Some("stun:stun.l.google.com:19302"));
        assert!(config.turn_url.is_none());
        assert!(config.turn_username.is_none());
        assert!(config.turn_credential.is_none());
        assert!(!config.has_turn());
    }

    #[test]
    fn from_env_vars_reads_turn() {
        let config = IceConfig::from_env_vars(
            Some("stun:custom.example.com:3478".to_string()),
            Some("turn:relay.example.com:3478".to_string()),
            Some("user1".to_string()),
            Some("pass1".to_string()),
        );
        assert_eq!(config.stun_url.as_deref(), Some("stun:custom.example.com:3478"));
        assert_eq!(config.turn_url.as_deref(), Some("turn:relay.example.com:3478"));
        assert_eq!(config.turn_username.as_deref(), Some("user1"));
        assert_eq!(config.turn_credential.as_deref(), Some("pass1"));
        assert!(config.has_turn());
    }

    #[test]
    fn from_env_vars_falls_back_to_default_stun() {
        let config = IceConfig::from_env_vars(None, None, None, None);
        assert_eq!(config.stun_url.as_deref(), Some("stun:stun.l.google.com:19302"));
        assert!(!config.has_turn());
    }

    #[test]
    fn has_turn_false_without_url() {
        let config = IceConfig {
            stun_url: Some("stun:example.com:3478".to_string()),
            turn_url: None,
            turn_username: Some("user".to_string()),
            turn_credential: Some("pass".to_string()),
        };
        assert!(!config.has_turn(), "has_turn should be false when turn_url is None");
    }

    #[test]
    fn has_turn_true_with_url() {
        let config = IceConfig {
            stun_url: None,
            turn_url: Some("turn:relay.example.com:3478".to_string()),
            turn_username: None,
            turn_credential: None,
        };
        assert!(config.has_turn(), "has_turn should be true when turn_url is Some");
    }
}
