use serde::{Deserialize, Serialize};

use super::frame_tree::Transform3D;

/// A predicted future state from a world model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictedState {
    pub tick_offset: u32,
    pub predicted_joints: Vec<f64>,
    pub predicted_tcp_poses: Vec<Transform3D>,
    pub collision_risk: f64,
    pub contact_risk: f64,
    pub occlusion_risk: f64,
    pub confidence: f64,
}

/// Evidence artifact from a prediction run, stored alongside controller evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictionEvidence {
    pub prediction_id: String,
    pub controller_id: String,
    pub horizon_ticks: u32,
    pub predictions: Vec<PredictedState>,
    pub model_id: String,
    pub model_version: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicted_state_serde() {
        let state = PredictedState {
            tick_offset: 50,
            predicted_joints: vec![0.1, 0.2, 0.3],
            predicted_tcp_poses: vec![Transform3D {
                translation: [0.5, 0.0, 0.3],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            }],
            collision_risk: 0.05,
            contact_risk: 0.3,
            occlusion_risk: 0.1,
            confidence: 0.85,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: PredictedState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
        assert!(back.confidence > 0.0);
    }

    #[test]
    fn prediction_evidence_serde() {
        let evidence = PredictionEvidence {
            prediction_id: "pred-001".into(),
            controller_id: "ctrl-001".into(),
            horizon_ticks: 100,
            predictions: vec![PredictedState {
                tick_offset: 10,
                predicted_joints: vec![0.5],
                predicted_tcp_poses: vec![],
                collision_risk: 0.0,
                contact_risk: 0.0,
                occlusion_risk: 0.0,
                confidence: 0.9,
            }],
            model_id: "pointworld-v1".into(),
            model_version: "0.2.0".into(),
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&evidence).unwrap();
        let back: PredictionEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(evidence.prediction_id, back.prediction_id);
        assert_eq!(evidence.predictions.len(), back.predictions.len());
    }

    #[test]
    fn risk_values_bounded() {
        let state = PredictedState {
            tick_offset: 1,
            predicted_joints: vec![],
            predicted_tcp_poses: vec![],
            collision_risk: 0.0,
            contact_risk: 1.0,
            occlusion_risk: 0.5,
            confidence: 0.0,
        };
        // Verify edge values serialize correctly
        let json = serde_json::to_string(&state).unwrap();
        let back: PredictedState = serde_json::from_str(&json).unwrap();
        assert!((back.contact_risk - 1.0).abs() < f64::EPSILON);
        assert!((back.confidence - 0.0).abs() < f64::EPSILON);
    }
}
