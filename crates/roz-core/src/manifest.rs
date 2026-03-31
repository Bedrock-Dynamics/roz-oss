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
    /// Channel manifest for the WASM controller interface.
    /// If present, used to build the [`crate::channels::ChannelManifest`] for this robot.
    pub channels: Option<ChannelConfig>,
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

/// Channel configuration section of `robot.toml`.
///
/// Maps directly to [`crate::channels::ChannelManifest`] via
/// [`RobotManifest::channel_manifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Robot identifier (matches `ChannelManifest::robot_id`).
    pub robot_id: String,
    /// Robot class (e.g. `"manipulator"`, `"expressive"`, `"drone"`).
    pub robot_class: String,
    /// Control loop rate in Hz.
    pub control_rate_hz: u32,
    /// Command channels (written by the controller each tick).
    #[serde(default)]
    pub commands: Vec<ChannelDef>,
    /// State channels (read by the controller each tick).
    #[serde(default)]
    pub states: Vec<ChannelDef>,
}

/// A single channel definition in `robot.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDef {
    /// Channel name (`ros2_control` convention: `"joint_name/interface_type"`).
    pub name: String,
    /// `"position"`, `"velocity"`, or `"effort"`.
    #[serde(rename = "type")]
    pub interface_type: String,
    /// Physical unit string (e.g. `"rad"`, `"m"`, `"rad/s"`).
    pub unit: String,
    /// `[min, max]` value limits.
    pub limits: [f64; 2],
    /// Safe default value (usually `0.0`).
    #[serde(default)]
    pub default: f64,
    /// Max rate of change per tick for acceleration/jerk limiting.
    pub max_rate_of_change: Option<f64>,
    /// Index of the corresponding position state channel.
    pub position_state_index: Option<usize>,
    /// Cross-channel delta constraint: `[other_command_index, max_delta]`.
    pub max_delta_from: Option<[f64; 2]>,
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

    /// Build a [`crate::channels::ChannelManifest`] from the `[channels]` section.
    ///
    /// Returns `None` if no `[channels]` section is present in the manifest.
    ///
    /// # Panics
    ///
    /// Panics if an `interface_type` string is not one of `"position"`, `"velocity"`, or `"effort"`.
    #[must_use]
    pub fn channel_manifest(&self) -> Option<crate::channels::ChannelManifest> {
        use crate::channels::{ChannelDescriptor, ChannelManifest, InterfaceType};

        let ch = self.channels.as_ref()?;

        let convert = |def: &ChannelDef| -> ChannelDescriptor {
            let interface_type = match def.interface_type.as_str() {
                "position" => InterfaceType::Position,
                "velocity" => InterfaceType::Velocity,
                "effort" => InterfaceType::Effort,
                other => panic!("unknown interface_type in robot.toml: {other:?}"),
            };
            ChannelDescriptor {
                name: def.name.clone(),
                interface_type,
                unit: def.unit.clone(),
                limits: (def.limits[0], def.limits[1]),
                default: def.default,
                max_rate_of_change: def.max_rate_of_change,
                position_state_index: def.position_state_index,
                max_delta_from: def.max_delta_from.map(|[idx, delta]| {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let index = idx as usize;
                    (index, delta)
                }),
            }
        };

        Some(ChannelManifest {
            robot_id: ch.robot_id.clone(),
            robot_class: ch.robot_class.clone(),
            control_rate_hz: ch.control_rate_hz,
            commands: ch.commands.iter().map(convert).collect(),
            states: ch.states.iter().map(convert).collect(),
        })
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

    #[test]
    fn channel_manifest_from_robot_toml() {
        let toml_str = r#"
[robot]
name = "test"
description = "test"

[channels]
robot_id = "test"
robot_class = "test"
control_rate_hz = 50

[[channels.commands]]
name = "joint/position"
type = "position"
unit = "rad"
limits = [-1.0, 1.0]
position_state_index = 0

[[channels.states]]
name = "joint/position"
type = "position"
unit = "rad"
limits = [-1.0, 1.0]
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let ch = manifest.channel_manifest().unwrap();
        assert_eq!(ch.robot_id, "test");
        assert_eq!(ch.control_rate_hz, 50);
        assert_eq!(ch.commands.len(), 1);
        assert_eq!(ch.states.len(), 1);
        assert_eq!(ch.commands[0].interface_type, crate::channels::InterfaceType::Position);
        assert_eq!(ch.commands[0].limits, (-1.0, 1.0));
        assert_eq!(ch.commands[0].position_state_index, Some(0));
    }

    #[test]
    fn channel_manifest_none_when_no_channels_section() {
        let manifest: RobotManifest = toml::from_str(EXAMPLE_TOML).unwrap();
        assert!(manifest.channel_manifest().is_none());
    }

    #[test]
    fn channel_manifest_with_max_delta_from() {
        let toml_str = r#"
[robot]
name = "test"
description = "test"

[channels]
robot_id = "test"
robot_class = "test"
control_rate_hz = 100

[[channels.commands]]
name = "a/position"
type = "position"
unit = "rad"
limits = [-3.14, 3.14]
max_delta_from = [1, 1.5]

[[channels.commands]]
name = "b/position"
type = "position"
unit = "rad"
limits = [-3.14, 3.14]
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let ch = manifest.channel_manifest().unwrap();
        assert_eq!(ch.commands[0].max_delta_from, Some((1, 1.5)));
        assert_eq!(ch.commands[1].max_delta_from, None);
    }

    #[test]
    fn reachy_mini_robot_toml_loads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/reachy-mini/robot.toml");
        if path.exists() {
            let manifest = RobotManifest::load(&path).unwrap();
            assert_eq!(manifest.robot.name, "reachy-mini");
            let ch = manifest.channel_manifest().unwrap();
            assert_eq!(ch.robot_id, "reachy_mini");
            assert_eq!(ch.robot_class, "expressive");
            assert_eq!(ch.control_rate_hz, 50);
            assert_eq!(ch.commands.len(), 9);
            assert_eq!(ch.states.len(), 9);

            // Verify head/orientation.yaw has max_delta_from body/yaw
            assert_eq!(ch.commands[5].name, "head/orientation.yaw");
            // 65 degrees in radians = 1.1344640137963142
            let expected_delta = 65.0_f64.to_radians();
            let (idx, delta) = ch.commands[5].max_delta_from.unwrap();
            assert_eq!(idx, 6);
            assert!(
                (delta - expected_delta).abs() < 1e-10,
                "max_delta_from delta should be 65 deg in radians: got {delta}, expected {expected_delta}"
            );

            // All channels are position type
            assert!(
                ch.commands
                    .iter()
                    .all(|c| c.interface_type == crate::channels::InterfaceType::Position)
            );
        }
    }
}
