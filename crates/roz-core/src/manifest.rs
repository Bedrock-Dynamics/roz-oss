//! Parser for the physical embodiment manifest.
//!
//! `embodiment.toml` is the canonical filename. Legacy `robot.toml` remains
//! supported as a compatibility fallback while the public surface converges.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Validation issue stamped onto channel-only runtimes synthesized from
/// `robot.toml`. These runtimes are transitional helpers, not authoritative
/// embodiment authority for live controller promotion.
pub const SYNTHETIC_EMBODIMENT_RUNTIME_ISSUE: &str = "embodiment runtime synthesized from robot.toml channel metadata";
pub const EMBODIMENT_MANIFEST_FILE: &str = "embodiment.toml";
pub const LEGACY_ROBOT_MANIFEST_FILE: &str = "robot.toml";

/// Top-level physical embodiment manifest parsed from `embodiment.toml`
/// or the legacy `robot.toml` fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbodimentManifest {
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
    /// If present, prefer projecting this into the canonical
    /// `ControlInterfaceManifest`; the legacy runtime manifest is retained only
    /// for compatibility with older Copper surfaces.
    pub channels: Option<ChannelConfig>,
    /// Daemon REST/WebSocket configuration for agent tools.
    pub daemon: Option<DaemonConfig>,
}

/// Legacy compatibility alias retained while downstream code converges on the
/// canonical embodiment vocabulary.
pub type RobotManifest = EmbodimentManifest;

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
/// Supports both the legacy runtime manifest projection
/// and the canonical [`crate::embodiment::binding::ControlInterfaceManifest`]
/// projection exposed by [`RobotManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Robot identifier for the legacy runtime manifest.
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
    /// Channel name (flat snake\_case: `"head_pitch"`, `"shoulder_pan"`).
    pub name: String,
    /// `"position"`, `"velocity"`, or `"effort"`.
    #[serde(rename = "type")]
    pub interface_type: String,
    /// Physical unit string (e.g. `"rad"`, `"m"`, `"rad/s"`).
    pub unit: String,
    /// Coordinate frame this channel is expressed in.
    ///
    /// Optional for legacy manifests; omitted values currently degrade to
    /// the empty string and should be filled explicitly in new manifests.
    #[serde(default)]
    pub frame_id: String,
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

/// Daemon configuration for REST and WebSocket robot control.
///
/// Configures how the agent's generic tools map to the daemon's specific
/// REST endpoints via body templates with `{{channel_name}}` placeholders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Base URL of the daemon (e.g. `http://localhost:8000`).
    pub base_url: String,
    /// WebSocket config for Layer 2 WASM controller bridge.
    pub websocket: Option<WebSocketConfig>,
    /// GET endpoint for reading robot state.
    pub get_state: Option<EndpointConfig>,
    /// POST endpoint for setting motor mode. Path may contain `{{mode}}`.
    pub set_motors: Option<EndpointConfig>,
    /// POST endpoint for interpolated motion. Body template with `{{channel_name}}` + `{{duration}}`.
    pub move_to: Option<MoveToConfig>,
    /// POST endpoint for playing named animations.
    pub play_animation: Option<PlayAnimationConfig>,
    /// POST endpoint for stopping motion.
    pub stop_motion: Option<EndpointConfig>,
}

/// WebSocket configuration for real-time control and sensor streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketConfig {
    /// WebSocket path (e.g. `/ws/sdk`).
    pub path: String,
    /// Message type for `set_target` commands (e.g. `set_full_target`).
    pub set_target_type: Option<String>,
    /// Body template for `set_target` WebSocket messages.
    pub set_target_body: Option<String>,
}

/// Generic REST endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// HTTP method (GET, POST, PUT, DELETE).
    pub method: String,
    /// URL path (may contain `{{placeholder}}` variables).
    pub path: String,
    /// Optional request body template.
    pub body: Option<String>,
}

/// Configuration for the `move_to` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveToConfig {
    /// HTTP method.
    pub method: String,
    /// URL path.
    pub path: String,
    /// Body template with `{{channel_name}}` placeholders for channel values
    /// and `{{duration}}` for the motion duration.
    pub body: String,
}

