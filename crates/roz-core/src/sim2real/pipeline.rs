use std::collections::HashMap;
use std::hash::BuildHasher;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::sim2real::metrics;
use crate::sim2real::report::{ChannelDivergence, DiagnosisAction, DivergenceReport, MetricKind, PhaseScore};
use crate::sim2real::spectral;

/// Configuration for a single channel comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub name: String,
    pub metric: MetricKind,
    pub threshold: f64,
}

/// Top-level configuration driving a sim-vs-real comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonConfig {
    pub channels: Vec<ChannelConfig>,
    pub pass_score: f64,
}

/// Compute a single metric for two equal-length 1-D series.
#[expect(clippy::cast_precision_loss, reason = "index-to-f64 for Frechet embedding is safe")]
fn compute_metric(metric: MetricKind, sim: &[f64], real: &[f64]) -> Option<f64> {
    match metric {
        MetricKind::Rmse => metrics::rmse(sim, real),
        MetricKind::Mae => metrics::mae(sim, real),
        MetricKind::MaxDeviation => metrics::max_deviation(sim, real),
        MetricKind::SpectralCoherence => spectral::spectral_coherence(sim, real),
        MetricKind::Dtw => {
            let result = crate::sim2real::dtw::dtw_align(sim, real, None);
            Some(result.distance)
        }
        MetricKind::Frechet => {
            // Frechet is defined on 3-D trajectories; for 1-D channels
            // we treat the index as X, the value as Y, and Z = 0.
            let to_pts = |data: &[f64]| -> Vec<nalgebra::Point3<f64>> {
                data.iter()
                    .enumerate()
                    .map(|(i, &v)| nalgebra::Point3::new(i as f64, v, 0.0))
                    .collect()
            };
            crate::sim2real::frechet::discrete_frechet(&to_pts(sim), &to_pts(real))
        }
    }
}

/// Determine the recommended action from the overall pass fraction.
fn action_for_score(score: f64, pass_score: f64) -> DiagnosisAction {
    if score >= pass_score {
        DiagnosisAction::Pass
    } else if score >= 0.7 {
        DiagnosisAction::Investigate
    } else if score >= 0.5 {
        DiagnosisAction::Retune
    } else {
        DiagnosisAction::Escalate
    }
}

/// Returns true for metrics where a higher value indicates better similarity.
const fn is_higher_better(metric: MetricKind) -> bool {
    matches!(metric, MetricKind::SpectralCoherence)
}

