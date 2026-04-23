//! Phase 26.5 SC6: worker observability configuration.
//!
//! Plan 05 created this module with the `RecordMode` enum so the camera
//! MCAP relay could compile against it. Plan 07 extends this file with
//! the full `ObservabilityConfig` + `ObservabilityCameraConfig` structs
//! that `WorkerConfig` wires into figment loading.
//!
//! # Environment variable mapping (research Q6; R-05)
//!
//! Nested figment env vars use the double-underscore separator:
//!   * `ROZ_OBSERVABILITY__CAMERA__RECORD`              = `"keyframes"` | `"full"` | `"off"`
//!   * `ROZ_OBSERVABILITY__CAMERA__KEYFRAME_INTERVAL_SECS` = f32 seconds (default 2.0)
//!
//! Single underscores will NOT parse — this is a figment quirk, not a
//! roz decision. Unit tests in `crates/roz-worker/src/config.rs` assert
//! the nested path loads.
//!
//! # Keyframe interval hint
//!
//! The `keyframe_interval_secs` field is stored but NOT enforced this
//! phase — openh264's default IDR cadence governs (research Q5 / Q8).
//! Stored for a future phase that wires `StreamHub::request_keyframe`
//! or an encoder-level control channel.

/// Per-camera record-mode policy for the MCAP relay.
///
/// * `Off` — relay task not spawned. Default in production is `Keyframes`
///   (set in Plan 07).
/// * `Keyframes` — forward only frames where `EncodedFrame.is_keyframe == true`.
///   Bandwidth-friendly; suitable for review / debugging.
/// * `Full` — forward every frame. High bandwidth; requires fast NATS +
///   server-side MCAP disk; not recommended over WAN.
#[derive(Debug, Default, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecordMode {
    #[default]
    Keyframes,
    Full,
    Off,
}

/// Phase 26.5 SC6 per-worker camera MCAP recording config.
///
/// TOML example:
///
/// ```toml
/// [observability.camera]
/// record = "keyframes"             # "keyframes" | "full" | "off"
/// keyframe_interval_secs = 2.0
/// ```
///
/// See the module-level docs for the figment env-var naming.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ObservabilityCameraConfig {
    #[serde(default = "default_record_mode")]
    pub record: RecordMode,
    /// Hint only this phase — the relay does NOT force IDR frames at this
    /// cadence (research Q5 / Q8). openh264's default IDR interval
    /// governs. Stored for a future phase that wires
    /// `StreamHub::request_keyframe` or an encoder-level control channel.
    #[serde(default = "default_keyframe_interval_secs")]
    pub keyframe_interval_secs: f32,
}

impl Default for ObservabilityCameraConfig {
    fn default() -> Self {
        Self {
            record: default_record_mode(),
            keyframe_interval_secs: default_keyframe_interval_secs(),
        }
    }
}

/// Top-level observability config bag. Extends additively in future phases
/// (e.g. metrics exporter settings, log sink overrides).
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub camera: ObservabilityCameraConfig,
}

const fn default_record_mode() -> RecordMode {
    RecordMode::Keyframes
}

const fn default_keyframe_interval_secs() -> f32 {
    2.0
}

#[cfg(test)]
mod tests {
    use super::{ObservabilityCameraConfig, ObservabilityConfig, RecordMode};

    #[test]
    fn record_mode_serde_lowercase_roundtrip() {
        for mode in [RecordMode::Keyframes, RecordMode::Full, RecordMode::Off] {
            let json = serde_json::to_string(&mode).expect("serialize");
            let back: RecordMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn record_mode_default_is_keyframes() {
        assert_eq!(RecordMode::default(), RecordMode::Keyframes);
    }

    #[test]
    fn record_mode_deserializes_from_lowercase_strings() {
        assert_eq!(
            serde_json::from_str::<RecordMode>("\"keyframes\"").unwrap(),
            RecordMode::Keyframes
        );
        assert_eq!(
            serde_json::from_str::<RecordMode>("\"full\"").unwrap(),
            RecordMode::Full
        );
        assert_eq!(serde_json::from_str::<RecordMode>("\"off\"").unwrap(), RecordMode::Off);
    }

    // Phase 26.5 SC6 additions below:

    #[test]
    fn observability_camera_config_defaults() {
        let cfg = ObservabilityCameraConfig::default();
        assert_eq!(cfg.record, RecordMode::Keyframes);
        assert!((cfg.keyframe_interval_secs - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn observability_config_defaults_to_keyframes_2s() {
        let cfg = ObservabilityConfig::default();
        assert_eq!(cfg.camera.record, RecordMode::Keyframes);
        assert!((cfg.camera.keyframe_interval_secs - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn observability_camera_config_toml_parses_override() {
        let toml_src = r#"
            record = "full"
            keyframe_interval_secs = 5.0
        "#;
        let cfg: ObservabilityCameraConfig = toml::from_str(toml_src).expect("parse");
        assert_eq!(cfg.record, RecordMode::Full);
        assert!((cfg.keyframe_interval_secs - 5.0).abs() < f32::EPSILON);
    }

    #[test]
    fn observability_config_toml_parses_nested() {
        let toml_src = r#"
            [camera]
            record = "off"
            keyframe_interval_secs = 10.0
        "#;
        let cfg: ObservabilityConfig = toml::from_str(toml_src).expect("parse");
        assert_eq!(cfg.camera.record, RecordMode::Off);
        assert!((cfg.camera.keyframe_interval_secs - 10.0).abs() < f32::EPSILON);
    }
}
