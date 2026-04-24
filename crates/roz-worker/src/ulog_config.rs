//! Phase 26.8 SC2 worker ulog auto-download config (D-07).
//!
//! TOML example (roz-worker.toml):
//!
//! ```toml
//! [ulog]
//! enabled = true                  # default; set false to opt out
//! download_timeout_secs = 60      # per-download bound (D-04 spawn timeout)
//! keep_fc_copy = false            # default; true skips LOG_ERASE post-upload
//! ```
//!
//! Env var path (figment `__` nesting; already wired in `WorkerConfig::load`):
//!
//!   `ROZ_ULOG__ENABLED=false`
//!   `ROZ_ULOG__DOWNLOAD_TIMEOUT_SECS=120`
//!   `ROZ_ULOG__KEEP_FC_COPY=true`
//!
//! # Placement note (D-07)
//!
//! `UlogConfig` lives top-level on [`crate::config::WorkerConfig`] — NOT
//! nested under `observability`. Ulog auto-download is a distinct subsystem
//! from MCAP/copper observability and deserves its own operator-facing
//! config surface.

/// Phase 26.8 D-07 — ulog auto-download controls.
///
/// Three fields control the session-finalize MAVLink log download hook.
/// Absent section yields [`Self::default`]: `enabled = true`,
/// `download_timeout_secs = 60`, `keep_fc_copy = false`.
///
/// # Fields
/// - `enabled`: master switch. When `false`, the finalize-hook is a silent
///   no-op regardless of MAVLink backend presence.
/// - `download_timeout_secs`: outer wall-clock bound on the finalize spawn
///   task. Inner MAVLink protocol state-machine has its own per-stage
///   timeouts (Plan 02); this is the belt-and-braces cap.
/// - `keep_fc_copy`: when `false` (default), the worker issues `LOG_ERASE`
///   on the FC after a verified upload (D-06). When `true`, erase is
///   skipped and logs accumulate on the FC until manual cleanup.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct UlogConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_download_timeout_secs")]
    pub download_timeout_secs: u64,
    #[serde(default)]
    pub keep_fc_copy: bool,
}

impl Default for UlogConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            download_timeout_secs: default_download_timeout_secs(),
            keep_fc_copy: false,
        }
    }
}

const fn default_enabled() -> bool {
    true
}

const fn default_download_timeout_secs() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::UlogConfig;

    #[test]
    fn ulog_config_defaults() {
        let cfg = UlogConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.download_timeout_secs, 60);
        assert!(!cfg.keep_fc_copy);
    }

    #[test]
    fn ulog_config_toml_parses_overrides() {
        let toml_src = r#"
            enabled = false
            download_timeout_secs = 120
            keep_fc_copy = true
        "#;
        let cfg: UlogConfig = toml::from_str(toml_src).expect("parse");
        assert!(!cfg.enabled);
        assert_eq!(cfg.download_timeout_secs, 120);
        assert!(cfg.keep_fc_copy);
    }

    #[test]
    fn ulog_config_toml_partial_uses_defaults() {
        let toml_src = r#"
            enabled = false
        "#;
        let cfg: UlogConfig = toml::from_str(toml_src).expect("parse");
        assert!(!cfg.enabled);
        assert_eq!(cfg.download_timeout_secs, 60);
        assert!(!cfg.keep_fc_copy);
    }
}
