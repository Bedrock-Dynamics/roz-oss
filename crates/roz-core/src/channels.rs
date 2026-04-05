//! Robot-agnostic control/state channel descriptions.
//!
//! Following the `ros2_control`/MuJoCo/Drake pattern: named, typed, bounded
//! channels with discovery. Each robot exposes N command channels and M state
//! channels. The WASM controller reads/writes by index; the safety filter
//! clamps per-channel; the actuator sink routes to the native protocol.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// InterfaceType
// ---------------------------------------------------------------------------

/// Type of a control or state interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceType {
    /// Angular or linear position (rad or m).
    Position,
    /// Angular or linear velocity (rad/s or m/s).
    Velocity,
    /// Torque or force (Nm or N).
    Effort,
}

// ---------------------------------------------------------------------------
// ChannelDescriptor
// ---------------------------------------------------------------------------

/// Describes one command or state channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelDescriptor {
    /// Channel name (`ros2_control` convention: `"joint_name/interface_type"`).
    pub name: String,
    /// What this channel represents.
    pub interface_type: InterfaceType,
    /// Physical unit string for documentation.
    pub unit: String,
    /// `(min, max)` value limits.
    pub limits: (f64, f64),
    /// Safe default value (usually `0.0`).
    pub default: f64,
    /// Max rate of change per tick for acceleration/jerk limiting.
    /// `None` = no rate limiting on this channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rate_of_change: Option<f64>,
    /// Index of the corresponding position state channel (for position limit enforcement).
    /// A velocity command channel paired with its position state channel.
    /// `None` = no position limit checking for this channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_state_index: Option<usize>,
    /// Maximum absolute delta between this channel and another command channel.
    /// Used for coupled constraints (e.g., head-body yaw cable limit).
    /// Format: `(other_command_index, max_delta_radians)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delta_from: Option<(usize, f64)>,
}

// ---------------------------------------------------------------------------
// LegacyRuntimeManifest
// ---------------------------------------------------------------------------

/// Compatibility manifest describing a robot's legacy control + state
/// interface.
///
/// Prefer `crate::embodiment::binding::ControlInterfaceManifest` on new
/// control surfaces. This type remains to support older Copper/runtime paths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyRuntimeManifest {
    /// Unique robot identifier.
    pub robot_id: String,
    /// Robot class: `"manipulator"`, `"drone"`, `"mobile"`, `"legged"`.
    pub robot_class: String,
    /// Nominal control loop frequency in Hz.
    pub control_rate_hz: u32,
    /// Command channels (written by the controller each tick).
    pub commands: Vec<ChannelDescriptor>,
    /// State channels (read by the controller each tick).
    pub states: Vec<ChannelDescriptor>,
}

impl LegacyRuntimeManifest {
    /// Number of command channels.
    pub const fn command_count(&self) -> usize {
        self.commands.len()
    }

    /// Number of state channels.
    pub const fn state_count(&self) -> usize {
        self.states.len()
    }

    /// Count of state channels with `InterfaceType::Position`.
    ///
    /// Used by backward-compat `sensor::get_joint_velocity` alias to offset
    /// into the state array past position channels.
    pub fn position_state_count(&self) -> usize {
        self.states
            .iter()
            .filter(|s| s.interface_type == InterfaceType::Position)
            .count()
    }

    /// Generic N-joint velocity-only manifest for backward compatibility.
    ///
    /// Creates `n_joints` velocity command channels with symmetric limits
    /// `(-max_velocity, max_velocity)` and no state channels. Useful for
    /// code that knows `max_velocity` but not the robot type.
    pub fn legacy_velocity_only(n_joints: usize, max_velocity: f64) -> Self {
        let commands = (0..n_joints)
            .map(|i| ChannelDescriptor {
                name: format!("joint{i}/velocity"),
                interface_type: InterfaceType::Velocity,
                unit: "rad/s".into(),
                limits: (-max_velocity, max_velocity),
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: None,
                max_delta_from: None,
            })
            .collect();

        Self {
            robot_id: "generic".into(),
            robot_class: "manipulator".into(),
            control_rate_hz: 100,
            commands,
            states: Vec::new(),
        }
    }

