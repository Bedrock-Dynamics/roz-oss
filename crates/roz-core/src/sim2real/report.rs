use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Which metric was used for a channel comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Rmse,
    Mae,
    MaxDeviation,
    Frechet,
    Dtw,
    SpectralCoherence,
}

/// Divergence measurement for a single channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDivergence {
    pub channel_name: String,
    pub metric: MetricKind,
    pub value: f64,
    pub threshold: Option<f64>,
    pub passed: bool,
}

/// Aggregated score for a named phase of a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseScore {
    pub phase_name: String,
    pub score: f64,
    pub channels: Vec<ChannelDivergence>,
}

/// Recommended action after analysing divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosisAction {
    Pass,
    Investigate,
    Retune,
    Escalate,
}

/// Top-level report summarising the divergence between a sim and real recording.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DivergenceReport {
    pub id: Uuid,
    pub sim_recording_id: Uuid,
    pub real_recording_id: Uuid,
    pub overall_score: f64,
    pub phases: Vec<PhaseScore>,
    pub action: DiagnosisAction,
    pub signatures_matched: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> DivergenceReport {
        DivergenceReport {
            id: Uuid::new_v4(),
            sim_recording_id: Uuid::new_v4(),
            real_recording_id: Uuid::new_v4(),
            overall_score: 0.85,
            phases: vec![PhaseScore {
                phase_name: "approach".into(),
                score: 0.9,
                channels: vec![ChannelDivergence {
                    channel_name: "velocity".into(),
                    metric: MetricKind::Rmse,
                    value: 0.05,
                    threshold: Some(0.1),
                    passed: true,
                }],
            }],
            action: DiagnosisAction::Pass,
            signatures_matched: vec!["motor_stall".into()],
            created_at: Utc::now(),
        }
    }

    #[test]
    fn serde_roundtrip_report() {
        let report = sample_report();
        let json = serde_json::to_string(&report).unwrap();
        let deser: DivergenceReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.overall_score, report.overall_score);
        assert_eq!(deser.action, report.action);
    }

    #[test]
    fn serde_roundtrip_metric_kind() {
        let kinds = vec![
            MetricKind::Rmse,
            MetricKind::Mae,
            MetricKind::MaxDeviation,
            MetricKind::Frechet,
            MetricKind::Dtw,
            MetricKind::SpectralCoherence,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let deser: MetricKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, deser);
        }
    }

    #[test]
    fn metric_kind_snake_case_serialization() {
        assert_eq!(serde_json::to_string(&MetricKind::Rmse).unwrap(), "\"rmse\"");
        assert_eq!(
            serde_json::to_string(&MetricKind::MaxDeviation).unwrap(),
            "\"max_deviation\""
        );
        assert_eq!(
            serde_json::to_string(&MetricKind::SpectralCoherence).unwrap(),
            "\"spectral_coherence\""
        );
    }

    #[test]
    fn diagnosis_action_variants() {
        let actions = [
            (DiagnosisAction::Pass, "\"pass\""),
            (DiagnosisAction::Investigate, "\"investigate\""),
            (DiagnosisAction::Retune, "\"retune\""),
            (DiagnosisAction::Escalate, "\"escalate\""),
        ];
        for (action, expected) in actions {
            assert_eq!(serde_json::to_string(&action).unwrap(), expected);
        }
    }

    #[test]
    fn channel_divergence_roundtrip() {
        let cd = ChannelDivergence {
            channel_name: "torque".into(),
            metric: MetricKind::Mae,
            value: 1.5,
            threshold: Some(2.0),
            passed: true,
        };
        let json = serde_json::to_string(&cd).unwrap();
        let deser: ChannelDivergence = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.channel_name, "torque");
        assert!(deser.passed);
    }

    #[test]
    fn phase_score_roundtrip() {
        let ps = PhaseScore {
            phase_name: "grasp".into(),
            score: 0.75,
            channels: Vec::new(),
        };
        let json = serde_json::to_string(&ps).unwrap();
        let deser: PhaseScore = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.phase_name, "grasp");
    }

    #[test]
    fn report_with_no_phases() {
        let report = DivergenceReport {
            id: Uuid::new_v4(),
            sim_recording_id: Uuid::new_v4(),
            real_recording_id: Uuid::new_v4(),
            overall_score: 0.0,
            phases: Vec::new(),
            action: DiagnosisAction::Escalate,
            signatures_matched: Vec::new(),
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let deser: DivergenceReport = serde_json::from_str(&json).unwrap();
        assert!(deser.phases.is_empty());
        assert_eq!(deser.action, DiagnosisAction::Escalate);
    }

    #[test]
    fn report_construction_preserves_ids() {
        let id = Uuid::new_v4();
        let sim_id = Uuid::new_v4();
        let real_id = Uuid::new_v4();
        let report = DivergenceReport {
            id,
            sim_recording_id: sim_id,
            real_recording_id: real_id,
            overall_score: 1.0,
            phases: Vec::new(),
            action: DiagnosisAction::Pass,
            signatures_matched: Vec::new(),
            created_at: Utc::now(),
        };
        assert_eq!(report.id, id);
        assert_eq!(report.sim_recording_id, sim_id);
        assert_eq!(report.real_recording_id, real_id);
    }
}
