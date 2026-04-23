//! Phase 26.5 SC6: record-mode policy enum for the camera MCAP relay.
//!
//! This module is created by Plan 05 so Plan 05's `mcap_relay.rs` can
//! import `RecordMode` at compile time. Plan 07 extends this file with
//! the full `ObservabilityConfig` + `ObservabilityCameraConfig` that
//! `WorkerConfig` wires into figment loading.

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

#[cfg(test)]
mod tests {
    use super::RecordMode;

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
}
