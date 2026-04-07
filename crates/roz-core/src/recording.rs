use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// RecordingSource
// ---------------------------------------------------------------------------

/// The origin of an MCAP recording — physical hardware, simulation, or a mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingSource {
    Simulation,
    Physical,
    Hybrid,
}

// ---------------------------------------------------------------------------
// RecordingChannel
// ---------------------------------------------------------------------------

/// Describes a single channel within an MCAP recording file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingChannel {
    pub name: String,
    pub topic: String,
    pub schema_name: String,
    pub message_count: u64,
}

// ---------------------------------------------------------------------------
// RecordingManifest
// ---------------------------------------------------------------------------

/// Top-level manifest for an MCAP recording, capturing run context and
/// the channels it contains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingManifest {
    pub id: Uuid,
    pub run_id: Uuid,
    pub environment_id: Uuid,
    pub host_id: Uuid,
    pub source: RecordingSource,
    pub channels: Vec<RecordingChannel>,
    pub duration_secs: f64,
    pub created_at: DateTime<Utc>,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn sample_channel(name: &str, topic: &str) -> RecordingChannel {
        RecordingChannel {
            name: name.to_string(),
            topic: topic.to_string(),
            schema_name: format!("{name}_schema"),
            message_count: 1000,
        }
    }

    fn sample_manifest() -> RecordingManifest {
        RecordingManifest {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            environment_id: Uuid::new_v4(),
            host_id: Uuid::new_v4(),
            source: RecordingSource::Physical,
            channels: vec![
                sample_channel("lidar", "/sensors/lidar"),
                sample_channel("imu", "/sensors/imu"),
            ],
            duration_secs: 120.5,
            created_at: Utc::now(),
        }
    }

    // -----------------------------------------------------------------------
    // RecordingSource serde
    // -----------------------------------------------------------------------

    #[test]
    fn recording_source_serde_roundtrip() {
        for source in [
            RecordingSource::Simulation,
            RecordingSource::Physical,
            RecordingSource::Hybrid,
        ] {
            let json = serde_json::to_string(&source).unwrap();
            let restored: RecordingSource = serde_json::from_str(&json).unwrap();
            assert_eq!(source, restored);
        }
    }

    #[test]
    fn recording_source_variants_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&RecordingSource::Simulation).unwrap(),
            "\"simulation\""
        );
        assert_eq!(
            serde_json::to_string(&RecordingSource::Physical).unwrap(),
            "\"physical\""
        );
        assert_eq!(serde_json::to_string(&RecordingSource::Hybrid).unwrap(), "\"hybrid\"");
    }

    // -----------------------------------------------------------------------
    // RecordingChannel serde
    // -----------------------------------------------------------------------

    #[test]
    fn channel_manifest_serde_roundtrip() {
        let original = sample_channel("camera", "/sensors/camera");
        let json = serde_json::to_string(&original).unwrap();
        let restored: RecordingChannel = serde_json::from_str(&json).unwrap();

        assert_eq!(original.name, restored.name);
        assert_eq!(original.topic, restored.topic);
        assert_eq!(original.schema_name, restored.schema_name);
        assert_eq!(original.message_count, restored.message_count);
    }

    // -----------------------------------------------------------------------
    // RecordingManifest serde
    // -----------------------------------------------------------------------

    #[test]
    fn recording_manifest_serde_roundtrip() {
        let original = sample_manifest();
        let json = serde_json::to_string(&original).unwrap();
        let restored: RecordingManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(original.id, restored.id);
        assert_eq!(original.run_id, restored.run_id);
        assert_eq!(original.environment_id, restored.environment_id);
        assert_eq!(original.host_id, restored.host_id);
        assert_eq!(original.source, restored.source);
        assert_eq!(original.channels.len(), restored.channels.len());
        assert!((original.duration_secs - restored.duration_secs).abs() < f64::EPSILON);
    }

    #[test]
    fn recording_manifest_with_multiple_channels() {
        let manifest = RecordingManifest {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            environment_id: Uuid::new_v4(),
            host_id: Uuid::new_v4(),
            source: RecordingSource::Hybrid,
            channels: vec![
                sample_channel("lidar", "/sensors/lidar"),
                sample_channel("imu", "/sensors/imu"),
                sample_channel("camera", "/sensors/camera"),
                sample_channel("gps", "/sensors/gps"),
            ],
            duration_secs: 300.0,
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let restored: RecordingManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.channels.len(), 4);
        assert_eq!(restored.channels[0].name, "lidar");
        assert_eq!(restored.channels[1].name, "imu");
        assert_eq!(restored.channels[2].name, "camera");
        assert_eq!(restored.channels[3].name, "gps");
    }

    #[test]
    fn recording_manifest_json_has_expected_field_names() {
        let manifest = sample_manifest();
        let json = serde_json::to_string(&manifest).unwrap();

        assert!(json.contains("\"id\""));
        assert!(json.contains("\"run_id\""));
        assert!(json.contains("\"environment_id\""));
        assert!(json.contains("\"host_id\""));
        assert!(json.contains("\"source\""));
        assert!(json.contains("\"channels\""));
        assert!(json.contains("\"duration_secs\""));
        assert!(json.contains("\"created_at\""));
    }
}