/// Run the comparison pipeline across all configured channels.
///
/// For each channel in `config.channels`, the function looks up the
/// corresponding data in `sim_data` and `real_data`, computes the
/// specified metric, and compares the result against the threshold.
///
/// Missing or incomputable channels are treated as failures.
#[expect(
    clippy::cast_precision_loss,
    reason = "channel counts will never exceed f64 mantissa range"
)]
pub fn compare<S: BuildHasher>(
    sim_data: &HashMap<String, Vec<f64>, S>,
    real_data: &HashMap<String, Vec<f64>, S>,
    config: &ComparisonConfig,
) -> DivergenceReport {
    let mut divergences = Vec::new();

    for ch in &config.channels {
        let (value, passed) = match (sim_data.get(&ch.name), real_data.get(&ch.name)) {
            (Some(sim), Some(real)) => compute_metric(ch.metric, sim, real).map_or((f64::NAN, false), |v| {
                let passed = if is_higher_better(ch.metric) {
                    v >= ch.threshold
                } else {
                    v <= ch.threshold
                };
                (v, passed)
            }),
            _ => (f64::NAN, false),
        };

        divergences.push(ChannelDivergence {
            channel_name: ch.name.clone(),
            metric: ch.metric,
            value,
            threshold: Some(ch.threshold),
            passed,
        });
    }

    let passed_count = divergences.iter().filter(|d| d.passed).count();
    let total = divergences.len();
    let overall_score = if total > 0 {
        passed_count as f64 / total as f64
    } else {
        1.0
    };

    let action = action_for_score(overall_score, config.pass_score);

    DivergenceReport {
        id: Uuid::new_v4(),
        sim_recording_id: Uuid::nil(),
        real_recording_id: Uuid::nil(),
        overall_score,
        phases: vec![PhaseScore {
            phase_name: "default".into(),
            score: overall_score,
            channels: divergences,
        }],
        action,
        signatures_matched: Vec::new(),
        created_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_data(channels: &[(&str, Vec<f64>)]) -> HashMap<String, Vec<f64>> {
        channels.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
    }

    #[test]
    fn all_channels_pass() {
        let sim = make_data(&[("vel", vec![1.0, 2.0, 3.0])]);
        let real = make_data(&[("vel", vec![1.0, 2.0, 3.0])]);
        let config = ComparisonConfig {
            channels: vec![ChannelConfig {
                name: "vel".into(),
                metric: MetricKind::Rmse,
                threshold: 0.1,
            }],
            pass_score: 0.8,
        };
        let report = compare(&sim, &real, &config);
        assert_eq!(report.action, DiagnosisAction::Pass);
        assert!((report.overall_score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn some_channels_fail() {
        let sim = make_data(&[("a", vec![1.0, 2.0]), ("b", vec![1.0, 2.0])]);
        let real = make_data(&[("a", vec![1.0, 2.0]), ("b", vec![10.0, 20.0])]);
        let config = ComparisonConfig {
            channels: vec![
                ChannelConfig {
                    name: "a".into(),
                    metric: MetricKind::Rmse,
                    threshold: 0.1,
                },
                ChannelConfig {
                    name: "b".into(),
                    metric: MetricKind::Rmse,
                    threshold: 0.1,
                },
            ],
            pass_score: 0.8,
        };
        let report = compare(&sim, &real, &config);
        assert!((report.overall_score - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_channel_data_fails() {
        let sim = make_data(&[("vel", vec![1.0, 2.0])]);
        let real: HashMap<String, Vec<f64>> = HashMap::new();
        let config = ComparisonConfig {
            channels: vec![ChannelConfig {
                name: "vel".into(),
                metric: MetricKind::Rmse,
                threshold: 1.0,
            }],
            pass_score: 0.8,
        };
        let report = compare(&sim, &real, &config);
        assert!((report.overall_score - 0.0).abs() < f64::EPSILON);
        assert_eq!(report.action, DiagnosisAction::Escalate);
    }

    #[test]
    fn empty_config_produces_pass() {
        let sim: HashMap<String, Vec<f64>> = HashMap::new();
        let real: HashMap<String, Vec<f64>> = HashMap::new();
        let config = ComparisonConfig {
            channels: Vec::new(),
            pass_score: 0.8,
        };
        let report = compare(&sim, &real, &config);
        assert!((report.overall_score - 1.0).abs() < f64::EPSILON);
        assert_eq!(report.action, DiagnosisAction::Pass);
    }

    #[test]
    fn score_threshold_investigate() {
        // 3 out of 4 pass => 0.75 => Investigate (>= 0.7, < pass_score)
        let sim = make_data(&[("a", vec![1.0]), ("b", vec![1.0]), ("c", vec![1.0]), ("d", vec![1.0])]);
        let real = make_data(&[("a", vec![1.0]), ("b", vec![1.0]), ("c", vec![1.0]), ("d", vec![100.0])]);
        let config = ComparisonConfig {
            channels: vec![
                ChannelConfig {
                    name: "a".into(),
                    metric: MetricKind::Mae,
                    threshold: 0.5,
                },
                ChannelConfig {
                    name: "b".into(),
                    metric: MetricKind::Mae,
                    threshold: 0.5,
                },
                ChannelConfig {
                    name: "c".into(),
                    metric: MetricKind::Mae,
                    threshold: 0.5,
                },
                ChannelConfig {
                    name: "d".into(),
                    metric: MetricKind::Mae,
                    threshold: 0.5,
                },
            ],
            pass_score: 0.8,
        };
        let report = compare(&sim, &real, &config);
        assert_eq!(report.action, DiagnosisAction::Investigate);
    }

    #[test]
    fn score_threshold_retune() {
        // 1 out of 2 pass => 0.5 => Retune (>= 0.5, < 0.7)
        let sim = make_data(&[("a", vec![1.0]), ("b", vec![1.0])]);
        let real = make_data(&[("a", vec![1.0]), ("b", vec![100.0])]);
        let config = ComparisonConfig {
            channels: vec![
                ChannelConfig {
                    name: "a".into(),
                    metric: MetricKind::Mae,
                    threshold: 0.5,
                },
                ChannelConfig {
                    name: "b".into(),
                    metric: MetricKind::Mae,
                    threshold: 0.5,
                },
            ],
            pass_score: 0.8,
        };
        let report = compare(&sim, &real, &config);
        assert_eq!(report.action, DiagnosisAction::Retune);
    }

    #[test]
    fn score_threshold_escalate() {
        // 0 out of 2 pass => 0.0 => Escalate (< 0.5)
        let sim = make_data(&[("a", vec![1.0]), ("b", vec![1.0])]);
        let real = make_data(&[("a", vec![100.0]), ("b", vec![100.0])]);
        let config = ComparisonConfig {
            channels: vec![
                ChannelConfig {
                    name: "a".into(),
                    metric: MetricKind::Mae,
                    threshold: 0.5,
                },
                ChannelConfig {
                    name: "b".into(),
                    metric: MetricKind::Mae,
                    threshold: 0.5,
                },
            ],
            pass_score: 0.8,
        };
        let report = compare(&sim, &real, &config);
        assert_eq!(report.action, DiagnosisAction::Escalate);
    }

    #[test]
    fn spectral_coherence_higher_is_better() {
        // Identical signals should pass with a high threshold
        let sim = make_data(&[("vel", vec![1.0, 2.0, 3.0, 4.0])]);
        let real = make_data(&[("vel", vec![1.0, 2.0, 3.0, 4.0])]);
        let config = ComparisonConfig {
            channels: vec![ChannelConfig {
                name: "vel".into(),
                metric: MetricKind::SpectralCoherence,
                threshold: 0.9,
            }],
            pass_score: 0.8,
        };
        let report = compare(&sim, &real, &config);
        assert_eq!(report.action, DiagnosisAction::Pass);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = ComparisonConfig {
            channels: vec![ChannelConfig {
                name: "vel".into(),
                metric: MetricKind::Dtw,
                threshold: 0.5,
            }],
            pass_score: 0.9,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deser: ComparisonConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.channels.len(), 1);
        assert_eq!(deser.channels[0].metric, MetricKind::Dtw);
    }
}
