use serde::{Deserialize, Serialize};

/// Top-level environment configuration parsed from `roz.env.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    pub name: String,
    pub kind: EnvironmentKind,
    #[serde(default)]
    pub simulation: Option<SimulationConfig>,
    #[serde(default)]
    pub hardware: Option<HardwareConfig>,
    #[serde(default)]
    pub toolchain: Option<ToolchainConfig>,
    #[serde(default)]
    pub safety: Option<SafetyConfig>,
    #[serde(default)]
    pub streams: Vec<String>,
}

/// The environment execution mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    Simulation,
    Hardware,
    Hybrid,
}

/// Configuration for simulation environments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationConfig {
    pub engine: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub world: Option<String>,
    #[serde(default)]
    pub headless: bool,
}

/// Configuration for hardware environments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareConfig {
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub host_type: Option<String>,
}

/// Toolchain and dependency configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainConfig {
    #[serde(default)]
    pub ros_distro: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
}

/// Safety constraints applied to the environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub max_speed_mps: Option<f64>,
    #[serde(default)]
    pub geofence: Option<String>,
}

impl EnvironmentConfig {
    /// Parse an `EnvironmentConfig` from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simulation_yaml() {
        let yaml = r"
name: sim-lab
kind: simulation
simulation:
  engine: gazebo
  image: ros:humble-gazebo
  world: warehouse.sdf
  headless: true
streams:
  - lidar-front
  - camera-rgb
";
        let config = EnvironmentConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.name, "sim-lab");
        assert_eq!(config.kind, EnvironmentKind::Simulation);
        let sim = config.simulation.unwrap();
        assert_eq!(sim.engine, "gazebo");
        assert_eq!(sim.image.as_deref(), Some("ros:humble-gazebo"));
        assert_eq!(sim.world.as_deref(), Some("warehouse.sdf"));
        assert!(sim.headless);
        assert_eq!(config.streams, vec!["lidar-front", "camera-rgb"]);
        assert!(config.hardware.is_none());
    }

    #[test]
    fn parse_hardware_yaml() {
        let yaml = r"
name: factory-floor
kind: hardware
hardware:
  required_capabilities:
    - gpu
    - ros2
  host_type: edge
safety:
  policy: warehouse-default
  max_speed_mps: 1.5
  geofence: loading-dock
";
        let config = EnvironmentConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.name, "factory-floor");
        assert_eq!(config.kind, EnvironmentKind::Hardware);
        let hw = config.hardware.unwrap();
        assert_eq!(hw.required_capabilities, vec!["gpu", "ros2"]);
        assert_eq!(hw.host_type.as_deref(), Some("edge"));
        let safety = config.safety.unwrap();
        assert_eq!(safety.policy.as_deref(), Some("warehouse-default"));
        assert_eq!(safety.max_speed_mps, Some(1.5));
        assert!(config.simulation.is_none());
    }

    #[test]
    fn parse_hybrid_yaml() {
        let yaml = r"
name: mixed-env
kind: hybrid
simulation:
  engine: isaac-sim
hardware:
  required_capabilities:
    - camera
toolchain:
  ros_distro: humble
  packages:
    - nav2
    - slam_toolbox
";
        let config = EnvironmentConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.name, "mixed-env");
        assert_eq!(config.kind, EnvironmentKind::Hybrid);
        assert!(config.simulation.is_some());
        assert!(config.hardware.is_some());
        let tc = config.toolchain.unwrap();
        assert_eq!(tc.ros_distro.as_deref(), Some("humble"));
        assert_eq!(tc.packages, vec!["nav2", "slam_toolbox"]);
    }

    #[test]
    fn invalid_kind_returns_error() {
        let yaml = r"
name: bad
kind: imaginary
";
        let result = EnvironmentConfig::from_yaml(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("imaginary"), "error should mention the bad value: {err}");
    }

    #[test]
    fn missing_name_returns_error() {
        let yaml = r"
kind: simulation
";
        let result = EnvironmentConfig::from_yaml(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("name"), "error should mention missing field: {err}");
    }

    #[test]
    fn missing_kind_returns_error() {
        let yaml = r"
name: no-kind
";
        let result = EnvironmentConfig::from_yaml(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("kind"), "error should mention missing field: {err}");
    }

    #[test]
    fn defaults_empty_when_omitted() {
        let yaml = r"
name: minimal
kind: simulation
";
        let config = EnvironmentConfig::from_yaml(yaml).unwrap();
        assert!(config.simulation.is_none());
        assert!(config.hardware.is_none());
        assert!(config.toolchain.is_none());
        assert!(config.safety.is_none());
        assert!(config.streams.is_empty());
    }

    #[test]
    fn roundtrip_through_json() {
        let yaml = r"
name: roundtrip
kind: hardware
hardware:
  required_capabilities:
    - lidar
";
        let config = EnvironmentConfig::from_yaml(yaml).unwrap();
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["name"], "roundtrip");
        assert_eq!(json["kind"], "hardware");
        assert_eq!(json["hardware"]["required_capabilities"][0], "lidar");

        // Deserialize back
        let restored: EnvironmentConfig = serde_json::from_value(json).unwrap();
        assert_eq!(restored.name, config.name);
        assert_eq!(restored.kind, config.kind);
    }
}
