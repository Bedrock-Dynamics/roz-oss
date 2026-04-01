//! Experiment execution and data collection framework.

use serde::{Deserialize, Serialize};

/// Tags a session with an experiment variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentTag {
    pub experiment_id: String,
    pub variant_id: String,
}

/// How sessions are assigned to variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentStrategy {
    Random,
    RoundRobin,
    Manual,
}

/// A variant in an experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantConfig {
    pub variant_id: String,
    pub description: String,
    pub blueprint_override: Option<String>,
    pub constitution_override: Option<String>,
    pub model_override: Option<String>,
}

/// Experiment configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentConfig {
    pub experiment_id: String,
    pub name: String,
    pub variants: Vec<VariantConfig>,
    pub assignment_strategy: AssignmentStrategy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experiment_tag_serde() {
        let tag = ExperimentTag {
            experiment_id: "exp-001".into(),
            variant_id: "control".into(),
        };
        let json = serde_json::to_string(&tag).unwrap();
        let back: ExperimentTag = serde_json::from_str(&json).unwrap();
        assert_eq!(tag, back);
    }

    #[test]
    fn experiment_config_serde() {
        let config = ExperimentConfig {
            experiment_id: "exp-001".into(),
            name: "constitution A/B test".into(),
            variants: vec![
                VariantConfig {
                    variant_id: "control".into(),
                    description: "baseline constitution".into(),
                    blueprint_override: None,
                    constitution_override: None,
                    model_override: None,
                },
                VariantConfig {
                    variant_id: "treatment".into(),
                    description: "new verification tier".into(),
                    blueprint_override: None,
                    constitution_override: Some("constitutions/v2.md".into()),
                    model_override: None,
                },
            ],
            assignment_strategy: AssignmentStrategy::Random,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ExperimentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.variants.len(), back.variants.len());
        assert_eq!(back.assignment_strategy, AssignmentStrategy::Random);
    }
}
