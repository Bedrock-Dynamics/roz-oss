use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::Deserialize;

/// Worker configuration loaded from environment variables and optional TOML.
///
/// Environment variables (prefixed with `ROZ_`):
/// - `ROZ_API_URL` — API base URL (required)
/// - `ROZ_NATS_URL` — NATS server URL (required)
/// - `ROZ_RESTATE_URL` — Restate ingress URL for task result callbacks (required)
/// - `ROZ_API_KEY` — API key for authenticating with the server (required)
/// - `ROZ_WORKER_ID` — unique worker ID (default: hostname)
/// - `ROZ_DATA_DIR` — local data directory for WAL (default: `/var/lib/roz`)
/// - `ROZ_GATEWAY_URL` — Model gateway base URL (default: `https://gateway-us.pydantic.dev`)
/// - `ROZ_GATEWAY_API_KEY` — Model gateway API key (required)
/// - `ROZ_MODEL_NAME` — Model identifier (default: `claude-sonnet-4-6`)
/// - `ROZ_MODEL_TIMEOUT_SECS` — Model HTTP request timeout in seconds (default: `120`)
/// - `ROZ_ANTHROPIC_PROVIDER` — PAIG proxy provider for Claude models (default: `anthropic`)
/// - `ROZ_ANTHROPIC_API_KEY` — Direct Anthropic API key; bypasses the PAIG gateway when set
/// - `ROZ_FALLBACK_MODEL` — Secondary model to use when the primary is rate-limited or overloaded
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerConfig {
    pub api_url: String,
    pub nats_url: String,
    pub restate_url: String,
    pub api_key: String,
    #[serde(default = "default_worker_id")]
    pub worker_id: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_gateway_url")]
    pub gateway_url: String,
    pub gateway_api_key: String,
    #[serde(default = "default_model_name")]
    pub model_name: String,
    #[serde(default = "default_model_timeout_secs")]
    pub model_timeout_secs: u64,
    #[serde(default = "default_anthropic_provider")]
    pub anthropic_provider: String,
    #[serde(default)]
    pub anthropic_api_key: Option<String>,
    /// Secondary model to use when the primary is rate-limited (429), unavailable (503),
    /// or overloaded (529). Set via `ROZ_FALLBACK_MODEL`.
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// Maximum joint velocity for the Copper safety filter (rad/s).
    /// Defaults to 1.5 rad/s if not specified. Set via `ROZ_MAX_VELOCITY`.
    #[serde(default)]
    pub max_velocity: Option<f64>,
    /// Path to a robot.toml or embodiment.toml manifest.
    /// When set, the worker parses the manifest and uploads the embodiment
    /// model to the server after host registration.
    /// Set via `ROZ_ROBOT_TOML` env var or `robot_toml` in roz-worker.toml.
    #[serde(default)]
    pub robot_toml: Option<String>,
    /// Maximum number of concurrently executing tasks before new work is rejected.
    #[serde(default = "default_max_concurrent_tasks")]
    pub max_concurrent_tasks: usize,
    /// Camera subsystem configuration.
    #[serde(default)]
    pub camera: CameraConfig,
    /// Optional Postgres URL for worker-side session turn persistence (DEBT-03).
    ///
    /// When `Some`, the worker connects directly to Postgres and spawns a
    /// [`roz_agent::agent_loop::TurnEmitter`] + flush task per `execute_task`
    /// invocation so agent turns are durably persisted to `roz_session_turns`.
    ///
    /// Fail-closed when unset: turn persistence is disabled, agent loop runs
    /// normally, no pool is created. Set via `ROZ_DATABASE_URL` (NOT
    /// `DATABASE_URL` — kept separate to avoid picking up unrelated server DB
    /// URLs in dev).
    #[serde(default)]
    pub database_url: Option<String>,
}

/// Camera subsystem configuration for the worker.
///
/// Controls whether the camera pipeline is started, which encoder to use,
/// and ICE (STUN/TURN) settings for WebRTC peer connections.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraConfig {
    #[serde(default = "default_camera_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub encoder: roz_core::camera::EncoderSelection,
    #[serde(default)]
    pub test_pattern: bool,
    #[serde(default = "default_stun_url")]
    pub stun_url: String,
    #[serde(default)]
    pub turn_url: Option<String>,
    #[serde(default)]
    pub turn_username: Option<String>,
    #[serde(default)]
    pub turn_credential: Option<String>,
    #[serde(default = "default_max_viewers")]
    pub max_viewers: usize,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            enabled: default_camera_enabled(),
            encoder: roz_core::camera::EncoderSelection::default(),
            test_pattern: false,
            stun_url: default_stun_url(),
            turn_url: None,
            turn_username: None,
            turn_credential: None,
            max_viewers: default_max_viewers(),
        }
    }
}

