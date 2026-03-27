use serde::{Deserialize, Serialize};

use crate::sim2real::report::{DivergenceReport, MetricKind};

/// A single condition that a divergent channel must satisfy for a
/// failure signature to match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureCondition {
    pub metric: MetricKind,
    pub channel_pattern: String,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
}

/// A named failure signature composed of one or more conditions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureSignature {
    pub name: String,
    pub description: String,
    pub conditions: Vec<SignatureCondition>,
}

/// Return the seven built-in failure signatures.
pub fn builtin_signatures() -> Vec<FailureSignature> {
    vec![
        FailureSignature {
            name: "motor_stall".into(),
            description: "High RMSE on motor velocity channels indicates a stalled motor.".into(),
            conditions: vec![SignatureCondition {
                metric: MetricKind::Rmse,
                channel_pattern: "motor".into(),
                min_value: Some(0.5),
                max_value: None,
            }],
        },
        FailureSignature {
            name: "pid_oscillation".into(),
            description: "High spectral coherence divergence on control channels suggests PID oscillation.".into(),
            conditions: vec![SignatureCondition {
                metric: MetricKind::SpectralCoherence,
                channel_pattern: "control".into(),
                min_value: Some(0.3),
                max_value: None,
            }],
        },
        FailureSignature {
            name: "contact_timing_mismatch".into(),
            description: "High DTW on contact event timing indicates misaligned contact events.".into(),
            conditions: vec![SignatureCondition {
                metric: MetricKind::Dtw,
                channel_pattern: "contact".into(),
                min_value: Some(0.2),
                max_value: None,
            }],
        },
        FailureSignature {
            name: "sensor_saturation".into(),
            description: "High max deviation on sensor channels indicates sensor saturation.".into(),
            conditions: vec![SignatureCondition {
                metric: MetricKind::MaxDeviation,
                channel_pattern: "sensor".into(),
                min_value: Some(1.0),
                max_value: None,
            }],
        },
        FailureSignature {
            name: "integral_windup".into(),
            description: "High MAE on integral error channels indicates integral windup.".into(),
            conditions: vec![SignatureCondition {
                metric: MetricKind::Mae,
                channel_pattern: "integral".into(),
                min_value: Some(0.5),
                max_value: None,
            }],
        },
        FailureSignature {
            name: "grasp_slip".into(),
            description: "High Frechet distance on gripper trajectory indicates grasp slippage.".into(),
            conditions: vec![SignatureCondition {
                metric: MetricKind::Frechet,
                channel_pattern: "gripper".into(),
                min_value: Some(0.3),
                max_value: None,
            }],
        },
        FailureSignature {
            name: "communication_latency".into(),
            description: "High DTW on command-response timing indicates communication latency.".into(),
            conditions: vec![SignatureCondition {
                metric: MetricKind::Dtw,
                channel_pattern: "command".into(),
                min_value: Some(0.1),
                max_value: None,
            }],
        },
    ]
}

/// Check whether a single condition matches any divergent channel in
/// the report.
fn condition_matches(condition: &SignatureCondition, report: &DivergenceReport) -> bool {
    for phase in &report.phases {
        for ch in &phase.channels {
            if ch.passed {
                continue;
            }
            if ch.metric != condition.metric {
                continue;
            }
            if !ch.channel_name.contains(&condition.channel_pattern) {
                continue;
            }
            let above_min = condition.min_value.is_none_or(|min| ch.value >= min);
            let below_max = condition.max_value.is_none_or(|max| ch.value <= max);
            if above_min && below_max {
                return true;
            }
        }
    }
    false
}