/// Configuration for the `play_animation` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayAnimationConfig {
    /// HTTP method.
    pub method: String,
    /// URL path prefix. Animation name appended: `{prefix}/{name}`.
    pub path_prefix: String,
    /// List of available animation names.
    #[serde(default)]
    pub available_moves: Vec<String>,
}

/// Check whether a channel name is valid flat snake\_case.
///
/// Valid names match `[a-zA-Z][a-zA-Z0-9_]{0,63}` -- no slashes, dots,
/// or leading digits.
#[must_use]
pub fn is_valid_channel_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

fn warn_if_invalid_channel_name(name: &str) {
    if !is_valid_channel_name(name) {
        tracing::warn!(
            "channel name '{}' contains invalid characters — must be [a-zA-Z][a-zA-Z0-9_]{{0,63}}",
            name
        );
    }
}

fn parse_interface_type(
    name: &str,
    interface_type: &str,
) -> Option<(
    crate::channels::InterfaceType,
    crate::embodiment::binding::CommandInterfaceType,
    crate::embodiment::binding::BindingType,
)> {
    use crate::channels::InterfaceType;
    use crate::embodiment::binding::{BindingType, CommandInterfaceType};

    warn_if_invalid_channel_name(name);
    match interface_type {
        "position" => Some((
            InterfaceType::Position,
            CommandInterfaceType::JointPosition,
            BindingType::JointPosition,
        )),
        "velocity" => Some((
            InterfaceType::Velocity,
            CommandInterfaceType::JointVelocity,
            BindingType::JointVelocity,
        )),
        "effort" => Some((
            InterfaceType::Effort,
            CommandInterfaceType::JointTorque,
            BindingType::Command,
        )),
        other => {
            tracing::warn!(
                channel = %name,
                "unknown interface_type in robot.toml: {other:?}, skipping channel"
            );
            None
        }
    }
}

