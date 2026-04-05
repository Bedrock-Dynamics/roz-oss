//! Integration of predictive world models with the verifier.
//! World model predictions are advisory evidence, never blocking.

use chrono::Utc;
use roz_core::embodiment::prediction::PredictionEvidence;
use roz_core::interfaces::WorldModelPredictor;
use roz_core::spatial::WorldState;

/// Configuration for world model verification.
pub struct WorldModelVerifierConfig {
    pub collision_risk_threshold: f64,
    pub contact_risk_threshold: f64,
    pub horizon_ticks: u32,
}

impl Default for WorldModelVerifierConfig {
    fn default() -> Self {
        Self {
            collision_risk_threshold: 0.5,
            contact_risk_threshold: 0.8,
            horizon_ticks: 50,
        }
    }
}

/// Result of world model verification (always advisory).
#[derive(Debug)]
pub struct WorldModelResult {
    pub evidence: PredictionEvidence,
    pub high_collision_risk: bool,
    pub high_contact_risk: bool,
    pub advisory_message: Option<String>,
}

/// Run world model predictions for a controller evaluation.
pub fn evaluate_with_world_model(
    predictor: &dyn WorldModelPredictor,
    history: &[WorldState],
    proposed_actions: &[Vec<f64>],
    controller_id: &str,
    config: &WorldModelVerifierConfig,
) -> Result<WorldModelResult, String> {
    let predictions = predictor
        .predict(history, proposed_actions, config.horizon_ticks)
        .map_err(|e| format!("world model prediction failed: {e}"))?;

    let high_collision = predictions
        .iter()
        .any(|p| p.collision_risk > config.collision_risk_threshold);
    let high_contact = predictions
        .iter()
        .any(|p| p.contact_risk > config.contact_risk_threshold);

    let advisory = if high_collision {
        Some(format!(
            "High collision risk detected in {}-tick horizon",
            config.horizon_ticks
        ))
    } else if high_contact {
        Some(format!(
            "High contact risk detected in {}-tick horizon",
            config.horizon_ticks
        ))
    } else {
        None
    };

    let evidence = PredictionEvidence {
        prediction_id: uuid::Uuid::new_v4().to_string(),
        controller_id: controller_id.into(),
        horizon_ticks: config.horizon_ticks,
        predictions,
        model_id: "world_model".into(),
        model_version: "0.1.0".into(),
        created_at: Utc::now(),
    };

    Ok(WorldModelResult {
        evidence,
        high_collision_risk: high_collision,
        high_contact_risk: high_contact,
        advisory_message: advisory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::embodiment::prediction::PredictedState;

    struct MockPredictor {
        predictions: Vec<PredictedState>,
    }

    impl WorldModelPredictor for MockPredictor {
        fn predict(
            &self,
            _history: &[WorldState],
            _actions: &[Vec<f64>],
            _horizon_ticks: u32,
        ) -> Result<Vec<PredictedState>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.predictions.clone())
        }
    }

    struct ErrorPredictor;

    impl WorldModelPredictor for ErrorPredictor {
        fn predict(
            &self,
            _history: &[WorldState],
            _actions: &[Vec<f64>],
            _horizon_ticks: u32,
        ) -> Result<Vec<PredictedState>, Box<dyn std::error::Error + Send + Sync>> {
            Err("simulated world model failure".into())
        }
    }

    fn low_risk_state() -> PredictedState {
        PredictedState {
            tick_offset: 10,
            predicted_joints: vec![0.1, 0.2],
            predicted_tcp_poses: vec![],
            collision_risk: 0.05,
            contact_risk: 0.1,
            occlusion_risk: 0.0,
            confidence: 0.9,
        }
    }

    fn high_collision_state() -> PredictedState {
        PredictedState {
            tick_offset: 20,
            predicted_joints: vec![0.3, 0.4],
            predicted_tcp_poses: vec![],
            collision_risk: 0.75,
            contact_risk: 0.1,
            occlusion_risk: 0.0,
            confidence: 0.8,
        }
    }

    fn high_contact_state() -> PredictedState {
        PredictedState {
            tick_offset: 15,
            predicted_joints: vec![0.5, 0.6],
            predicted_tcp_poses: vec![],
            collision_risk: 0.1,
            contact_risk: 0.9,
            occlusion_risk: 0.0,
            confidence: 0.85,
        }
    }

    #[test]
    fn evaluate_clean_predictions_no_advisory() {
        let predictor = MockPredictor {
            predictions: vec![low_risk_state()],
        };
        let config = WorldModelVerifierConfig::default();
        let result = evaluate_with_world_model(&predictor, &[], &[vec![0.1, 0.2]], "ctrl-001", &config).unwrap();

        assert!(!result.high_collision_risk);
        assert!(!result.high_contact_risk);
        assert!(result.advisory_message.is_none());
        assert_eq!(result.evidence.controller_id, "ctrl-001");
        assert_eq!(result.evidence.model_id, "world_model");
        assert_eq!(result.evidence.horizon_ticks, 50);
        assert_eq!(result.evidence.predictions.len(), 1);
    }

    #[test]
    fn evaluate_high_collision_risk_produces_advisory() {
        let predictor = MockPredictor {
            predictions: vec![low_risk_state(), high_collision_state()],
        };
        let config = WorldModelVerifierConfig::default();
        let result = evaluate_with_world_model(&predictor, &[], &[vec![0.5, 0.5]], "ctrl-002", &config).unwrap();

        assert!(result.high_collision_risk);
        assert!(!result.high_contact_risk);
        let msg = result.advisory_message.unwrap();
        assert!(msg.contains("collision"), "advisory should mention collision: {msg}");
        assert!(msg.contains("50"), "advisory should mention horizon ticks: {msg}");
    }

    #[test]
    fn evaluate_high_contact_risk_produces_advisory() {
        let predictor = MockPredictor {
            predictions: vec![low_risk_state(), high_contact_state()],
        };
        let config = WorldModelVerifierConfig::default();
        let result = evaluate_with_world_model(&predictor, &[], &[vec![0.3, 0.3]], "ctrl-003", &config).unwrap();

        assert!(!result.high_collision_risk);
        assert!(result.high_contact_risk);
        let msg = result.advisory_message.unwrap();
        assert!(msg.contains("contact"), "advisory should mention contact: {msg}");
    }

    #[test]
    fn evaluate_predictor_error_is_propagated() {
        let predictor = ErrorPredictor;
        let config = WorldModelVerifierConfig::default();
        let err = evaluate_with_world_model(&predictor, &[], &[], "ctrl-error", &config).unwrap_err();
        assert!(err.contains("world model prediction failed"), "got: {err}");
        assert!(err.contains("simulated world model failure"), "got: {err}");
    }

    #[test]
    fn evidence_fields_are_populated() {
        let predictor = MockPredictor {
            predictions: vec![low_risk_state()],
        };
        let config = WorldModelVerifierConfig {
            collision_risk_threshold: 0.5,
            contact_risk_threshold: 0.8,
            horizon_ticks: 100,
        };
        let result = evaluate_with_world_model(&predictor, &[], &[], "ctrl-fields", &config).unwrap();

        assert!(!result.evidence.prediction_id.is_empty());
        assert_eq!(result.evidence.horizon_ticks, 100);
        assert_eq!(result.evidence.model_version, "0.1.0");
    }

    #[test]
    fn collision_advisory_takes_precedence_over_contact() {
        // When both collision and contact thresholds are exceeded, collision advisory wins
        let predictor = MockPredictor {
            predictions: vec![high_collision_state(), high_contact_state()],
        };
        let config = WorldModelVerifierConfig::default();
        let result = evaluate_with_world_model(&predictor, &[], &[], "ctrl-both", &config).unwrap();

        assert!(result.high_collision_risk);
        assert!(result.high_contact_risk);
        // Advisory message is the collision one since it's checked first
        let msg = result.advisory_message.unwrap();
        assert!(
            msg.contains("collision"),
            "collision advisory should take precedence: {msg}"
        );
    }
}
