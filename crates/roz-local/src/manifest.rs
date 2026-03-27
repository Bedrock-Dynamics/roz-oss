use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct ProjectManifest {
    pub project: ProjectConfig,
    pub model: ModelConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub guards: GuardsConfig,
    #[serde(default)]
    pub simulation: SimulationConfig,
}

#[derive(Debug, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    #[serde(default = "default_framework")]
    pub framework: String,
}

fn default_framework() -> String {
    "custom".into()
}

#[derive(Debug, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub name: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

fn default_base_url() -> String {
    "http://localhost:11434/v1".into()
}

#[derive(Debug, Deserialize)]
pub struct SafetyConfig {
    #[serde(default = "default_velocity")]
    pub max_velocity_ms: f64,
    #[serde(default = "default_altitude")]
    pub max_altitude_m: f64,
    #[serde(default)]
    pub geofence: Option<String>,
    #[serde(default)]
    pub require_approval: Vec<String>,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            max_velocity_ms: default_velocity(),
            max_altitude_m: default_altitude(),
            geofence: None,
            require_approval: Vec::new(),
        }
    }
}

const fn default_velocity() -> f64 {
    10.0
}
const fn default_altitude() -> f64 {
    120.0
}

#[derive(Debug, Deserialize)]
pub struct GuardsConfig {
    #[serde(default = "default_guards")]
    pub enabled: Vec<String>,
}

impl Default for GuardsConfig {
    fn default() -> Self {
        Self {
            enabled: default_guards(),
        }
    }
}

fn default_guards() -> Vec<String> {
    vec!["velocity".into(), "battery".into(), "geofence".into(), "rate".into()]
}

#[derive(Debug, Deserialize)]
pub struct SimulationConfig {
    /// Docker image override (default: bedrockdynamics/substrate-sim:px4-gazebo-humble).
    #[serde(default)]
    pub image: Option<String>,
    /// Default vehicle model.
    #[serde(default = "default_sim_model")]
    pub vehicle_model: String,
    /// Default world name.
    #[serde(default = "default_sim_world")]
    pub world: String,
    /// CPU limit for the container.
    #[serde(default)]
    pub cpu_limit: Option<String>,
    /// Memory limit for the container.
    #[serde(default)]
    pub memory_limit: Option<String>,
}

fn default_sim_model() -> String {
    "x500".to_string()
}
fn default_sim_world() -> String {
    "default".to_string()
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            image: None,
            vehicle_model: default_sim_model(),
            world: default_sim_world(),
            cpu_limit: None,
            memory_limit: None,
        }
    }
}

impl SimulationConfig {
    /// Convert to a `SimContainerConfig` for the Docker launcher.
    pub fn to_container_config(&self) -> crate::docker::SimContainerConfig {
        crate::docker::SimContainerConfig {
            image: self
                .image
                .clone()
                .unwrap_or_else(|| crate::docker::DEFAULT_SIM_IMAGE.to_string()),
            px4_model: self.vehicle_model.clone(),
            px4_world: self.world.clone(),
            cpu_limit: self.cpu_limit.clone(),
            memory_limit: self.memory_limit.clone(),
        }
    }
}

impl ProjectManifest {
    pub fn load(project_dir: &Path) -> Result<Self, ManifestError> {
        let path = project_dir.join("roz.toml");
        let content = std::fs::read_to_string(&path).map_err(|e| ManifestError::Io {
            path: path.clone(),
            source: e,
        })?;
        toml::from_str(&content).map_err(|e| ManifestError::Parse { path, source: e })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_minimal_manifest() {
        let toml_str = r#"
[project]
name = "test-project"

[model]
provider = "ollama"
name = "llama3.1"
"#;
        let manifest: ProjectManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.project.name, "test-project");
        assert_eq!(manifest.project.framework, "custom");
        assert_eq!(manifest.model.provider, "ollama");
        assert_eq!(manifest.model.name, "llama3.1");
        assert_eq!(manifest.model.base_url, "http://localhost:11434/v1");
        assert_eq!(manifest.guards.enabled.len(), 4);
    }

    #[test]
    fn parse_full_manifest() {
        let toml_str = r#"
[project]
name = "my-drone"
framework = "px4"

[model]
provider = "anthropic"
name = "claude-sonnet-4-6"
base_url = "https://api.anthropic.com"

[safety]
max_velocity_ms = 5.0
max_altitude_m = 50.0
geofence = "safety/default.toml"
require_approval = ["arm", "takeoff"]

[guards]
enabled = ["velocity", "geofence"]
"#;
        let manifest: ProjectManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.project.framework, "px4");
        assert_eq!(manifest.safety.max_velocity_ms, 5.0);
        assert_eq!(manifest.safety.require_approval, vec!["arm", "takeoff"]);
        assert_eq!(manifest.guards.enabled, vec!["velocity", "geofence"]);
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create(dir.path().join("roz.toml")).unwrap();
        writeln!(
            f,
            r#"
[project]
name = "test"
[model]
provider = "ollama"
name = "llama3.1"
"#
        )
        .unwrap();
        let manifest = ProjectManifest::load(dir.path()).unwrap();
        assert_eq!(manifest.project.name, "test");
    }

    #[test]
    fn load_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = ProjectManifest::load(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn parse_manifest_with_simulation() {
        let toml_str = r#"
[project]
name = "my-drone"

[model]
provider = "anthropic"
name = "claude-sonnet-4-5"

[simulation]
vehicle_model = "rc_cessna"
world = "baylands"
cpu_limit = "2"
memory_limit = "2G"
"#;
        let manifest: ProjectManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.simulation.vehicle_model, "rc_cessna");
        assert_eq!(manifest.simulation.world, "baylands");
        assert_eq!(manifest.simulation.cpu_limit.as_deref(), Some("2"));
    }

    #[test]
    fn manifest_without_simulation_uses_defaults() {
        let toml_str = r#"
[project]
name = "test"

[model]
provider = "ollama"
name = "llama3.1"
"#;
        let manifest: ProjectManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.simulation.vehicle_model, "x500");
        assert_eq!(manifest.simulation.world, "default");
        assert!(manifest.simulation.image.is_none());
    }

    #[test]
    fn simulation_config_to_container_config() {
        let sim = SimulationConfig {
            image: Some("custom:latest".into()),
            vehicle_model: "standard_vtol".into(),
            world: "baylands".into(),
            cpu_limit: Some("8".into()),
            memory_limit: Some("8G".into()),
        };
        let config = sim.to_container_config();
        assert_eq!(config.image, "custom:latest");
        assert_eq!(config.px4_model, "standard_vtol");
    }
}
