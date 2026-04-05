use roz_core::recording::RecordingManifest;
use std::collections::HashMap;

/// Reads and parses recorded data (in-memory representation for pure-logic testing).
pub struct RecordingReader {
    manifest: RecordingManifest,
    channel_data: HashMap<String, Vec<Vec<u8>>>,
}

impl RecordingReader {
    /// Open a recording from a manifest and pre-loaded channel data.
    pub const fn open(manifest: RecordingManifest, channel_data: HashMap<String, Vec<Vec<u8>>>) -> Self {
        Self { manifest, channel_data }
    }

    /// Get the recording manifest.
    pub const fn manifest(&self) -> &RecordingManifest {
        &self.manifest
    }

    /// Get raw channel data.
    pub fn channel_data(&self, channel_name: &str) -> Option<&Vec<Vec<u8>>> {
        self.channel_data.get(channel_name)
    }

    /// Interpret channel data as f64 values (little-endian 8-byte doubles).
    pub fn channel_as_f64(&self, channel_name: &str) -> Option<Vec<f64>> {
        let data = self.channel_data.get(channel_name)?;
        let values: Vec<f64> = data
            .iter()
            .filter_map(|bytes| {
                if bytes.len() >= 8 {
                    Some(f64::from_le_bytes(bytes[..8].try_into().ok()?))
                } else {
                    None
                }
            })
            .collect();
        Some(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use roz_core::recording::{RecordingChannel, RecordingSource};
    use uuid::Uuid;

    fn sample_manifest() -> RecordingManifest {
        RecordingManifest {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            environment_id: Uuid::new_v4(),
            host_id: Uuid::new_v4(),
            source: RecordingSource::Simulation,
            channels: vec![RecordingChannel {
                name: "velocity".to_string(),
                topic: "/vel".to_string(),
                schema_name: "Float64".to_string(),
                message_count: 3,
            }],
            duration_secs: 10.0,
            created_at: Utc::now(),
        }
    }

    fn f64_to_bytes(v: f64) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }

    #[test]
    fn open_and_read_manifest() {
        let manifest = sample_manifest();
        let reader = RecordingReader::open(manifest.clone(), HashMap::new());
        assert_eq!(reader.manifest().id, manifest.id);
    }

    #[test]
    fn channel_data_raw() {
        let manifest = sample_manifest();
        let mut data = HashMap::new();
        data.insert("velocity".to_string(), vec![vec![1, 2, 3], vec![4, 5, 6]]);

        let reader = RecordingReader::open(manifest, data);
        let raw = reader.channel_data("velocity").unwrap();
        assert_eq!(raw.len(), 2);
    }

    #[test]
    fn channel_as_f64_parses_correctly() {
        let manifest = sample_manifest();
        let mut data = HashMap::new();
        data.insert(
            "velocity".to_string(),
            vec![f64_to_bytes(1.5), f64_to_bytes(2.7), f64_to_bytes(3.15)],
        );

        let reader = RecordingReader::open(manifest, data);
        let values = reader.channel_as_f64("velocity").unwrap();
        assert_eq!(values.len(), 3);
        assert!((values[0] - 1.5).abs() < f64::EPSILON);
        assert!((values[1] - 2.7).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_channel_returns_none() {
        let manifest = sample_manifest();
        let reader = RecordingReader::open(manifest, HashMap::new());
        assert!(reader.channel_data("nonexistent").is_none());
        assert!(reader.channel_as_f64("nonexistent").is_none());
    }

    #[test]
    fn short_bytes_skipped_in_f64_parse() {
        let manifest = sample_manifest();
        let mut data = HashMap::new();
        data.insert(
            "velocity".to_string(),
            vec![f64_to_bytes(1.0), vec![1, 2, 3]], // second entry too short
        );

        let reader = RecordingReader::open(manifest, data);
        let values = reader.channel_as_f64("velocity").unwrap();
        assert_eq!(values.len(), 1); // only the valid entry
    }

    #[test]
    fn empty_channel_data() {
        let manifest = sample_manifest();
        let mut data = HashMap::new();
        data.insert("velocity".to_string(), vec![]);

        let reader = RecordingReader::open(manifest, data);
        let values = reader.channel_as_f64("velocity").unwrap();
        assert!(values.is_empty());
    }
}