impl EmbodimentManifest {
    /// Load an [`EmbodimentManifest`] from a supported manifest path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the TOML is invalid.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let manifest: Self = toml::from_str(&contents)?;
        Ok(manifest)
    }

    /// Resolve the manifest file for a project directory.
    ///
    /// Prefers canonical `embodiment.toml` and falls back to legacy
    /// `robot.toml` only for compatibility.
    #[must_use]
    pub fn project_manifest_path(project_dir: &Path) -> Option<PathBuf> {
        let canonical = project_dir.join(EMBODIMENT_MANIFEST_FILE);
        if canonical.exists() {
            return Some(canonical);
        }

        let legacy = project_dir.join(LEGACY_ROBOT_MANIFEST_FILE);
        if legacy.exists() {
            return Some(legacy);
        }

        None
    }

    /// Load the project manifest from its canonical compatibility path.
    ///
    /// # Errors
    ///
    /// Returns an error if neither supported manifest filename exists or if
    /// the selected manifest cannot be parsed.
    pub fn load_from_project_dir(project_dir: &Path) -> anyhow::Result<Self> {
        let (manifest, _) = Self::load_from_project_dir_with_path(project_dir)?;
        Ok(manifest)
    }

    /// Like [`Self::load_from_project_dir`] but also returns the winning path.
    ///
    /// # Errors
    ///
    /// Returns an error if neither supported manifest filename exists or if
    /// the selected manifest cannot be parsed.
    pub fn load_from_project_dir_with_path(project_dir: &Path) -> anyhow::Result<(Self, PathBuf)> {
        let path = Self::project_manifest_path(project_dir).ok_or_else(|| {
            anyhow::anyhow!(
                "no {} or {} found in {}",
                EMBODIMENT_MANIFEST_FILE,
                LEGACY_ROBOT_MANIFEST_FILE,
                project_dir.display()
            )
        })?;
        let manifest = Self::load(&path)?;
        Ok((manifest, path))
    }

    /// Build a legacy runtime manifest from the `[channels]` section.
    ///
    /// Returns `None` if no `[channels]` section is present in the manifest.
    ///
    /// Channels with an unrecognised `interface_type` are skipped with a warning
    /// rather than panicking, so callers always get a best-effort manifest.
    ///
    /// Prefer [`Self::control_interface_manifest`] on production paths. This
    /// legacy projection remains only for compatibility with older Copper
    /// entrypoints that still consume the legacy shape.
    #[doc(hidden)]
    #[deprecated(note = "compatibility-only projection; prefer control_interface_manifest")]
    #[must_use]
    pub fn legacy_runtime_manifest(&self) -> Option<crate::channels::LegacyRuntimeManifest> {
        use crate::channels::{ChannelDescriptor, LegacyRuntimeManifest};

        let ch = self.channels.as_ref()?;

        let convert = |def: &ChannelDef| -> Option<ChannelDescriptor> {
            let (interface_type, _, _) = parse_interface_type(&def.name, &def.interface_type)?;
            Some(ChannelDescriptor {
                name: def.name.clone(),
                interface_type,
                unit: def.unit.clone(),
                limits: (def.limits[0], def.limits[1]),
                default: def.default,
                max_rate_of_change: def.max_rate_of_change,
                position_state_index: def.position_state_index,
                max_delta_from: def.max_delta_from.and_then(|[idx, delta]| {
                    if idx < 0.0 || !idx.is_finite() {
                        tracing::warn!(
                            channel = %def.name,
                            "invalid max_delta_from index {idx}, ignoring constraint"
                        );
                        return None;
                    }
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    Some((idx as usize, delta))
                }),
            })
        };

        Some(LegacyRuntimeManifest {
            robot_id: ch.robot_id.clone(),
            robot_class: ch.robot_class.clone(),
            control_rate_hz: ch.control_rate_hz,
            commands: ch.commands.iter().filter_map(&convert).collect(),
            states: ch.states.iter().filter_map(&convert).collect(),
        })
    }

    /// Build a canonical [`crate::embodiment::binding::ControlInterfaceManifest`]
    /// from the `[channels]` section.
    ///
    /// This avoids routing local/control surfaces through the lossy legacy
    /// legacy runtime manifest shape when only the controller I/O contract is needed.
    ///
    /// Returns `None` if no `[channels]` section is present in the manifest.
    #[must_use]
    pub fn control_interface_manifest(&self) -> Option<crate::embodiment::binding::ControlInterfaceManifest> {
        use crate::embodiment::binding::{
            BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
        };

        let ch = self.channels.as_ref()?;

        let command_defs: Vec<(&ChannelDef, CommandInterfaceType, BindingType)> = ch
            .commands
            .iter()
            .filter_map(|def| {
                let (_, interface_type, binding_type) = parse_interface_type(&def.name, &def.interface_type)?;
                Some((def, interface_type, binding_type))
            })
            .collect();

        let channels: Vec<ControlChannelDef> = command_defs
            .iter()
            .map(|(def, interface_type, _)| ControlChannelDef {
                name: def.name.clone(),
                interface_type: interface_type.clone(),
                units: def.unit.clone(),
                frame_id: def.frame_id.clone(),
            })
            .collect();

        let bindings: Vec<ChannelBinding> = command_defs
            .iter()
            .enumerate()
            .map(|(channel_index, (def, _, binding_type))| ChannelBinding {
                physical_name: (*def).name.clone(),
                channel_index: channel_index as u32,
                binding_type: binding_type.clone(),
                frame_id: def.frame_id.clone(),
                units: (*def).unit.clone(),
                semantic_role: None,
            })
            .collect();

        let mut manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels,
            bindings,
        };
        manifest.stamp_digest();
        Some(manifest)
    }

    /// Build a minimal canonical [`crate::embodiment::EmbodimentRuntime`] from
    /// `robot.toml` channel metadata.
    ///
    /// This is a transitional adapter for projects that only declare
    /// `robot.toml` control channels today. It preserves canonical control
    /// bindings and non-empty frame IDs so downstream helper code can stop
    /// depending purely on placeholder digest tuples, but it is not
    /// authoritative enough for live controller promotion.
    ///
    /// The synthesized model is intentionally conservative:
    /// - root frame is `world`
    /// - declared channel frame IDs become direct children of `world`
    /// - joint records are inferred from joint-like channel bindings
    #[must_use]
    pub fn embodiment_runtime(&self) -> Option<crate::embodiment::EmbodimentRuntime> {
        use std::collections::BTreeSet;

        use crate::embodiment::binding::BindingType;
        use crate::embodiment::frame_tree::{FrameSource, FrameTree, Transform3D};
        use crate::embodiment::limits::JointSafetyLimits;
        use crate::embodiment::model::{EmbodimentModel, Joint, JointType, Link};

        let control_manifest = self.control_interface_manifest()?;

        let mut frame_tree = FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);

        let mut links = vec![Link {
            name: "world".into(),
            parent_joint: None,
            inertial: None,
            visual_geometry: None,
            collision_geometry: None,
        }];
        let mut watched_frames = vec!["world".to_string()];
        let mut seen_frames = BTreeSet::from(["world".to_string()]);

        for channel in &control_manifest.channels {
            if channel.frame_id.is_empty() || !seen_frames.insert(channel.frame_id.clone()) {
                continue;
            }
            let _ = frame_tree.add_frame(
                &channel.frame_id,
                "world",
                Transform3D::identity(),
                FrameSource::Dynamic,
            );
            links.push(Link {
                name: channel.frame_id.clone(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            });
            watched_frames.push(channel.frame_id.clone());
        }

        let joints: Vec<Joint> = control_manifest
            .bindings
            .iter()
            .filter_map(|binding| {
                let joint_type = match binding.binding_type {
                    BindingType::JointPosition => JointType::Revolute,
                    BindingType::JointVelocity => JointType::Continuous,
                    BindingType::Command => JointType::Continuous,
                    BindingType::GripperPosition => JointType::Prismatic,
                    BindingType::GripperForce => JointType::Prismatic,
                    BindingType::ForceTorque
                    | BindingType::ImuOrientation
                    | BindingType::ImuAngularVelocity
                    | BindingType::ImuLinearAcceleration => return None,
                };

                Some(Joint {
                    name: binding.physical_name.clone(),
                    joint_type,
                    parent_link: "world".into(),
                    child_link: if binding.frame_id.is_empty() {
                        "world".into()
                    } else {
                        binding.frame_id.clone()
                    },
                    axis: [0.0, 0.0, 1.0],
                    origin: Transform3D::identity(),
                    limits: JointSafetyLimits {
                        joint_name: binding.physical_name.clone(),
                        max_velocity: f64::INFINITY,
                        max_acceleration: f64::INFINITY,
                        max_jerk: f64::INFINITY,
                        position_min: f64::NEG_INFINITY,
                        position_max: f64::INFINITY,
                        max_torque: None,
                    },
                })
            })
            .collect();

        let model = EmbodimentModel {
            model_id: self.robot.name.clone(),
            model_digest: String::new(),
            embodiment_family: None,
            links,
            joints,
            frame_tree,
            collision_bodies: Vec::new(),
            allowed_collision_pairs: Vec::new(),
            tcps: Vec::new(),
            sensor_mounts: Vec::new(),
            workspace_zones: Vec::new(),
            watched_frames,
            channel_bindings: control_manifest.bindings.clone(),
        };
        let mut runtime = crate::embodiment::EmbodimentRuntime::compile(model, None, None);
        runtime
            .validation_issues
            .push(SYNTHETIC_EMBODIMENT_RUNTIME_ISSUE.into());
        runtime.validation_issues.sort();
        runtime.validation_issues.dedup();
        Some(runtime)
    }

    /// Return an embodiment runtime only when `robot.toml` carries
    /// controller-authoritative embodiment data.
    ///
    /// The current channel-only `robot.toml` shape synthesizes a placeholder
    /// runtime for compatibility helpers, so this method intentionally fails
    /// closed until a real embodiment source is available.
    #[must_use]
    pub fn authoritative_embodiment_runtime(&self) -> Option<crate::embodiment::EmbodimentRuntime> {
        self.embodiment_runtime().filter(|runtime| {
            !runtime
                .validation_issues
                .iter()
                .any(|issue| issue == SYNTHETIC_EMBODIMENT_RUNTIME_ISSUE)
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
    #[allow(deprecated)]
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
name = "joint_position"
type = "position"
unit = "rad"
limits = [-1.0, 1.0]
position_state_index = 0

[[channels.states]]
name = "joint_position"
type = "position"
unit = "rad"
limits = [-1.0, 1.0]
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let ch = manifest.legacy_runtime_manifest().unwrap();
        assert_eq!(ch.robot_id, "test");
        assert_eq!(ch.control_rate_hz, 50);
        assert_eq!(ch.commands.len(), 1);
        assert_eq!(ch.states.len(), 1);
        assert_eq!(ch.commands[0].interface_type, crate::channels::InterfaceType::Position);
        assert_eq!(ch.commands[0].limits, (-1.0, 1.0));
        assert_eq!(ch.commands[0].position_state_index, Some(0));
    }

    #[test]
    #[allow(deprecated)]
    fn channel_manifest_none_when_no_channels_section() {
        let manifest: RobotManifest = toml::from_str(EXAMPLE_TOML).unwrap();
        assert!(manifest.legacy_runtime_manifest().is_none());
    }

    #[test]
    fn control_interface_manifest_from_robot_toml() {
        let toml_str = r#"
[robot]
name = "test"
description = "test"

[channels]
robot_id = "test"
robot_class = "test"
control_rate_hz = 50

[[channels.commands]]
name = "joint_velocity"
type = "velocity"
unit = "rad/s"
frame_id = "shoulder_link"
limits = [-1.0, 1.0]

[[channels.commands]]
name = "joint_effort"
type = "effort"
unit = "Nm"
frame_id = "wrist_link"
limits = [-5.0, 5.0]
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let control = manifest.control_interface_manifest().unwrap();
        assert_eq!(control.version, 1);
        assert_eq!(control.channels.len(), 2);
        assert_eq!(control.bindings.len(), 2);
        assert!(!control.manifest_digest.is_empty());
        assert_eq!(
            control.channels[0].interface_type,
            crate::embodiment::binding::CommandInterfaceType::JointVelocity
        );
        assert_eq!(
            control.bindings[1].binding_type,
            crate::embodiment::binding::BindingType::Command
        );
        assert_eq!(control.channels[0].frame_id, "shoulder_link");
        assert_eq!(control.bindings[1].frame_id, "wrist_link");
    }

    #[test]
    #[allow(deprecated)]
    fn control_interface_manifest_matches_legacy_projection() {
        let toml_str = r#"
[robot]
name = "test"
description = "test"

[channels]
robot_id = "test"
robot_class = "test"
control_rate_hz = 100

[[channels.commands]]
name = "a_position"
type = "position"
unit = "rad"
limits = [-3.14, 3.14]

[[channels.commands]]
name = "b_velocity"
type = "velocity"
unit = "rad/s"
limits = [-2.0, 2.0]
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let channel_manifest = manifest.legacy_runtime_manifest().unwrap();
        let control_manifest = manifest.control_interface_manifest().unwrap();
        assert!(
            channel_manifest
                .legacy_compatibility_issues_with_control_manifest(&control_manifest)
                .is_empty()
        );
    }

    #[test]
    fn embodiment_runtime_from_robot_toml_uses_canonical_channel_frames() {
        let toml_str = r#"
[robot]
name = "test"
description = "test"

[channels]
robot_id = "test"
robot_class = "test"
control_rate_hz = 100

[[channels.commands]]
name = "joint_velocity"
type = "velocity"
unit = "rad/s"
frame_id = "shoulder_link"
limits = [-1.0, 1.0]
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let runtime = manifest.embodiment_runtime().unwrap();
        assert!(runtime.frame_graph.frame_exists("world"));
        assert!(runtime.frame_graph.frame_exists("shoulder_link"));
        assert!(runtime.watched_frames.iter().any(|frame| frame == "shoulder_link"));
        assert_eq!(runtime.model.channel_bindings[0].frame_id, "shoulder_link");
        assert!(
            runtime
                .validation_issues
                .iter()
                .any(|issue| issue.contains("synthesized from robot.toml"))
        );
        assert!(
            manifest.authoritative_embodiment_runtime().is_none(),
            "channel-only robot.toml must not be treated as authoritative embodiment runtime"
        );
    }

    #[test]
    fn control_interface_manifest_none_when_no_channels_section() {
        let manifest: RobotManifest = toml::from_str(EXAMPLE_TOML).unwrap();
        assert!(manifest.control_interface_manifest().is_none());
    }

    #[test]
    #[allow(deprecated)]
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
name = "a_position"
type = "position"
unit = "rad"
limits = [-3.14, 3.14]
max_delta_from = [1, 1.5]

[[channels.commands]]
name = "b_position"
type = "position"
unit = "rad"
limits = [-3.14, 3.14]
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let ch = manifest.legacy_runtime_manifest().unwrap();
        assert_eq!(ch.commands[0].max_delta_from, Some((1, 1.5)));
        assert_eq!(ch.commands[1].max_delta_from, None);
    }

    #[test]
    fn daemon_config_parses() {
        let toml_str = r#"
[robot]
name = "test"
description = "test"

[daemon]
base_url = "http://localhost:8000"

[daemon.get_state]
method = "GET"
path = "/api/state/full"

[daemon.set_motors]
method = "POST"
path = "/api/motors/set_mode/{{mode}}"

[daemon.move_to]
method = "POST"
path = "/api/move/goto"
body = '{"pitch": {{head_pitch}}, "duration": {{duration}}}'

[daemon.play_animation]
method = "POST"
path_prefix = "/api/move/play"
available_moves = ["wake_up", "goto_sleep"]

[daemon.stop_motion]
method = "POST"
path = "/api/motors/set_mode/disabled"
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let daemon = manifest.daemon.unwrap();
        assert_eq!(daemon.base_url, "http://localhost:8000");
        assert!(daemon.move_to.is_some());
        assert_eq!(daemon.play_animation.as_ref().unwrap().available_moves.len(), 2);
        assert_eq!(daemon.get_state.as_ref().unwrap().method, "GET");
    }

    #[test]
    fn daemon_config_optional() {
        let manifest: RobotManifest = toml::from_str(EXAMPLE_TOML).unwrap();
        assert!(manifest.daemon.is_none());
    }

    #[test]
    fn websocket_config_parses() {
        let toml_str = r#"
[robot]
name = "test"
description = "test"

[daemon]
base_url = "http://localhost:8000"

[daemon.websocket]
path = "/ws/sdk"
set_target_type = "set_full_target"
set_target_body = '{"type": "set_full_target", "head": [1,0,0,0]}'
"#;
        let manifest: RobotManifest = toml::from_str(toml_str).unwrap();
        let ws = manifest.daemon.unwrap().websocket.unwrap();
        assert_eq!(ws.path, "/ws/sdk");
        assert_eq!(ws.set_target_type.unwrap(), "set_full_target");
    }

    #[test]
    fn channel_name_validation_warns_on_slash() {
        // Old-style hierarchical names must be rejected
        assert!(!is_valid_channel_name("head/position.x"));
        assert!(!is_valid_channel_name("body/yaw"));
        // New flat snake_case names pass
        assert!(is_valid_channel_name("head_x"));
        assert!(is_valid_channel_name("body_yaw"));
        assert!(is_valid_channel_name("shoulder_pan"));
        // Edge cases
        assert!(!is_valid_channel_name("")); // empty
        assert!(!is_valid_channel_name("0starts_with_number"));
    }

    #[test]
    fn project_manifest_prefers_embodiment_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(LEGACY_ROBOT_MANIFEST_FILE),
            "[robot]\nname = \"legacy\"\ndescription = \"legacy manifest\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(EMBODIMENT_MANIFEST_FILE),
            "[robot]\nname = \"canonical\"\ndescription = \"canonical manifest\"\n",
        )
        .unwrap();

        let (manifest, path) = RobotManifest::load_from_project_dir_with_path(dir.path()).unwrap();
        assert_eq!(manifest.robot.name, "canonical");
        assert_eq!(path, dir.path().join(EMBODIMENT_MANIFEST_FILE));
    }

    #[test]
    fn project_manifest_falls_back_to_robot_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(LEGACY_ROBOT_MANIFEST_FILE),
            "[robot]\nname = \"legacy\"\ndescription = \"legacy manifest\"\n",
        )
        .unwrap();

        let (manifest, path) = RobotManifest::load_from_project_dir_with_path(dir.path()).unwrap();
        assert_eq!(manifest.robot.name, "legacy");
        assert_eq!(path, dir.path().join(LEGACY_ROBOT_MANIFEST_FILE));
    }
}