/// Return the names of every signature whose conditions **all** match
/// divergent channels in the report.
pub fn match_signatures(report: &DivergenceReport, signatures: &[FailureSignature]) -> Vec<String> {
    signatures
        .iter()
        .filter(|sig| sig.conditions.iter().all(|c| condition_matches(c, report)))
        .map(|sig| sig.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::sim2real::report::{ChannelDivergence, DiagnosisAction, DivergenceReport, PhaseScore};

    fn make_report(channels: Vec<ChannelDivergence>) -> DivergenceReport {
        DivergenceReport {
            id: Uuid::new_v4(),
            sim_recording_id: Uuid::new_v4(),
            real_recording_id: Uuid::new_v4(),
            overall_score: 0.0,
            phases: vec![PhaseScore {
                phase_name: "test".into(),
                score: 0.0,
                channels,
            }],
            action: DiagnosisAction::Escalate,
            signatures_matched: Vec::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn builtin_returns_seven() {
        assert_eq!(builtin_signatures().len(), 7);
    }

    #[test]
    fn match_motor_stall_signature() {
        let report = make_report(vec![ChannelDivergence {
            channel_name: "left_motor_velocity".into(),
            metric: MetricKind::Rmse,
            value: 0.8,
            threshold: Some(0.3),
            passed: false,
        }]);
        let sigs = builtin_signatures();
        let matched = match_signatures(&report, &sigs);
        assert!(matched.contains(&"motor_stall".to_string()));
    }

    #[test]
    fn no_match_when_channels_pass() {
        let report = make_report(vec![ChannelDivergence {
            channel_name: "left_motor_velocity".into(),
            metric: MetricKind::Rmse,
            value: 0.1,
            threshold: Some(0.3),
            passed: true,
        }]);
        let sigs = builtin_signatures();
        let matched = match_signatures(&report, &sigs);
        assert!(matched.is_empty());
    }

    #[test]
    fn no_match_when_value_below_min() {
        let report = make_report(vec![ChannelDivergence {
            channel_name: "left_motor_velocity".into(),
            metric: MetricKind::Rmse,
            value: 0.1, // below min_value 0.5
            threshold: Some(0.05),
            passed: false,
        }]);
        let sigs = builtin_signatures();
        let matched = match_signatures(&report, &sigs);
        assert!(!matched.contains(&"motor_stall".to_string()));
    }

    #[test]
    fn partial_condition_does_not_match() {
        // Signature with two conditions — only one is satisfied
        let sig = FailureSignature {
            name: "multi_cond".into(),
            description: "test".into(),
            conditions: vec![
                SignatureCondition {
                    metric: MetricKind::Rmse,
                    channel_pattern: "motor".into(),
                    min_value: Some(0.5),
                    max_value: None,
                },
                SignatureCondition {
                    metric: MetricKind::Mae,
                    channel_pattern: "sensor".into(),
                    min_value: Some(1.0),
                    max_value: None,
                },
            ],
        };
        let report = make_report(vec![ChannelDivergence {
            channel_name: "left_motor".into(),
            metric: MetricKind::Rmse,
            value: 0.8,
            threshold: Some(0.3),
            passed: false,
        }]);
        let matched = match_signatures(&report, &[sig]);
        assert!(matched.is_empty());
    }

    #[test]
    fn multiple_matches() {
        let report = make_report(vec![
            ChannelDivergence {
                channel_name: "left_motor_velocity".into(),
                metric: MetricKind::Rmse,
                value: 0.8,
                threshold: Some(0.3),
                passed: false,
            },
            ChannelDivergence {
                channel_name: "force_sensor_1".into(),
                metric: MetricKind::MaxDeviation,
                value: 2.0,
                threshold: Some(0.5),
                passed: false,
            },
        ]);
        let sigs = builtin_signatures();
        let matched = match_signatures(&report, &sigs);
        assert!(matched.contains(&"motor_stall".to_string()));
        assert!(matched.contains(&"sensor_saturation".to_string()));
    }

    #[test]
    fn max_value_bound_respected() {
        let sig = FailureSignature {
            name: "bounded".into(),
            description: "test".into(),
            conditions: vec![SignatureCondition {
                metric: MetricKind::Rmse,
                channel_pattern: "motor".into(),
                min_value: Some(0.5),
                max_value: Some(1.0),
            }],
        };
        let report_in = make_report(vec![ChannelDivergence {
            channel_name: "motor_left".into(),
            metric: MetricKind::Rmse,
            value: 0.7,
            threshold: Some(0.3),
            passed: false,
        }]);
        assert_eq!(match_signatures(&report_in, &[sig.clone()]).len(), 1);

        let report_out = make_report(vec![ChannelDivergence {
            channel_name: "motor_left".into(),
            metric: MetricKind::Rmse,
            value: 1.5, // above max
            threshold: Some(0.3),
            passed: false,
        }]);
        assert!(match_signatures(&report_out, &[sig]).is_empty());
    }

    #[test]
    fn serde_roundtrip_signature() {
        let sigs = builtin_signatures();
        let json = serde_json::to_string(&sigs).unwrap();
        let deser: Vec<FailureSignature> = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.len(), 7);
        assert_eq!(deser[0].name, "motor_stall");
    }
}
