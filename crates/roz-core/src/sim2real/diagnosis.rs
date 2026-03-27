use serde::{Deserialize, Serialize};

use crate::sim2real::report::DiagnosisAction;

/// Pre-computed statistics for a single data channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelStats {
    pub channel_name: String,
    pub mean: f64,
    pub std_dev: f64,
    pub min: f64,
    pub max: f64,
    pub sample_count: usize,
}

/// Context provided to an LLM for divergence diagnosis.
///
/// Contains only pre-computed statistics — never raw time series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosisContext {
    pub sim_stats: Vec<ChannelStats>,
    pub real_stats: Vec<ChannelStats>,
    pub divergence_summary: String,
    pub action: DiagnosisAction,
}

/// Compute summary statistics for a named channel from raw data.
///
/// If `data` is empty, all numeric fields are set to 0.0 and
/// `sample_count` is 0.
#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts will never exceed f64 mantissa range"
)]
pub fn compute_channel_stats(name: &str, data: &[f64]) -> ChannelStats {
    if data.is_empty() {
        return ChannelStats {
            channel_name: name.to_string(),
            mean: 0.0,
            std_dev: 0.0,
            min: 0.0,
            max: 0.0,
            sample_count: 0,
        };
    }

    let n = data.len() as f64;
    let mean = data.iter().sum::<f64>() / n;
    let variance = data.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let std_dev = variance.sqrt();
    let min = data.iter().copied().reduce(f64::min).unwrap_or(0.0);
    let max = data.iter().copied().reduce(f64::max).unwrap_or(0.0);

    ChannelStats {
        channel_name: name.to_string(),
        mean,
        std_dev,
        min,
        max,
        sample_count: data.len(),
    }
}

/// Generate a human-readable summary of the diagnosis context suitable
/// for inclusion in an LLM prompt.
pub fn generate_summary(ctx: &DiagnosisContext) -> String {
    let mut lines = Vec::new();

    lines.push(format!("Recommended action: {:?}", ctx.action));
    lines.push(String::new());
    lines.push(ctx.divergence_summary.clone());
    lines.push(String::new());

    if !ctx.sim_stats.is_empty() {
        lines.push("Sim channel statistics:".to_string());
        for s in &ctx.sim_stats {
            lines.push(format!(
                "  {}: mean={:.4}, std={:.4}, min={:.4}, max={:.4}, n={}",
                s.channel_name, s.mean, s.std_dev, s.min, s.max, s.sample_count,
            ));
        }
        lines.push(String::new());
    }

    if !ctx.real_stats.is_empty() {
        lines.push("Real channel statistics:".to_string());
        for s in &ctx.real_stats {
            lines.push(format!(
                "  {}: mean={:.4}, std={:.4}, min={:.4}, max={:.4}, n={}",
                s.channel_name, s.mean, s.std_dev, s.min, s.max, s.sample_count,
            ));
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_computation_correctness() {
        let data = vec![2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let stats = compute_channel_stats("test", &data);
        assert_eq!(stats.channel_name, "test");
        assert_eq!(stats.sample_count, 8);
        assert!((stats.mean - 5.0).abs() < f64::EPSILON);
        assert!((stats.min - 2.0).abs() < f64::EPSILON);
        assert!((stats.max - 9.0).abs() < f64::EPSILON);
        // Variance = 4.0, std_dev = 2.0
        assert!((stats.std_dev - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_data_handling() {
        let stats = compute_channel_stats("empty", &[]);
        assert_eq!(stats.sample_count, 0);
        assert!((stats.mean).abs() < f64::EPSILON);
        assert!((stats.std_dev).abs() < f64::EPSILON);
    }

    #[test]
    fn single_element_stats() {
        let stats = compute_channel_stats("one", &[42.0]);
        assert_eq!(stats.sample_count, 1);
        assert!((stats.mean - 42.0).abs() < f64::EPSILON);
        assert!((stats.std_dev).abs() < f64::EPSILON);
        assert!((stats.min - 42.0).abs() < f64::EPSILON);
        assert!((stats.max - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn summary_generation() {
        let ctx = DiagnosisContext {
            sim_stats: vec![compute_channel_stats("vel", &[1.0, 2.0, 3.0])],
            real_stats: vec![compute_channel_stats("vel", &[1.5, 2.5, 3.5])],
            divergence_summary: "Velocity channel shows moderate drift.".into(),
            action: DiagnosisAction::Investigate,
        };
        let summary = generate_summary(&ctx);
        assert!(summary.contains("Investigate"));
        assert!(summary.contains("Velocity channel shows moderate drift."));
        assert!(summary.contains("Sim channel statistics:"));
        assert!(summary.contains("Real channel statistics:"));
    }

    #[test]
    fn summary_with_empty_stats() {
        let ctx = DiagnosisContext {
            sim_stats: Vec::new(),
            real_stats: Vec::new(),
            divergence_summary: "No data available.".into(),
            action: DiagnosisAction::Escalate,
        };
        let summary = generate_summary(&ctx);
        assert!(summary.contains("Escalate"));
        assert!(summary.contains("No data available."));
        assert!(!summary.contains("Sim channel statistics:"));
    }

    #[test]
    fn serde_roundtrip_context() {
        let ctx = DiagnosisContext {
            sim_stats: vec![compute_channel_stats("ch1", &[1.0, 2.0])],
            real_stats: vec![compute_channel_stats("ch1", &[1.5, 2.5])],
            divergence_summary: "test".into(),
            action: DiagnosisAction::Pass,
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let deser: DiagnosisContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.action, DiagnosisAction::Pass);
        assert_eq!(deser.sim_stats.len(), 1);
    }
}
