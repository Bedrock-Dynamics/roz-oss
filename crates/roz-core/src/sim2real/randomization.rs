use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

/// A probability distribution for randomization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RandomDistribution {
    Uniform { min: f64, max: f64 },
    Normal { mean: f64, std_dev: f64 },
}

/// A single named parameter with its randomization distribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RandomizationParam {
    pub name: String,
    pub distribution: RandomDistribution,
}

/// A named group of randomization parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RandomizationGroup {
    pub name: String,
    pub parameters: Vec<RandomizationParam>,
}

/// Full domain randomization configuration broken into physical domains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainRandomizationConfig {
    pub physics: Vec<RandomizationGroup>,
    pub sensors: Vec<RandomizationGroup>,
    pub environmental: Vec<RandomizationGroup>,
    pub actuators: Vec<RandomizationGroup>,
}

/// Sample a single value from the given distribution using a
/// deterministic seed.
///
/// For `Uniform`: produces a value in `[min, max]`.
/// For `Normal`: uses the Box-Muller transform to produce a normally
/// distributed sample.
pub fn sample(dist: &RandomDistribution, seed: u64) -> f64 {
    let mut rng = StdRng::seed_from_u64(seed);
    match dist {
        RandomDistribution::Uniform { min, max } => rng.gen_range(*min..=*max),
        RandomDistribution::Normal { mean, std_dev } => {
            let u1: f64 = rng.gen_range(0.0001_f64..1.0);
            let u2: f64 = rng.gen_range(0.0..std::f64::consts::TAU);
            mean + std_dev * (-2.0 * u1.ln()).sqrt() * u2.cos()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_sample_in_range() {
        let dist = RandomDistribution::Uniform { min: 0.0, max: 10.0 };
        for seed in 0..100 {
            let v = sample(&dist, seed);
            assert!((0.0..=10.0).contains(&v), "value {v} out of [0, 10]");
        }
    }

    #[test]
    fn normal_sample_near_mean() {
        let dist = RandomDistribution::Normal {
            mean: 5.0,
            std_dev: 0.001,
        };
        let v = sample(&dist, 42);
        assert!((v - 5.0).abs() < 0.1, "value {v} too far from mean 5.0");
    }

    #[test]
    fn deterministic_same_seed() {
        let dist = RandomDistribution::Uniform {
            min: -100.0,
            max: 100.0,
        };
        let a = sample(&dist, 999);
        let b = sample(&dist, 999);
        assert!((a - b).abs() < f64::EPSILON);
    }

    #[test]
    fn different_seeds_differ() {
        let dist = RandomDistribution::Uniform {
            min: 0.0,
            max: 1_000_000.0,
        };
        let a = sample(&dist, 1);
        let b = sample(&dist, 2);
        // Statistically near-impossible for two different seeds to produce the
        // exact same f64 from a wide uniform distribution.
        assert!((a - b).abs() > f64::EPSILON);
    }

    #[test]
    fn serde_roundtrip_config() {
        let config = DomainRandomizationConfig {
            physics: vec![RandomizationGroup {
                name: "gravity".into(),
                parameters: vec![RandomizationParam {
                    name: "g".into(),
                    distribution: RandomDistribution::Normal {
                        mean: 9.81,
                        std_dev: 0.05,
                    },
                }],
            }],
            sensors: Vec::new(),
            environmental: Vec::new(),
            actuators: Vec::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let deser: DomainRandomizationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.physics.len(), 1);
        assert_eq!(deser.physics[0].name, "gravity");
    }

    #[test]
    fn distribution_serde_variant_names() {
        let u = RandomDistribution::Uniform { min: 0.0, max: 1.0 };
        let json = serde_json::to_string(&u).unwrap();
        assert!(json.contains("\"uniform\""));

        let n = RandomDistribution::Normal {
            mean: 0.0,
            std_dev: 1.0,
        };
        let json = serde_json::to_string(&n).unwrap();
        assert!(json.contains("\"normal\""));
    }
}
