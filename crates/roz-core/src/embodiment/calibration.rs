use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::frame_tree::Transform3D;

/// Per-sensor calibration data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorCalibration {
    pub sensor_id: String,
    pub offset: Vec<f64>,
    pub scale: Option<Vec<f64>>,
    pub calibrated_at: DateTime<Utc>,
}

/// A versioned calibration overlay applied on top of the base embodiment model.
///
/// Calibration is a compiled overlay, not side metadata. The `EmbodimentRuntime`
/// is built from base model + calibration + safety overlay. Stale calibration
/// degrades trust immediately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationOverlay {
    pub calibration_id: String,
    pub calibration_digest: String,
    pub calibrated_at: DateTime<Utc>,
    pub stale_after: Option<DateTime<Utc>>,

    /// Joint zero position corrections (applied on top of URDF nominal).
    #[serde(default)]
    pub joint_offsets: BTreeMap<String, f64>,

    /// Frame transform corrections (extrinsic calibration).
    #[serde(default)]
    pub frame_corrections: BTreeMap<String, Transform3D>,

    /// Per-sensor calibration data.
    #[serde(default)]
    pub sensor_calibrations: BTreeMap<String, SensorCalibration>,

    /// Valid operating temperature range (optional).
    pub temperature_range: Option<(f64, f64)>,

    /// Must match the base model's digest.
    pub valid_for_model_digest: String,
}

impl CalibrationOverlay {
    /// Compute the SHA-256 digest of this overlay's canonical JSON serialization.
    #[must_use]
    pub fn compute_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hashable = self.clone();
        hashable.calibration_digest = String::new();
        let canonical = serde_json::to_string(&hashable).expect("CalibrationOverlay must serialize");
        let hash = Sha256::digest(canonical.as_bytes());
        hex::encode(hash)
    }

    /// Compute and set the `calibration_digest` field.
    pub fn stamp_digest(&mut self) {
        self.calibration_digest = self.compute_digest();
    }

    /// Check if this calibration is stale.
    #[must_use]
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        self.stale_after.is_some_and(|stale| now >= stale)
    }

    /// Check if this calibration is valid for the given model digest.
    /// Verifies that: (1) the calibration has a stamped digest, (2) the
    /// stamped digest matches recomputation, (3) it targets the given model.
    #[must_use]
    pub fn is_valid_for_model(&self, model_digest: &str) -> bool {
        !self.calibration_digest.is_empty()
            && self.calibration_digest == self.compute_digest()
            && self.valid_for_model_digest == model_digest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_overlay() -> CalibrationOverlay {
        CalibrationOverlay {
            calibration_id: "cal-001".into(),
            calibration_digest: "sha256_abc".into(),
            calibrated_at: Utc::now(),
            stale_after: Some(Utc::now() + chrono::Duration::hours(24)),
            joint_offsets: BTreeMap::from([("shoulder_pitch".into(), 0.02), ("elbow".into(), -0.01)]),
            frame_corrections: BTreeMap::from([(
                "camera_link".into(),
                Transform3D {
                    translation: [0.001, -0.002, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
            )]),
            sensor_calibrations: BTreeMap::from([(
                "wrist_ft".into(),
                SensorCalibration {
                    sensor_id: "wrist_ft".into(),
                    offset: vec![0.1, -0.05, 0.0, 0.0, 0.0, 0.0],
                    scale: Some(vec![1.01, 0.99, 1.0, 1.0, 1.0, 1.0]),
                    calibrated_at: Utc::now(),
                },
            )]),
            temperature_range: Some((15.0, 35.0)),
            valid_for_model_digest: "model_sha_xyz".into(),
        }
    }

    #[test]
    fn calibration_serde_roundtrip() {
        let overlay = sample_overlay();
        let json = serde_json::to_string(&overlay).unwrap();
        let back: CalibrationOverlay = serde_json::from_str(&json).unwrap();
        assert_eq!(overlay.calibration_id, back.calibration_id);
        assert_eq!(overlay.joint_offsets.len(), back.joint_offsets.len());
        assert_eq!(overlay.frame_corrections.len(), back.frame_corrections.len());
        assert_eq!(overlay.sensor_calibrations.len(), back.sensor_calibrations.len());
    }

    #[test]
    fn is_stale_before_deadline() {
        let overlay = sample_overlay();
        assert!(!overlay.is_stale(Utc::now()));
    }

    #[test]
    fn is_stale_after_deadline() {
        let mut overlay = sample_overlay();
        overlay.stale_after = Some(Utc::now() - chrono::Duration::hours(1));
        assert!(overlay.is_stale(Utc::now()));
    }

    #[test]
    fn is_stale_no_deadline() {
        let mut overlay = sample_overlay();
        overlay.stale_after = None;
        assert!(!overlay.is_stale(Utc::now()));
    }

    #[test]
    fn valid_for_correct_model_after_stamp() {
        let mut overlay = sample_overlay();
        overlay.stamp_digest();
        assert!(overlay.is_valid_for_model("model_sha_xyz"));
    }

    #[test]
    fn invalid_for_wrong_model() {
        let mut overlay = sample_overlay();
        overlay.stamp_digest();
        assert!(!overlay.is_valid_for_model("different_model"));
    }

    #[test]
    fn invalid_without_stamp() {
        let overlay = sample_overlay();
        // calibration_digest is "sha256_abc" — doesn't match compute_digest()
        assert!(!overlay.is_valid_for_model("model_sha_xyz"));
    }

    #[test]
    fn digest_is_deterministic() {
        let overlay = sample_overlay();
        let d1 = overlay.compute_digest();
        let d2 = overlay.compute_digest();
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
    }

    #[test]
    fn digest_changes_on_modification() {
        let o1 = sample_overlay();
        let mut o2 = sample_overlay();
        o2.joint_offsets.insert("wrist".into(), 0.05);
        assert_ne!(o1.compute_digest(), o2.compute_digest());
    }

    #[test]
    fn digest_stable_regardless_of_insertion_order() {
        // BTreeMap guarantees sorted keys, so insertion order doesn't matter.
        // This test proves it by building two overlays with different insertion orders.
        // Use a fixed timestamp so both overlays are byte-for-byte identical except
        // for the insertion order of joint_offsets.
        let fixed_ts = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let make_overlay = |offsets: BTreeMap<String, f64>| CalibrationOverlay {
            calibration_id: "cal-order-test".into(),
            calibration_digest: String::new(),
            calibrated_at: fixed_ts,
            stale_after: None,
            joint_offsets: offsets,
            frame_corrections: BTreeMap::new(),
            sensor_calibrations: BTreeMap::new(),
            temperature_range: None,
            valid_for_model_digest: "model_xyz".into(),
        };

        let mut offsets1 = BTreeMap::new();
        offsets1.insert("aaa".into(), 1.0);
        offsets1.insert("zzz".into(), 2.0);
        offsets1.insert("mmm".into(), 3.0);

        let mut offsets2 = BTreeMap::new();
        offsets2.insert("zzz".into(), 2.0);
        offsets2.insert("mmm".into(), 3.0);
        offsets2.insert("aaa".into(), 1.0);

        let o1 = make_overlay(offsets1);
        let o2 = make_overlay(offsets2);

        assert_eq!(o1.compute_digest(), o2.compute_digest());
    }
}