    /// Compatibility-only projection from the spec-level control interface
    /// vocabulary to the legacy runtime manifest consumed by older Copper
    /// entrypoints.
    ///
    /// The newer `ControlInterfaceManifest` does not encode per-channel
    /// limits, defaults, or rate-of-change metadata, so this function
    /// fabricates conservative placeholders where required.
    #[must_use]
    pub(crate) fn legacy_from_control_interface_manifest(
        cim: &crate::embodiment::binding::ControlInterfaceManifest,
    ) -> Self {
        use crate::embodiment::binding::CommandInterfaceType;

        let commands = cim
            .channels
            .iter()
            .map(|ch| {
                let (itype, default_limit) = match ch.interface_type {
                    CommandInterfaceType::JointVelocity => (InterfaceType::Velocity, std::f64::consts::PI),
                    CommandInterfaceType::JointPosition => (InterfaceType::Position, std::f64::consts::PI),
                    CommandInterfaceType::JointTorque | CommandInterfaceType::GripperForce => {
                        (InterfaceType::Effort, 50.0)
                    }
                    CommandInterfaceType::GripperPosition => (InterfaceType::Position, 0.1),
                    CommandInterfaceType::ForceTorqueSensor | CommandInterfaceType::ImuSensor => {
                        (InterfaceType::Position, 1.0)
                    }
                };
                ChannelDescriptor {
                    name: ch.name.clone(),
                    interface_type: itype,
                    unit: ch.units.clone(),
                    limits: (-default_limit, default_limit),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                }
            })
            .collect();

        Self {
            robot_id: String::new(),
            robot_class: String::new(),
            control_rate_hz: 100,
            commands,
            states: Vec::new(),
        }
    }

    /// Compatibility-only lossy projection retained for internal callers.
    #[doc(hidden)]
    #[deprecated(note = "compatibility-only helper; prefer ControlInterfaceManifest as the canonical contract")]
    #[must_use]
    pub fn from_control_interface_manifest_lossy(cim: &crate::embodiment::binding::ControlInterfaceManifest) -> Self {
        Self::legacy_from_control_interface_manifest(cim)
    }

    /// Report semantic drift between a legacy runtime manifest and a
    /// spec-level `ControlInterfaceManifest` that should describe the same
    /// control interface.
    #[must_use]
    pub(crate) fn legacy_compatibility_issues_with_control_manifest(
        &self,
        cim: &crate::embodiment::binding::ControlInterfaceManifest,
    ) -> Vec<String> {
        use crate::embodiment::binding::CommandInterfaceType;

        let mut issues = Vec::new();
        if self.commands.len() != cim.channels.len() {
            issues.push(format!(
                "command count mismatch: legacy={} spec={}",
                self.commands.len(),
                cim.channels.len()
            ));
        }

        for (index, (legacy, spec)) in self.commands.iter().zip(&cim.channels).enumerate() {
            if legacy.name != spec.name {
                issues.push(format!(
                    "channel {index} name mismatch: legacy={} spec={}",
                    legacy.name, spec.name
                ));
            }
            let expected = match spec.interface_type {
                CommandInterfaceType::JointVelocity => InterfaceType::Velocity,
                CommandInterfaceType::JointPosition => InterfaceType::Position,
                CommandInterfaceType::JointTorque | CommandInterfaceType::GripperForce => InterfaceType::Effort,
                CommandInterfaceType::GripperPosition => InterfaceType::Position,
                CommandInterfaceType::ForceTorqueSensor | CommandInterfaceType::ImuSensor => InterfaceType::Position,
            };
            if legacy.interface_type != expected {
                issues.push(format!(
                    "channel {index} interface mismatch: legacy={:?} spec={:?}",
                    legacy.interface_type, spec.interface_type
                ));
            }
            if legacy.unit != spec.units {
                issues.push(format!(
                    "channel {index} unit mismatch: legacy={} spec={}",
                    legacy.unit, spec.units
                ));
            }
        }

        issues
    }

    /// Compatibility-only drift reporter retained for internal callers.
    #[doc(hidden)]
    #[deprecated(note = "compatibility-only helper; prefer explicit legacy projection checks within roz-core")]
    #[must_use]
    pub fn compatibility_issues_with_control_manifest(
        &self,
        cim: &crate::embodiment::binding::ControlInterfaceManifest,
    ) -> Vec<String> {
        self.legacy_compatibility_issues_with_control_manifest(cim)
    }

