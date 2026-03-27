use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::sim2real::report::MetricKind;

/// A named parameter with a set of values to sweep over.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioParameter {
    pub name: String,
    pub values: Vec<serde_json::Value>,
}

/// A criterion that must be met for a scenario to be considered successful.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuccessCriterion {
    pub metric: MetricKind,
    pub channel: String,
    pub threshold: f64,
}

/// Configuration for recording data during a scenario run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingConfig {
    pub duration_secs: u32,
    pub channels: Vec<String>,
}

/// Full scenario definition including parameters, criteria, and recording config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioDef {
    pub name: String,
    pub description: String,
    pub parameters: Vec<ScenarioParameter>,
    pub success_criteria: Vec<SuccessCriterion>,
    pub recording: RecordingConfig,
}

/// Compute the Cartesian product of all parameter value lists.
///
/// Returns one `HashMap<String, Value>` per combination.  If `params`
/// is empty the result is a single empty map (the trivial product).
pub fn generate_sweep_matrix(params: &[ScenarioParameter]) -> Vec<HashMap<String, serde_json::Value>> {
    let mut results: Vec<HashMap<String, serde_json::Value>> = vec![HashMap::new()];

    for param in params {
        let mut next = Vec::with_capacity(results.len() * param.values.len());
        for existing in &results {
            for val in &param.values {
                let mut combo = existing.clone();
                combo.insert(param.name.clone(), val.clone());
                next.push(combo);
            }
        }
        results = next;
    }

    results
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn single_param_sweep() {
        let params = vec![ScenarioParameter {
            name: "speed".into(),
            values: vec![json!(1), json!(2), json!(3)],
        }];
        let matrix = generate_sweep_matrix(&params);
        assert_eq!(matrix.len(), 3);
    }

    #[test]
    fn two_params_cartesian_product() {
        let params = vec![
            ScenarioParameter {
                name: "speed".into(),
                values: vec![json!(1), json!(2)],
            },
            ScenarioParameter {
                name: "mass".into(),
                values: vec![json!(0.5)],
            },
        ];
        let matrix = generate_sweep_matrix(&params);
        assert_eq!(matrix.len(), 2);
        assert_eq!(matrix[0]["speed"], json!(1));
        assert_eq!(matrix[0]["mass"], json!(0.5));
        assert_eq!(matrix[1]["speed"], json!(2));
        assert_eq!(matrix[1]["mass"], json!(0.5));
    }

    #[test]
    fn three_params_cartesian() {
        let params = vec![
            ScenarioParameter {
                name: "a".into(),
                values: vec![json!(1), json!(2)],
            },
            ScenarioParameter {
                name: "b".into(),
                values: vec![json!(3), json!(4)],
            },
            ScenarioParameter {
                name: "c".into(),
                values: vec![json!(5), json!(6)],
            },
        ];
        let matrix = generate_sweep_matrix(&params);
        assert_eq!(matrix.len(), 8); // 2 * 2 * 2
    }

    #[test]
    fn empty_params_single_empty_map() {
        let matrix = generate_sweep_matrix(&[]);
        assert_eq!(matrix.len(), 1);
        assert!(matrix[0].is_empty());
    }

    #[test]
    fn single_value_param() {
        let params = vec![ScenarioParameter {
            name: "x".into(),
            values: vec![json!(42)],
        }];
        let matrix = generate_sweep_matrix(&params);
        assert_eq!(matrix.len(), 1);
        assert_eq!(matrix[0]["x"], json!(42));
    }

    #[test]
    fn large_sweep() {
        let params = vec![
            ScenarioParameter {
                name: "a".into(),
                values: (0..5).map(|i| json!(i)).collect(),
            },
            ScenarioParameter {
                name: "b".into(),
                values: (0..4).map(|i| json!(i)).collect(),
            },
            ScenarioParameter {
                name: "c".into(),
                values: (0..3).map(|i| json!(i)).collect(),
            },
        ];
        let matrix = generate_sweep_matrix(&params);
        assert_eq!(matrix.len(), 60); // 5 * 4 * 3
    }

    #[test]
    fn scenario_def_serde_roundtrip() {
        let def = ScenarioDef {
            name: "pick_and_place".into(),
            description: "Test pick and place operation".into(),
            parameters: vec![ScenarioParameter {
                name: "speed".into(),
                values: vec![json!(1.0), json!(2.0)],
            }],
            success_criteria: vec![SuccessCriterion {
                metric: MetricKind::Rmse,
                channel: "gripper_force".into(),
                threshold: 0.1,
            }],
            recording: RecordingConfig {
                duration_secs: 30,
                channels: vec!["gripper_force".into(), "joint_angles".into()],
            },
        };
        let json_str = serde_json::to_string(&def).unwrap();
        let deser: ScenarioDef = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deser.name, "pick_and_place");
        assert_eq!(deser.parameters.len(), 1);
    }

    #[test]
    fn all_combinations_are_unique() {
        let params = vec![
            ScenarioParameter {
                name: "x".into(),
                values: vec![json!(1), json!(2)],
            },
            ScenarioParameter {
                name: "y".into(),
                values: vec![json!(3), json!(4)],
            },
        ];
        let matrix = generate_sweep_matrix(&params);
        // Check uniqueness by converting to sorted key-value strings
        let strs: Vec<String> = matrix.iter().map(|m| format!("{m:?}")).collect();
        let unique: std::collections::HashSet<&String> = strs.iter().collect();
        assert_eq!(unique.len(), matrix.len());
    }
}
