/// Data classification for privacy-aware routing.
///
/// Sensitive data (camera feeds, audio, PII) should only be processed
/// by local models. Safe data (joint positions, sensor readings) can
/// go to cloud models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Safe,
    Sensitive,
}

pub struct DataClassification;

impl DataClassification {
    pub fn classify(data_type: &str) -> Classification {
        let lower = data_type.to_lowercase();
        if lower.contains("camera")
            || lower.contains("rgb")
            || lower.contains("image")
            || lower.contains("video")
            || lower.contains("audio")
            || lower.contains("microphone")
            || lower.contains("voice")
            || lower.contains("face")
            || lower.contains("person")
            || lower.contains("pii")
        {
            Classification::Sensitive
        } else {
            Classification::Safe
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_camera_data_as_sensitive() {
        assert_eq!(DataClassification::classify("camera_rgb"), Classification::Sensitive);
    }

    #[test]
    fn classify_joint_position_as_safe() {
        assert_eq!(DataClassification::classify("joint_position"), Classification::Safe);
    }

    #[test]
    fn classify_audio_as_sensitive() {
        assert_eq!(
            DataClassification::classify("microphone_stream"),
            Classification::Sensitive
        );
    }

    #[test]
    fn classify_imu_as_safe() {
        assert_eq!(DataClassification::classify("imu_acceleration"), Classification::Safe);
    }
}