    /// Whether this legacy manifest matches the command metadata declared by
    /// the spec-level control interface manifest.
    #[must_use]
    pub(crate) fn is_compatible_with_control_manifest_compat(
        &self,
        cim: &crate::embodiment::binding::ControlInterfaceManifest,
    ) -> bool {
        self.legacy_compatibility_issues_with_control_manifest(cim).is_empty()
    }

    /// Compatibility-only equivalence check retained for internal callers.
    #[doc(hidden)]
    #[deprecated(note = "compatibility-only helper; prefer explicit legacy projection checks within roz-core")]
    #[must_use]
    pub fn is_compatible_with_control_manifest(
        &self,
        cim: &crate::embodiment::binding::ControlInterfaceManifest,
    ) -> bool {
        self.is_compatible_with_control_manifest_compat(cim)
    }
}

impl Default for LegacyRuntimeManifest {
    /// Empty manifest with no channels. Suitable for modules that do not
    /// import any channel host functions.
    fn default() -> Self {
        Self {
            robot_id: String::new(),
            robot_class: String::new(),
            control_rate_hz: 100,
            commands: Vec::new(),
            states: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion from spec-level ControlInterfaceManifest
// ---------------------------------------------------------------------------

#[doc(hidden)]
impl From<&crate::embodiment::binding::ControlInterfaceManifest> for LegacyRuntimeManifest {
    /// Compatibility-only lossy projection from a spec-level
    /// `ControlInterfaceManifest` into the legacy runtime manifest.
    ///
    /// Each `ControlChannelDef` becomes a command `ChannelDescriptor`. The
    /// conversion uses conservative defaults for limits and rate-of-change
    /// since `ControlInterfaceManifest` does not carry per-channel limits.
    fn from(cim: &crate::embodiment::binding::ControlInterfaceManifest) -> Self {
        Self::legacy_from_control_interface_manifest(cim)
    }
}

#[doc(hidden)]
impl From<&LegacyRuntimeManifest> for crate::embodiment::binding::ControlInterfaceManifest {
    /// Compatibility-only projection from a legacy runtime manifest back into
    /// the spec-level `ControlInterfaceManifest`.
    ///
    /// This keeps worker and tool surfaces on the newer manifest vocabulary even
    /// while Copper still consumes the legacy runtime manifest internally.
    fn from(manifest: &LegacyRuntimeManifest) -> Self {
        use crate::embodiment::binding::{
            BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
        };

        let channels: Vec<ControlChannelDef> = manifest
            .commands
            .iter()
            .map(|ch| ControlChannelDef {
                name: ch.name.clone(),
                interface_type: match ch.interface_type {
                    InterfaceType::Position => CommandInterfaceType::JointPosition,
                    InterfaceType::Velocity => CommandInterfaceType::JointVelocity,
                    InterfaceType::Effort => CommandInterfaceType::JointTorque,
                },
                units: ch.unit.clone(),
                frame_id: String::new(),
            })
            .collect();

        let bindings: Vec<ChannelBinding> = manifest
            .commands
            .iter()
            .enumerate()
            .map(|(i, ch)| ChannelBinding {
                physical_name: ch.name.clone(),
                channel_index: i as u32,
                binding_type: match ch.interface_type {
                    InterfaceType::Position => BindingType::JointPosition,
                    InterfaceType::Velocity => BindingType::JointVelocity,
                    InterfaceType::Effort => BindingType::Command,
                },
                frame_id: String::new(),
                units: ch.unit.clone(),
                semantic_role: None,
            })
            .collect();

        let mut cim = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels,
            bindings,
        };
        cim.stamp_digest();
        cim
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    #[test]
    fn manifest_serde_roundtrip() {
        let manifest = LegacyRuntimeManifest::legacy_velocity_only(6, PI);
        let json = serde_json::to_string(&manifest).expect("serialization must succeed");
        let restored: LegacyRuntimeManifest = serde_json::from_str(&json).expect("deserialization must succeed");

        assert_eq!(restored.robot_id, manifest.robot_id);
        assert_eq!(restored.robot_class, manifest.robot_class);
        assert_eq!(restored.control_rate_hz, manifest.control_rate_hz);
        assert_eq!(restored.commands.len(), manifest.commands.len());
        assert_eq!(restored.states.len(), manifest.states.len());

        // Spot-check a command channel survives the round-trip.
        assert_eq!(restored.commands[0].name, manifest.commands[0].name);
        assert_eq!(restored.commands[0].interface_type, manifest.commands[0].interface_type);
        assert_eq!(restored.commands[0].limits, manifest.commands[0].limits);
        assert_eq!(
            restored.commands[0].position_state_index,
            manifest.commands[0].position_state_index
        );
    }

    #[test]
    fn generic_velocity_has_correct_channels() {
        let m = LegacyRuntimeManifest::legacy_velocity_only(6, PI);

        assert_eq!(
            m.command_count(),
            6,
            "legacy_velocity_only(6) must have 6 command channels"
        );
        assert_eq!(m.state_count(), 0, "generic_velocity has no state channels");

        for (i, cmd) in m.commands.iter().enumerate() {
            assert_eq!(
                cmd.interface_type,
                InterfaceType::Velocity,
                "command {i} must be Velocity"
            );
            assert_eq!(cmd.limits, (-PI, PI), "command {i} must have symmetric PI limits");
        }

        assert_eq!(m.robot_class, "manipulator");
        assert_eq!(m.control_rate_hz, 100);
    }

    #[test]
    fn default_manifest_is_empty() {
        let m = LegacyRuntimeManifest::default();
        assert_eq!(m.command_count(), 0);
        assert_eq!(m.state_count(), 0);
        assert_eq!(m.control_rate_hz, 100);
    }

    #[test]
    fn from_control_interface_manifest() {
        use crate::embodiment::binding::{CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest};

        let cim = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![
                ControlChannelDef {
                    name: "shoulder_vel".into(),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: "base_link".into(),
                },
                ControlChannelDef {
                    name: "gripper_pos".into(),
                    interface_type: CommandInterfaceType::GripperPosition,
                    units: "m".into(),
                    frame_id: "wrist_link".into(),
                },
            ],
            bindings: vec![],
        };
        let manifest = LegacyRuntimeManifest::from(&cim);
        assert_eq!(manifest.command_count(), 2);
        assert_eq!(manifest.commands[0].name, "shoulder_vel");
        assert_eq!(manifest.commands[0].interface_type, InterfaceType::Velocity);
        assert_eq!(manifest.commands[1].name, "gripper_pos");
        assert_eq!(manifest.commands[1].interface_type, InterfaceType::Position);
        assert_eq!(manifest.control_rate_hz, 100);
    }

    #[test]
    fn position_state_count_filters_correctly() {
        let m = LegacyRuntimeManifest::legacy_velocity_only(4, 1.0);
        // generic_velocity has no state channels at all.
        assert_eq!(m.position_state_count(), 0);
    }

    #[test]
    fn into_control_interface_manifest_roundtrips_command_names() {
        use crate::embodiment::binding::{BindingType, CommandInterfaceType, ControlInterfaceManifest};

        let manifest = LegacyRuntimeManifest::legacy_velocity_only(2, PI);
        let cim = ControlInterfaceManifest::from(&manifest);

        assert_eq!(cim.version, 1);
        assert_eq!(cim.channels.len(), 2);
        assert_eq!(cim.bindings.len(), 2);
        assert!(!cim.manifest_digest.is_empty());
        assert_eq!(cim.channels[0].name, "joint0/velocity");
        assert_eq!(cim.channels[0].interface_type, CommandInterfaceType::JointVelocity);
        assert_eq!(cim.bindings[0].binding_type, BindingType::JointVelocity);
    }

    #[test]
    fn compatibility_check_accepts_equivalent_control_manifest() {
        let manifest = LegacyRuntimeManifest::legacy_velocity_only(3, PI);
        let cim = crate::embodiment::binding::ControlInterfaceManifest::from(&manifest);
        assert!(manifest.is_compatible_with_control_manifest_compat(&cim));
        assert!(
            manifest
                .legacy_compatibility_issues_with_control_manifest(&cim)
                .is_empty()
        );
    }

    #[test]
    fn compatibility_check_reports_drift() {
        let manifest = LegacyRuntimeManifest::legacy_velocity_only(2, PI);
        let mut cim = crate::embodiment::binding::ControlInterfaceManifest::from(&manifest);
        cim.channels[0].name = "different".into();
        let issues = manifest.legacy_compatibility_issues_with_control_manifest(&cim);
        assert!(!issues.is_empty());
        assert!(issues.iter().any(|issue| issue.contains("name mismatch")));
    }
}
