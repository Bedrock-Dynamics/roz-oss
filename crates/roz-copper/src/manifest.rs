//! Parser for `robot.toml` -- the hardware manifest describing a robot's
//! capabilities for the LLM system prompt.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

/// Top-level manifest parsed from `robot.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RobotManifest {
    /// Basic robot identity.
    pub robot: RobotInfo,
    /// Hardware capabilities (e.g. joint groups, grippers).
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Attached sensors.
    #[serde(default)]
    pub sensors: Vec<Sensor>,
    /// Safety parameters.
    pub safety: Option<SafetyConfig>,
}

/// Robot identity metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RobotInfo {
    /// Human-readable robot name.
    pub name: String,
    /// One-line description of the robot.
    pub description: String,
}

/// A single hardware capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    /// Capability name (e.g. `left_arm`).
    pub name: String,
    /// Capability type (e.g. `joint_group`, `gripper`).
    #[serde(rename = "type")]
    pub cap_type: String,
    /// Actions this capability supports.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Joint names (for joint-group capabilities).
    #[serde(default)]
    pub joints: Vec<String>,
    /// Optional limits (free-form TOML table).
    pub limits: Option<toml::Value>,
}

/// A sensor attached to the robot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sensor {
    /// Sensor name (e.g. `wrist_imu`).
    pub name: String,
    /// Sensor type (e.g. `imu`, `force_torque`, `camera`).
    #[serde(rename = "type")]
    pub sensor_type: String,
    /// Data channels the sensor provides.
    #[serde(default)]
    pub data: Vec<String>,
    /// Publish rate in Hz, if applicable.
    pub rate_hz: Option<u32>,
}

/// Safety configuration for the robot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    /// What happens on e-stop (e.g. `hold_position`, `power_off`).
    pub e_stop_behavior: Option<String>,
    /// Watchdog timeout before triggering e-stop.
    pub watchdog_timeout_ms: Option<u64>,
    /// Maximum allowable contact force.
    pub max_contact_force_n: Option<f64>,
    /// Workspace boundaries (free-form TOML table).
    pub workspace_bounds_m: Option<toml::Value>,
}

impl RobotManifest {
    /// Load a `RobotManifest` from a `robot.toml` file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the TOML is invalid.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let manifest: Self = toml::from_str(&contents)?;
        Ok(manifest)
    }

    /// Render as a concise system prompt block (~400-500 tokens).
    #[must_use]
    pub fn to_system_prompt(&self) -> String {
        let mut prompt = format!("# Robot: {}\n{}\n\n", self.robot.name, self.robot.description);

        if !self.capabilities.is_empty() {
            prompt.push_str("## Capabilities\n");
            for cap in &self.capabilities {
                let _ = writeln!(
                    prompt,
                    "- **{}** ({}): {}",
                    cap.name,
                    cap.cap_type,
                    cap.actions.join(", ")
                );
            }
            prompt.push('\n');
        }

        if !self.sensors.is_empty() {
            prompt.push_str("## Sensors\n");
            for sensor in &self.sensors {
                let rate = sensor.rate_hz.map_or(String::new(), |r| format!(" @ {r}Hz"));
                let _ = writeln!(
                    prompt,
                    "- **{}** ({}){}: {}",
                    sensor.name,
                    sensor.sensor_type,
                    rate,
                    sensor.data.join(", ")
                );
            }
            prompt.push('\n');
        }

        if let Some(ref safety) = self.safety {
            prompt.push_str("## Safety Limits\n");
            if let Some(ref estop) = safety.e_stop_behavior {
                let _ = writeln!(prompt, "- E-stop: {estop}");
            }
            if let Some(force) = safety.max_contact_force_n {
                let _ = writeln!(prompt, "- Max contact force: {force}N");
            }
            if let Some(timeout) = safety.watchdog_timeout_ms {
                let _ = writeln!(prompt, "- Watchdog timeout: {timeout}ms");
            }
        }

        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_TOML: &str = r#"
[robot]
name = "test-bot"
description = "A test robot"

[[capabilities]]
name = "arm"
type = "joint_group"
actions = ["move_joint", "set_velocity"]
joints = ["shoulder", "elbow"]

[[sensors]]
name = "imu"
type = "imu"
data = ["orientation", "angular_velocity"]
rate_hz = 100

[safety]
e_stop_behavior = "hold_position"
max_contact_force_n = 80.0
watchdog_timeout_ms = 500
"#;

    #[test]
    fn parse_robot_toml() {
        let manifest: RobotManifest = toml::from_str(EXAMPLE_TOML).unwrap();
        assert_eq!(manifest.robot.name, "test-bot");
        assert_eq!(manifest.capabilities.len(), 1);
        assert_eq!(manifest.sensors.len(), 1);
        assert!(manifest.safety.is_some());
    }

    #[test]
    fn system_prompt_contains_key_info() {
        let manifest: RobotManifest = toml::from_str(EXAMPLE_TOML).unwrap();
        let prompt = manifest.to_system_prompt();
        assert!(prompt.contains("test-bot"));
        assert!(prompt.contains("arm"));
        assert!(prompt.contains("imu"));
        assert!(prompt.contains("80N"));
    }
}