const fn default_camera_enabled() -> bool {
    cfg!(target_os = "linux")
}

fn default_stun_url() -> String {
    "stun:stun.l.google.com:19302".to_string()
}

const fn default_max_viewers() -> usize {
    10
}

fn default_worker_id() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "unknown".to_string())
}

fn default_data_dir() -> String {
    "/var/lib/roz".to_string()
}

fn default_gateway_url() -> String {
    "https://gateway-us.pydantic.dev".to_string()
}

fn default_model_name() -> String {
    "claude-sonnet-4-6".to_string()
}

const fn default_model_timeout_secs() -> u64 {
    120
}

const fn default_max_concurrent_tasks() -> usize {
    4
}

fn default_anthropic_provider() -> String {
    "anthropic".to_string()
}

impl WorkerConfig {
    /// Load configuration from environment variables (prefixed `ROZ_`) and
    /// an optional `roz-worker.toml` file.
    pub fn load() -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Toml::file("roz-worker.toml"))
            .merge(Env::prefixed("ROZ_"))
            .extract()
            .map_err(Box::new)
    }

    /// Load configuration from a specific figment (for testing).
    pub fn from_figment(figment: &Figment) -> Result<Self, Box<figment::Error>> {
        figment.extract().map_err(Box::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Serialized;

    /// Minimal required fields for all tests (includes gateway_api_key).
    fn base_config() -> serde_json::Value {
        serde_json::json!({
            "api_url": "http://localhost:8080",
            "nats_url": "nats://localhost:4222",
            "restate_url": "http://localhost:9080",
            "api_key": "roz_sk_test123",
            "gateway_api_key": "paig_test_key",
        })
    }

    #[test]
    fn config_loads_from_env_vars() {
        let mut vals = base_config();
        vals["worker_id"] = serde_json::json!("test-worker-1");
        vals["data_dir"] = serde_json::json!("/tmp/roz-test");
        vals["gateway_url"] = serde_json::json!("https://custom-gw.example.com");
        vals["model_name"] = serde_json::json!("claude-haiku-4-5");
        vals["model_timeout_secs"] = serde_json::json!(60);

        let figment = Figment::new().merge(Serialized::defaults(vals));
        let config = WorkerConfig::from_figment(&figment).unwrap();
        assert_eq!(config.api_url, "http://localhost:8080");
        assert_eq!(config.nats_url, "nats://localhost:4222");
        assert_eq!(config.restate_url, "http://localhost:9080");
        assert_eq!(config.api_key, "roz_sk_test123");
        assert_eq!(config.worker_id, "test-worker-1");
        assert_eq!(config.data_dir, "/tmp/roz-test");
        assert_eq!(config.gateway_url, "https://custom-gw.example.com");
        assert_eq!(config.gateway_api_key, "paig_test_key");
        assert_eq!(config.model_name, "claude-haiku-4-5");
        assert_eq!(config.model_timeout_secs, 60);
        assert_eq!(config.max_concurrent_tasks, 4);
    }

    #[test]
    fn config_defaults_worker_id_and_data_dir() {
        let figment = Figment::new().merge(Serialized::defaults(base_config()));
        let config = WorkerConfig::from_figment(&figment).unwrap();
        assert_eq!(config.api_url, "http://localhost:8080");
        assert_eq!(config.nats_url, "nats://localhost:4222");
        assert_eq!(config.restate_url, "http://localhost:9080");
        assert_eq!(config.api_key, "roz_sk_test123");
        // worker_id should default to hostname (non-empty)
        assert!(!config.worker_id.is_empty());
        assert_eq!(config.data_dir, "/var/lib/roz");
    }

    #[test]
    fn config_defaults_model_provider_fields() {
        let figment = Figment::new().merge(Serialized::defaults(base_config()));
        let config = WorkerConfig::from_figment(&figment).unwrap();
        assert_eq!(config.gateway_url, "https://gateway-us.pydantic.dev");
        assert_eq!(config.model_name, "claude-sonnet-4-6");
        assert_eq!(config.model_timeout_secs, 120);
        assert_eq!(config.gateway_api_key, "paig_test_key");
        assert_eq!(config.max_concurrent_tasks, 4);
    }

    #[test]
    fn missing_api_url_gives_clear_error() {
        let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
            "nats_url": "nats://localhost:4222",
            "restate_url": "http://localhost:9080",
            "api_key": "roz_sk_test123",
            "gateway_api_key": "paig_test_key",
        })));

        let err = *WorkerConfig::from_figment(&figment).unwrap_err();
        let err_string = err.to_string();
        assert!(
            err_string.contains("api_url"),
            "error should mention missing field 'api_url', got: {err_string}"
        );
    }

    #[test]
    fn missing_nats_url_gives_clear_error() {
        let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
            "api_url": "http://localhost:8080",
            "restate_url": "http://localhost:9080",
            "api_key": "roz_sk_test123",
            "gateway_api_key": "paig_test_key",
        })));

        let err = *WorkerConfig::from_figment(&figment).unwrap_err();
        let err_string = err.to_string();
        assert!(
            err_string.contains("nats_url"),
            "error should mention missing field 'nats_url', got: {err_string}"
        );
    }

    #[test]
    fn config_includes_restate_url_and_api_key() {
        let figment = Figment::new()
            .merge(("api_url", "http://localhost:3000"))
            .merge(("nats_url", "nats://localhost:4222"))
            .merge(("worker_id", "test-worker"))
            .merge(("data_dir", "/tmp/roz"))
            .merge(("restate_url", "http://localhost:8080"))
            .merge(("api_key", "roz_sk_test"))
            .merge(("gateway_api_key", "paig_test"));
        let config = WorkerConfig::from_figment(&figment).unwrap();
        assert_eq!(config.restate_url, "http://localhost:8080");
        assert_eq!(config.api_key, "roz_sk_test");
    }

    #[test]
    fn missing_restate_url_gives_clear_error() {
        let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
            "api_url": "http://localhost:8080",
            "nats_url": "nats://localhost:4222",
            "api_key": "roz_sk_test123",
            "gateway_api_key": "paig_test_key",
        })));

        let err = *WorkerConfig::from_figment(&figment).unwrap_err();
        let err_string = err.to_string();
        assert!(
            err_string.contains("restate_url"),
            "error should mention missing field 'restate_url', got: {err_string}"
        );
    }

    #[test]
    fn missing_api_key_gives_clear_error() {
        let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
            "api_url": "http://localhost:8080",
            "nats_url": "nats://localhost:4222",
            "restate_url": "http://localhost:9080",
            "gateway_api_key": "paig_test_key",
        })));

        let err = *WorkerConfig::from_figment(&figment).unwrap_err();
        let err_string = err.to_string();
        assert!(
            err_string.contains("api_key"),
            "error should mention missing field 'api_key', got: {err_string}"
        );
    }

    #[test]
    fn missing_gateway_api_key_gives_clear_error() {
        let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
            "api_url": "http://localhost:8080",
            "nats_url": "nats://localhost:4222",
            "restate_url": "http://localhost:9080",
            "api_key": "roz_sk_test123",
        })));

        let err = *WorkerConfig::from_figment(&figment).unwrap_err();
        let err_string = err.to_string();
        assert!(
            err_string.contains("gateway_api_key"),
            "error should mention missing field 'gateway_api_key', got: {err_string}"
        );
    }

    #[test]
    fn config_supports_max_concurrent_tasks_override() {
        let figment = Figment::new()
            .merge(Serialized::defaults(base_config()))
            .merge(("max_concurrent_tasks", 9));
        let config = WorkerConfig::from_figment(&figment).unwrap();
        assert_eq!(config.max_concurrent_tasks, 9);
    }

    #[test]
    fn config_robot_toml_defaults_to_none() {
        let figment = Figment::new().merge(Serialized::defaults(base_config()));
        let config = WorkerConfig::from_figment(&figment).unwrap();
        assert!(config.robot_toml.is_none());
    }

    #[test]
    fn config_robot_toml_loads_when_set() {
        let mut vals = base_config();
        vals["robot_toml"] = serde_json::json!("/etc/roz/robot.toml");
        let figment = Figment::new().merge(Serialized::defaults(vals));
        let config = WorkerConfig::from_figment(&figment).unwrap();
        assert_eq!(config.robot_toml.as_deref(), Some("/etc/roz/robot.toml"));
    }
}
