//! Compiled runtime combining base model, calibration, and safety overlays.
//!
//! `EmbodimentRuntime` is the authoritative, fully-resolved representation
//! of a robot's physical configuration used at runtime. It is compiled from
//! the base `EmbodimentModel`, an optional `CalibrationOverlay`, and an
//! optional `SafetyOverlay`. The combined digest ensures that any change
//! in any layer invalidates cached artefacts (controllers, evidence bundles).

use std::collections::{BTreeMap, BTreeSet};

use nalgebra::{DMatrix, DVector, Quaternion, UnitQuaternion, Vector3};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::calibration::CalibrationOverlay;
use super::frame_snapshot::{FrameGraphSnapshot, FrameSnapshotInput, TimestampedTransform, WorldAnchor};
use super::frame_tree::{FrameSource, FrameTree, Transform3D};
use super::model::{CollisionBody, EmbodimentModel, Geometry, Joint, JointType, ToolCenterPoint};
use super::perception::{ActivePerceptionCommand, ObservationGoal, ViewpointTarget};
use super::safety_overlay::SafetyOverlay;
use super::workspace::WorkspaceEnvelope;
use crate::clock::ClockDomain;
use crate::session::snapshot::FreshnessState;

/// Fully-resolved embodiment configuration for runtime use.
///
/// Produced by `compile()` from the three layers. The `combined_digest`
/// is a SHA-256 over the three layer digests, providing a single cache key
/// for the entire physical configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbodimentRuntime {
    /// The base physical model.
    pub model: EmbodimentModel,
    /// Optional calibration corrections applied on top of the base model.
    pub calibration: Option<CalibrationOverlay>,
    /// Optional safety constraints for the current deployment.
    pub safety_overlay: Option<SafetyOverlay>,
    /// Digest of the active embodiment model.
    #[serde(default)]
    pub model_digest: String,
    /// Digest of the active calibration layer currently bound into this runtime.
    #[serde(default)]
    pub calibration_digest: String,
    /// Digest of the active safety overlay currently bound into this runtime.
    #[serde(default)]
    pub safety_digest: String,
    /// SHA-256 over `(model_digest, calibration_digest, overlay_digest)`.
    pub combined_digest: String,
    /// Compiled frame graph with any valid calibration corrections applied.
    #[serde(default)]
    pub frame_graph: FrameTree,
    /// Active calibration identity, if a valid calibration overlay was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_calibration_id: Option<String>,
    /// Number of joints compiled into the runtime.
    #[serde(default)]
    pub joint_count: usize,
    /// Number of TCPs compiled into the runtime.
    #[serde(default)]
    pub tcp_count: usize,
    /// Bounded frame subset surfaces can project into tick input or replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched_frames: Vec<String>,
    /// Validation or degradation notes captured during runtime compilation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_issues: Vec<String>,
}

/// Runtime-owned projection of a watched frame into controller-facing pose data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchedPoseProjection {
    pub frame_id: String,
    pub relative_to: String,
    pub transform: Transform3D,
    pub freshness: FreshnessState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedFramePose {
    pub frame_id: String,
    pub relative_to: String,
    pub transform: Transform3D,
    pub freshness: FreshnessState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceFrameMatch {
    pub zone_name: String,
    pub zone_type: super::workspace::ZoneType,
    pub contains_origin: bool,
    pub point_in_zone: [f64; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceFrameEvaluation {
    pub pose: ResolvedFramePose,
    pub matches: Vec<WorkspaceFrameMatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspacePoseEvaluation {
    pub relative_to: String,
    pub pose: Transform3D,
    pub safe: bool,
    pub min_margin_m: Option<f64>,
    pub in_human_presence: bool,
    pub zone_checks: Vec<WorkspaceZoneCheck>,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceZoneCheck {
    pub zone_name: String,
    pub zone_type: super::workspace::ZoneType,
    pub signed_margin_m: f64,
    pub contains_origin: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceCheckFrameResult {
    pub frame_id: String,
    pub margin_m: Option<f64>,
    pub safe: bool,
    pub in_human_presence: bool,
    pub zone_checks: Vec<WorkspaceZoneCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceCheckResult {
    pub safe: bool,
    pub min_margin_m: Option<f64>,
    pub frames: Vec<WorkspaceCheckFrameResult>,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickDerivedFeaturesProjection {
    pub calibration_valid: bool,
    pub workspace_margin: Option<f64>,
    pub collision_margin: Option<f64>,
    pub force_margin: Option<f64>,
    pub observation_confidence: Option<f64>,
    pub active_perception_available: bool,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickProjection {
    pub watched_poses: Vec<WatchedPoseProjection>,
    pub features: TickDerivedFeaturesProjection,
    pub validation_issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickJointStateProjection {
    pub name: String,
    pub position: f64,
    pub velocity: f64,
    pub effort: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickInputProjection {
    pub tick: u64,
    pub monotonic_time_ns: u64,
    pub snapshot: FrameGraphSnapshot,
    pub joints: Vec<TickJointStateProjection>,
    pub watched_poses: Vec<WatchedPoseProjection>,
    pub features: TickDerivedFeaturesProjection,
    pub validation_issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SensorReachability {
    pub sensor_id: String,
    pub actuated: bool,
    pub has_frustum: bool,
    pub joint_chain: Vec<String>,
    pub pose: ResolvedFramePose,
    pub workspace_safe: bool,
    pub min_margin_m: Option<f64>,
    pub in_human_presence: bool,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivePerceptionCommandCheck {
    pub sensor: SensorReachability,
    pub target_valid: bool,
    pub planned_pose: Option<WorkspacePoseEvaluation>,
    pub execution: PlanExecutionAssessment,
    pub safe: bool,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcpReachabilityEvaluation {
    pub tcp_name: String,
    pub current_pose: ResolvedFramePose,
    pub target_pose: ResolvedFramePose,
    pub workspace: WorkspacePoseEvaluation,
    pub translation_error_m: f64,
    pub orientation_error_rad: f64,
    pub jacobian_rank: usize,
    pub min_singular_value: Option<f64>,
    pub manipulability: Option<f64>,
    pub conditioning: JacobianConditioning,
    pub execution: PlanExecutionAssessment,
    pub well_conditioned: bool,
    pub safe: bool,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcpMotionPlan {
    pub tcp_name: String,
    pub current_pose: ResolvedFramePose,
    pub target_pose: ResolvedFramePose,
    pub projected_pose: ResolvedFramePose,
    pub proposed_joint_positions: Vec<f64>,
    pub target_workspace: WorkspacePoseEvaluation,
    pub projected_workspace: WorkspacePoseEvaluation,
    pub remaining_translation_error_m: f64,
    pub remaining_orientation_error_rad: f64,
    pub jacobian_rank: usize,
    pub min_singular_value: Option<f64>,
    pub manipulability: Option<f64>,
    pub conditioning: JacobianConditioning,
    pub execution: PlanExecutionAssessment,
    pub safe: bool,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcpIkSolution {
    pub tcp_name: String,
    pub converged: bool,
    pub iterations: usize,
    pub final_joint_positions: Vec<f64>,
    pub final_pose: ResolvedFramePose,
    pub final_workspace: WorkspacePoseEvaluation,
    pub remaining_translation_error_m: f64,
    pub remaining_orientation_error_rad: f64,
    pub conditioning: JacobianConditioning,
    pub execution: PlanExecutionAssessment,
    pub safe: bool,
    pub alerts: Vec<String>,
    pub steps: Vec<TcpMotionPlan>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TcpTrajectoryPlan {
    pub tcp_name: String,
    pub relative_to: String,
    pub converged: bool,
    pub safe: bool,
    pub execution: PlanExecutionAssessment,
    pub quality: TrajectoryQualitySummary,
    pub waypoint_summaries: Vec<TrajectoryWaypointSummary>,
    pub joint_trajectory: Vec<PlannedJointTrajectorySample>,
    pub waypoint_solutions: Vec<TcpIkSolution>,
    pub final_joint_positions: Vec<f64>,
    pub final_pose: ResolvedFramePose,
    pub final_workspace: WorkspacePoseEvaluation,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum JacobianConditioning {
    Singular,
    IllConditioned,
    Marginal,
    WellConditioned,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PlanDegradationReason {
    OutsideAllowedWorkspace,
    RestrictedWorkspace,
    HumanPresenceZone,
    SingularJacobian,
    IllConditionedJacobian,
    NonConverged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanExecutionAssessment {
    pub executable: bool,
    pub safe: bool,
    pub conditioning: JacobianConditioning,
    pub min_workspace_margin_m: Option<f64>,
    pub degradation_reasons: Vec<PlanDegradationReason>,
    pub alerts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryWaypointSummary {
    pub waypoint_index: usize,
    pub converged: bool,
    pub safe: bool,
    pub execution: PlanExecutionAssessment,
    pub remaining_translation_error_m: f64,
    pub remaining_orientation_error_rad: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedJointTrajectorySample {
    pub waypoint_index: usize,
    pub joint_positions: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryQualitySummary {
    pub total_waypoints: usize,
    pub converged_waypoints: usize,
    pub min_workspace_margin_m: Option<f64>,
    pub worst_conditioning: JacobianConditioning,
    pub max_remaining_translation_error_m: f64,
    pub max_remaining_orientation_error_rad: f64,
}

#[derive(Debug)]
struct CompiledFrameGraph {
    frame_graph: FrameTree,
    validation_issues: Vec<String>,
}

impl EmbodimentRuntime {
    fn geometry_bounding_radius(geometry: &Geometry) -> Option<f64> {
        match geometry {
            Geometry::Box { half_extents } => Some(
                half_extents[0]
                    .mul_add(
                        half_extents[0],
                        half_extents[1].mul_add(half_extents[1], half_extents[2] * half_extents[2]),
                    )
                    .sqrt(),
            ),
            Geometry::Sphere { radius } => Some(*radius),
            Geometry::Cylinder { radius, length } => Some(radius.hypot(length * 0.5)),
            Geometry::Mesh { .. } => None,
        }
    }

    fn collision_body_pose(&self, snapshot: &FrameGraphSnapshot, body: &CollisionBody) -> Result<Transform3D, String> {
        let link_pose = self.resolve_frame_pose(snapshot, &body.link_name, None)?;
        Ok(link_pose.transform.compose(&body.origin))
    }

    fn collision_margin_for_snapshot(&self, snapshot: &FrameGraphSnapshot) -> (Option<f64>, Vec<String>) {
        if self.model.collision_bodies.len() < 2 {
            return (None, Vec::new());
        }

        let mut issues = Vec::new();
        let mut bodies = Vec::new();
        for body in &self.model.collision_bodies {
            let Some(radius) = Self::geometry_bounding_radius(&body.geometry) else {
                issues.push(format!(
                    "collision body `{}` uses unsupported geometry for collision-margin projection",
                    body.link_name
                ));
                continue;
            };
            match self.collision_body_pose(snapshot, body) {
                Ok(pose) => bodies.push((body.link_name.clone(), pose.translation, radius)),
                Err(error) => issues.push(format!(
                    "failed to resolve collision body `{}` for collision-margin projection: {error}",
                    body.link_name
                )),
            }
        }

        let mut min_margin = None;
        for (index, (left_link, left_center, left_radius)) in bodies.iter().enumerate() {
            for (right_link, right_center, right_radius) in bodies.iter().skip(index + 1) {
                if left_link == right_link || self.model.is_allowed_collision_pair(left_link, right_link) {
                    continue;
                }
                let dx = left_center[0] - right_center[0];
                let dy = left_center[1] - right_center[1];
                let dz = left_center[2] - right_center[2];
                let center_distance = dx.mul_add(dx, dy.mul_add(dy, dz * dz)).sqrt();
                let margin = center_distance - (left_radius + right_radius);
                min_margin = Some(min_margin.map_or(margin, |current: f64| current.min(margin)));
            }
        }

        issues.sort();
        issues.dedup();
        (min_margin, issues)
    }

    fn joint_effort_limit(&self, joint_name: &str) -> Option<f64> {
        self.safety_overlay
            .as_ref()
            .and_then(|overlay| overlay.joint_limit_overrides.get(joint_name))
            .and_then(|limits| limits.max_torque)
            .or_else(|| {
                self.model
                    .get_joint(joint_name)
                    .and_then(|joint| joint.limits.max_torque)
            })
    }

    fn force_margin_from_efforts(&self, efforts: Option<&[f64]>) -> Option<f64> {
        let efforts = efforts?;
        self.model
            .channel_bindings
            .iter()
            .filter_map(|binding| {
                let index = usize::try_from(binding.channel_index).ok()?;
                let effort = efforts.get(index).copied()?;
                let limit = self.joint_effort_limit(&binding.physical_name)?;
                Some(limit - effort.abs())
            })
            .fold(None, |acc: Option<f64>, margin| {
                Some(acc.map_or(margin, |current| current.min(margin)))
            })
    }

    fn observation_confidence_for_snapshot(&self, snapshot: &FrameGraphSnapshot) -> Option<f64> {
        let watched_frames = if snapshot.watched_frames.is_empty() {
            &self.watched_frames
        } else {
            &snapshot.watched_frames
        };
        let mut samples = Vec::new();
        for frame_id in watched_frames {
            let score = match snapshot
                .frame_freshness
                .get(frame_id)
                .unwrap_or(&FreshnessState::Unknown)
            {
                FreshnessState::Fresh => 1.0,
                FreshnessState::Stale { .. } => 0.5,
                FreshnessState::Unknown => 0.0,
            };
            samples.push(score);
        }
        samples.extend(
            snapshot
                .world_anchors
                .iter()
                .map(|anchor| anchor.confidence.clamp(0.0, 1.0)),
        );
        if samples.is_empty() {
            return None;
        }
        let sample_count = f64::from(u32::try_from(samples.len()).ok()?);
        Some(samples.iter().sum::<f64>() / sample_count)
    }

    fn link_frame_for_frame(&self, frame_id: &str) -> Option<String> {
        let mut current = Some(frame_id.to_string());
        let mut visited = BTreeSet::new();
        while let Some(frame_id) = current {
            if !visited.insert(frame_id.clone()) {
                return None;
            }
            if self.model.get_link(&frame_id).is_some() {
                return Some(frame_id);
            }
            current = self
                .frame_graph
                .get_frame(&frame_id)
                .and_then(|node| node.parent_id.clone());
        }
        None
    }

    fn normalized_axis(axis: [f64; 3]) -> Option<[f64; 3]> {
        let norm_sq = axis[0].mul_add(axis[0], axis[1].mul_add(axis[1], axis[2] * axis[2]));
        if norm_sq <= f64::EPSILON {
            return None;
        }
        let norm = norm_sq.sqrt();
        Some([axis[0] / norm, axis[1] / norm, axis[2] / norm])
    }

    fn rotation_from_axis_angle(axis: [f64; 3], angle_rad: f64) -> [f64; 4] {
        let half_angle = angle_rad * 0.5;
        let sin_half = half_angle.sin();
        [
            half_angle.cos(),
            axis[0] * sin_half,
            axis[1] * sin_half,
            axis[2] * sin_half,
        ]
    }

    fn joint_position_transform(joint: &Joint, position: f64, timestamp_ns: u64) -> Result<Transform3D, String> {
        let variable = match joint.joint_type {
            JointType::Revolute | JointType::Continuous => {
                let Some(axis) = Self::normalized_axis(joint.axis) else {
                    return Err(format!("joint `{}` has invalid zero-length axis", joint.name));
                };
                Transform3D {
                    translation: [0.0, 0.0, 0.0],
                    rotation: Self::rotation_from_axis_angle(axis, position),
                    timestamp_ns,
                }
            }
            JointType::Prismatic => {
                let Some(axis) = Self::normalized_axis(joint.axis) else {
                    return Err(format!("joint `{}` has invalid zero-length axis", joint.name));
                };
                Transform3D {
                    translation: [axis[0] * position, axis[1] * position, axis[2] * position],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns,
                }
            }
            JointType::Fixed => Transform3D::identity(),
        };
        Ok(joint.origin.compose(&variable))
    }

    fn joint_state_transforms(
        &self,
        joint_positions: &BTreeMap<String, f64>,
        timestamp_ns: u64,
    ) -> (Vec<TimestampedTransform>, Vec<String>) {
        let mut transforms = Vec::new();
        let mut validation_issues = Vec::new();

        for (joint_name, position) in joint_positions {
            let Some(joint) = self.model.get_joint(joint_name) else {
                validation_issues.push(format!("joint position input references missing joint `{joint_name}`"));
                continue;
            };
            if !self.frame_graph.frame_exists(&joint.parent_link) {
                validation_issues.push(format!(
                    "joint `{}` references missing parent link frame `{}`",
                    joint.name, joint.parent_link
                ));
                continue;
            }
            if !self.frame_graph.frame_exists(&joint.child_link) {
                validation_issues.push(format!(
                    "joint `{}` references missing child link frame `{}`",
                    joint.name, joint.child_link
                ));
                continue;
            }
            match Self::joint_position_transform(joint, *position, timestamp_ns) {
                Ok(transform) => transforms.push(TimestampedTransform {
                    frame_id: joint.child_link.clone(),
                    parent_id: Some(joint.parent_link.clone()),
                    transform,
                    freshness: if timestamp_ns == 0 {
                        FreshnessState::Unknown
                    } else {
                        FreshnessState::Fresh
                    },
                    source: FrameSource::Computed,
                }),
                Err(error) => validation_issues.push(error),
            }
        }

        validation_issues.sort();
        validation_issues.dedup();
        (transforms, validation_issues)
    }

    fn overlay_snapshot_tree(snapshot: &FrameGraphSnapshot) -> (FrameTree, Vec<String>) {
        let mut projection_tree = snapshot.frame_tree.clone();
        let mut validation_issues = Vec::new();
        for transform in &snapshot.dynamic_transforms {
            if let Err(error) = projection_tree.overlay_transform(
                &transform.frame_id,
                transform.transform.clone(),
                Some(transform.source.clone()),
            ) {
                validation_issues.push(format!(
                    "failed to overlay dynamic transform for `{}` during snapshot projection: {error}",
                    transform.frame_id
                ));
            }
        }
        validation_issues.sort();
        validation_issues.dedup();
        (projection_tree, validation_issues)
    }

    fn compiled_frame_graph(model: &EmbodimentModel, calibration: Option<&CalibrationOverlay>) -> CompiledFrameGraph {
        let source_tree = &model.frame_tree;
        let Some(root_id) = source_tree.root() else {
            return CompiledFrameGraph {
                frame_graph: FrameTree::new(),
                validation_issues: vec!["frame tree missing root frame".into()],
            };
        };
        let Some(root_node) = source_tree.get_frame(root_id) else {
            return CompiledFrameGraph {
                frame_graph: FrameTree::new(),
                validation_issues: vec![format!("frame tree root `{root_id}` missing node definition")],
            };
        };

        let mut compiled = FrameTree::new();
        compiled.set_root(root_id, root_node.source.clone());
        let mut validation_issues = Vec::new();

        let mut pending: BTreeSet<String> = source_tree
            .all_frame_ids()
            .into_iter()
            .filter(|frame_id| *frame_id != root_id)
            .map(str::to_string)
            .collect();

        while !pending.is_empty() {
            let mut progressed = false;
            for frame_id in pending.clone() {
                let Some(node) = source_tree.get_frame(&frame_id) else {
                    continue;
                };
                let Some(parent_id) = node.parent_id.as_deref() else {
                    continue;
                };
                if !compiled.frame_exists(parent_id) {
                    continue;
                }
                let transform = calibration
                    .and_then(|overlay| overlay.frame_corrections.get(&frame_id))
                    .cloned()
                    .unwrap_or_else(|| node.static_transform.clone());
                if compiled
                    .add_frame(&frame_id, parent_id, transform, node.source.clone())
                    .is_ok()
                {
                    pending.remove(&frame_id);
                    progressed = true;
                }
            }
            if !progressed {
                validation_issues.push(format!(
                    "frame graph compilation fell back to source tree for unresolved frames: {}",
                    pending.into_iter().collect::<Vec<_>>().join(", ")
                ));
                return CompiledFrameGraph {
                    frame_graph: source_tree.clone(),
                    validation_issues,
                };
            }
        }

        CompiledFrameGraph {
            frame_graph: compiled,
            validation_issues,
        }
    }

    fn infer_watched_frames(
        model: &EmbodimentModel,
        calibration: Option<&CalibrationOverlay>,
        safety_overlay: Option<&SafetyOverlay>,
    ) -> Vec<String> {
        let mut watched = BTreeSet::new();
        if let Some(root) = model.frame_tree.root() {
            watched.insert(root.to_string());
        }
        for tcp in &model.tcps {
            if model.frame_tree.frame_exists(&tcp.parent_link) {
                watched.insert(tcp.parent_link.clone());
            }
        }
        for sensor in &model.sensor_mounts {
            if model.frame_tree.frame_exists(&sensor.parent_link) {
                watched.insert(sensor.parent_link.clone());
            }
        }
        for zone in &model.workspace_zones {
            if model.frame_tree.frame_exists(&zone.origin_frame) {
                watched.insert(zone.origin_frame.clone());
            }
        }
        for binding in &model.channel_bindings {
            if model.frame_tree.frame_exists(&binding.frame_id) {
                watched.insert(binding.frame_id.clone());
            }
        }
        if let Some(calibration) = calibration {
            for frame_id in calibration.frame_corrections.keys() {
                if model.frame_tree.frame_exists(frame_id) {
                    watched.insert(frame_id.clone());
                }
            }
        }
        if let Some(safety_overlay) = safety_overlay {
            for zone in safety_overlay
                .workspace_restrictions
                .iter()
                .chain(&safety_overlay.human_presence_zones)
                .chain(&safety_overlay.contact_allowed_zones)
            {
                if model.frame_tree.frame_exists(&zone.origin_frame) {
                    watched.insert(zone.origin_frame.clone());
                }
            }
        }

        watched.into_iter().collect()
    }

    fn resolve_watched_frames(
        model: &EmbodimentModel,
        calibration: Option<&CalibrationOverlay>,
        safety_overlay: Option<&SafetyOverlay>,
        frame_graph: &FrameTree,
    ) -> (Vec<String>, Vec<String>) {
        let mut validation_issues = Vec::new();
        if !model.watched_frames.is_empty() {
            let mut watched = Vec::new();
            for frame_id in &model.watched_frames {
                if frame_graph.frame_exists(frame_id) {
                    watched.push(frame_id.clone());
                } else {
                    validation_issues.push(format!("watched frame `{frame_id}` missing from compiled frame graph"));
                }
            }
            watched.sort();
            watched.dedup();
            return (watched, validation_issues);
        }

        let watched = Self::infer_watched_frames(model, calibration, safety_overlay);
        if !watched.is_empty() {
            validation_issues.push("watched frames inferred from legacy model fields".into());
        }
        (watched, validation_issues)
    }

    #[must_use]
    pub const fn uses_legacy_watched_frame_inference(&self) -> bool {
        !self.model.has_declared_watched_frames() && !self.watched_frames.is_empty()
    }

    fn push_missing_frame_issue(issues: &mut Vec<String>, subject: &str, frame_id: &str, frame_graph: &FrameTree) {
        if !frame_graph.frame_exists(frame_id) {
            issues.push(format!("{subject} references missing frame `{frame_id}`"));
        }
    }

    fn collect_validation_issues(
        model: &EmbodimentModel,
        calibration: Option<&CalibrationOverlay>,
        calibration_applied: bool,
        safety_overlay: Option<&SafetyOverlay>,
        frame_graph: &FrameTree,
    ) -> Vec<String> {
        let mut issues = Vec::new();

        if model.frame_tree.root().is_none() {
            issues.push("model frame tree has no root".into());
        }

        for tcp in &model.tcps {
            Self::push_missing_frame_issue(
                &mut issues,
                &format!("tcp `{}` parent link", tcp.name),
                &tcp.parent_link,
                frame_graph,
            );
        }
        for sensor in &model.sensor_mounts {
            Self::push_missing_frame_issue(
                &mut issues,
                &format!("sensor `{}` parent link", sensor.sensor_id),
                &sensor.parent_link,
                frame_graph,
            );
        }
        for zone in &model.workspace_zones {
            Self::push_missing_frame_issue(
                &mut issues,
                &format!("workspace zone `{}`", zone.name),
                &zone.origin_frame,
                frame_graph,
            );
        }
        for binding in &model.channel_bindings {
            Self::push_missing_frame_issue(
                &mut issues,
                &format!("channel binding `{}`", binding.physical_name),
                &binding.frame_id,
                frame_graph,
            );
        }

        if let Some(calibration) = calibration {
            if !calibration_applied {
                issues.push(format!(
                    "calibration `{}` was not applied because it is not valid for model digest `{}`",
                    calibration.calibration_id, model.model_digest
                ));
            }
            for frame_id in calibration.frame_corrections.keys() {
                Self::push_missing_frame_issue(
                    &mut issues,
                    &format!("calibration `{}` correction", calibration.calibration_id),
                    frame_id,
                    frame_graph,
                );
            }
        }

        if let Some(safety_overlay) = safety_overlay {
            for joint_name in safety_overlay.joint_limit_overrides.keys() {
                if model.get_joint(joint_name).is_none() {
                    issues.push(format!(
                        "safety overlay joint override references missing joint `{joint_name}`"
                    ));
                }
            }
            for envelope in &safety_overlay.contact_force_envelopes {
                if model.get_link(&envelope.link_name).is_none() {
                    issues.push(format!(
                        "safety overlay contact envelope references missing link `{}`",
                        envelope.link_name
                    ));
                }
            }
            for sensor_id in safety_overlay.force_rate_limits.keys() {
                if !model.sensor_mounts.iter().any(|mount| mount.sensor_id == *sensor_id) {
                    issues.push(format!(
                        "safety overlay force-rate limit references missing sensor `{sensor_id}`"
                    ));
                }
            }
            for zone in safety_overlay
                .workspace_restrictions
                .iter()
                .chain(&safety_overlay.human_presence_zones)
                .chain(&safety_overlay.contact_allowed_zones)
            {
                Self::push_missing_frame_issue(
                    &mut issues,
                    &format!("safety zone `{}`", zone.name),
                    &zone.origin_frame,
                    frame_graph,
                );
            }
        }

        issues.sort();
        issues.dedup();
        issues
    }

    fn joint_position_inputs(&self, joint_positions: &[f64]) -> BTreeMap<String, f64> {
        self.model
            .joints
            .iter()
            .enumerate()
            .filter_map(|(index, joint)| {
                joint_positions
                    .get(index)
                    .copied()
                    .map(|position| (joint.name.clone(), position))
            })
            .collect()
    }

    fn tcp_pose_in_snapshot(
        &self,
        snapshot: &FrameGraphSnapshot,
        tcp: &ToolCenterPoint,
        relative_to: Option<&str>,
    ) -> Result<ResolvedFramePose, String> {
        let parent_pose = self.resolve_frame_pose(snapshot, &tcp.parent_link, relative_to)?;
        let transform = parent_pose.transform.compose(&tcp.offset);
        let freshness = if parent_pose.freshness.is_fresh() {
            FreshnessState::Fresh
        } else {
            FreshnessState::Unknown
        };

        Ok(ResolvedFramePose {
            frame_id: tcp.name.clone(),
            relative_to: parent_pose.relative_to,
            transform,
            freshness,
        })
    }

    fn sensor_pose_in_snapshot(
        &self,
        snapshot: &FrameGraphSnapshot,
        sensor: &super::model::SensorMount,
        relative_to: Option<&str>,
    ) -> Result<ResolvedFramePose, String> {
        let parent_pose = self.resolve_frame_pose(snapshot, &sensor.parent_link, relative_to)?;
        let transform = parent_pose.transform.compose(&sensor.offset);
        let freshness = if parent_pose.freshness.is_fresh() {
            FreshnessState::Fresh
        } else {
            FreshnessState::Unknown
        };

        Ok(ResolvedFramePose {
            frame_id: sensor.sensor_id.clone(),
            relative_to: parent_pose.relative_to,
            transform,
            freshness,
        })
    }

    fn unit_quaternion(rotation: [f64; 4]) -> UnitQuaternion<f64> {
        UnitQuaternion::new_normalize(Quaternion::new(rotation[0], rotation[1], rotation[2], rotation[3]))
    }

    fn quaternion_array(rotation: &UnitQuaternion<f64>) -> [f64; 4] {
        let q = rotation.quaternion();
        [q.w, q.i, q.j, q.k]
    }

    #[allow(clippy::missing_const_for_fn)]
    fn vector3(value: [f64; 3]) -> Vector3<f64> {
        Vector3::new(value[0], value[1], value[2])
    }

    fn scaled_axis(rotation: [f64; 4]) -> Vector3<f64> {
        match Self::unit_quaternion(rotation).axis_angle() {
            Some((axis, angle)) => axis.into_inner() * angle,
            None => Vector3::zeros(),
        }
    }

    fn classify_conditioning(min_singular_value: Option<f64>) -> JacobianConditioning {
        match min_singular_value {
            None => JacobianConditioning::Singular,
            Some(value) if value < 1e-6 => JacobianConditioning::Singular,
            Some(value) if value < 1e-4 => JacobianConditioning::IllConditioned,
            Some(value) if value < 1e-3 => JacobianConditioning::Marginal,
            Some(_) => JacobianConditioning::WellConditioned,
        }
    }

    fn jacobian_metrics(jacobian: &DMatrix<f64>) -> (usize, Option<f64>, Option<f64>, JacobianConditioning) {
        let singular_values = jacobian.clone().svd(false, false).singular_values;
        let jacobian_rank = singular_values.iter().filter(|&&value| value > 1e-6).count();
        let min_singular_value = singular_values.iter().copied().reduce(f64::min);
        let manipulability = (!singular_values.is_empty()).then(|| singular_values.iter().copied().product());
        let conditioning = Self::classify_conditioning(min_singular_value);
        (jacobian_rank, min_singular_value, manipulability, conditioning)
    }

    fn combine_workspace_margins(workspaces: &[&WorkspacePoseEvaluation]) -> Option<f64> {
        let mut min_margin = None;
        for workspace in workspaces {
            if let Some(margin) = workspace.min_margin_m {
                min_margin = Some(min_margin.map_or(margin, |current: f64| current.min(margin)));
            }
        }
        min_margin
    }

    fn collect_workspace_degradation_reasons(
        workspaces: &[&WorkspacePoseEvaluation],
        reasons: &mut Vec<PlanDegradationReason>,
    ) {
        for workspace in workspaces {
            if workspace
                .alerts
                .iter()
                .any(|alert| alert.contains("outside every allowed workspace zone"))
            {
                reasons.push(PlanDegradationReason::OutsideAllowedWorkspace);
            }
            if workspace
                .alerts
                .iter()
                .any(|alert| alert.contains("restricted workspace zone"))
            {
                reasons.push(PlanDegradationReason::RestrictedWorkspace);
            }
            if workspace.in_human_presence {
                reasons.push(PlanDegradationReason::HumanPresenceZone);
            }
        }
    }

    fn build_execution_assessment(
        workspaces: &[&WorkspacePoseEvaluation],
        conditioning: JacobianConditioning,
        converged: Option<bool>,
        extra_alerts: &[String],
    ) -> PlanExecutionAssessment {
        let mut alerts = Vec::new();
        let mut reasons = Vec::new();

        for workspace in workspaces {
            alerts.extend(workspace.alerts.iter().cloned());
        }
        alerts.extend(extra_alerts.iter().cloned());

        Self::collect_workspace_degradation_reasons(workspaces, &mut reasons);
        match conditioning {
            JacobianConditioning::Singular => reasons.push(PlanDegradationReason::SingularJacobian),
            JacobianConditioning::IllConditioned => reasons.push(PlanDegradationReason::IllConditionedJacobian),
            JacobianConditioning::Marginal | JacobianConditioning::WellConditioned => {}
        }
        if matches!(converged, Some(false)) {
            reasons.push(PlanDegradationReason::NonConverged);
        }

        alerts.sort();
        alerts.dedup();
        reasons.sort();
        reasons.dedup();

        let workspace_safe = workspaces.iter().all(|workspace| workspace.safe);
        let in_human_presence = workspaces.iter().any(|workspace| workspace.in_human_presence);
        let safe = workspace_safe && !in_human_presence;
        let executable = safe
            && converged.unwrap_or(true)
            && !matches!(
                conditioning,
                JacobianConditioning::Singular | JacobianConditioning::IllConditioned
            );

        PlanExecutionAssessment {
            executable,
            safe,
            conditioning,
            min_workspace_margin_m: Self::combine_workspace_margins(workspaces),
            degradation_reasons: reasons,
            alerts,
        }
    }

    fn interpolate_transform(start: &Transform3D, end: &Transform3D, alpha: f64) -> Transform3D {
        let alpha = alpha.clamp(0.0, 1.0);
        let start_q = Self::unit_quaternion(start.rotation);
        let end_q = Self::unit_quaternion(end.rotation);
        let blended_q = start_q.slerp(&end_q, alpha);
        Transform3D {
            translation: [
                (end.translation[0] - start.translation[0]).mul_add(alpha, start.translation[0]),
                (end.translation[1] - start.translation[1]).mul_add(alpha, start.translation[1]),
                (end.translation[2] - start.translation[2]).mul_add(alpha, start.translation[2]),
            ],
            rotation: Self::quaternion_array(&blended_q),
            timestamp_ns: end.timestamp_ns,
        }
    }

    /// Compile a runtime from the three layers.
    ///
    /// Computes the `combined_digest` from the individual layer digests.
    #[must_use]
    pub fn compile(
        model: EmbodimentModel,
        calibration: Option<CalibrationOverlay>,
        safety_overlay: Option<SafetyOverlay>,
    ) -> Self {
        let mut model = model;
        let model_digest = model.compute_digest();
        let mut validation_issues = Vec::new();
        if model.model_digest != model_digest {
            validation_issues.push(format!(
                "model digest was normalized at runtime: provided={} computed={}",
                model.model_digest, model_digest
            ));
        }
        model.model_digest.clone_from(&model_digest);
        let calibration_digest = calibration
            .as_ref()
            .filter(|overlay| overlay.is_valid_for_model(&model_digest))
            .map_or_else(|| "none".to_string(), |overlay| overlay.calibration_digest.clone());
        let safety_digest = safety_overlay
            .as_ref()
            .map_or_else(|| "none".to_string(), |overlay| overlay.overlay_digest.clone());
        let valid_calibration = calibration
            .as_ref()
            .filter(|overlay| overlay.is_valid_for_model(&model_digest));
        let compiled_frame_graph = Self::compiled_frame_graph(&model, valid_calibration);
        let frame_graph = compiled_frame_graph.frame_graph;
        let (watched_frames, watched_frame_issues) =
            Self::resolve_watched_frames(&model, valid_calibration, safety_overlay.as_ref(), &frame_graph);
        let active_calibration_id = valid_calibration.map(|overlay| overlay.calibration_id.clone());
        validation_issues.extend(compiled_frame_graph.validation_issues);
        validation_issues.extend(watched_frame_issues);
        validation_issues.extend(Self::collect_validation_issues(
            &model,
            calibration.as_ref(),
            valid_calibration.is_some(),
            safety_overlay.as_ref(),
            &frame_graph,
        ));
        validation_issues.sort();
        validation_issues.dedup();
        let combined_input = format!("{model_digest}:{calibration_digest}:{safety_digest}");
        let combined_digest = hex::encode(Sha256::digest(combined_input.as_bytes()));
        let joint_count = model.joints.len();
        let tcp_count = model.tcps.len();

        Self {
            model,
            calibration,
            safety_overlay,
            model_digest,
            calibration_digest,
            safety_digest,
            combined_digest,
            frame_graph,
            active_calibration_id,
            joint_count,
            tcp_count,
            watched_frames,
            validation_issues,
        }
    }

    /// Build a `FrameGraphSnapshot` from the current model's frame tree.
    #[must_use]
    pub fn build_frame_snapshot(&self) -> FrameGraphSnapshot {
        self.build_frame_snapshot_with_input(0, 0, &FrameSnapshotInput::default())
    }

    /// Build a snapshot at the given time, enriching it with caller-supplied
    /// dynamic transforms that are not captured in the compiled static graph.
    #[must_use]
    pub fn build_frame_snapshot_with_dynamic_transforms(
        &self,
        snapshot_id: u64,
        timestamp_ns: u64,
        dynamic_transforms: &[(String, Transform3D)],
    ) -> FrameGraphSnapshot {
        let typed_dynamic_transforms: Vec<TimestampedTransform> = dynamic_transforms
            .iter()
            .map(|(frame_id, transform)| {
                let node = self.frame_graph.get_frame(frame_id);
                let freshness =
                    if transform.timestamp_ns == 0 || (timestamp_ns != 0 && transform.timestamp_ns > timestamp_ns) {
                        FreshnessState::Unknown
                    } else {
                        FreshnessState::Fresh
                    };
                TimestampedTransform {
                    frame_id: frame_id.clone(),
                    parent_id: node.and_then(|frame| frame.parent_id.clone()),
                    transform: transform.clone(),
                    freshness,
                    source: node.map_or(FrameSource::Dynamic, |frame| frame.source.clone()),
                }
            })
            .collect();

        self.build_frame_snapshot_with_input(
            snapshot_id,
            timestamp_ns,
            &FrameSnapshotInput {
                clock_domain: ClockDomain::Monotonic,
                joint_positions: BTreeMap::new(),
                dynamic_transforms: typed_dynamic_transforms,
                world_anchors: Vec::new(),
                validation_issues: Vec::new(),
            },
        )
    }

    /// Build a snapshot at the given time from explicit dynamic transforms and anchors.
    #[must_use]
    pub fn build_frame_snapshot_with_input(
        &self,
        snapshot_id: u64,
        timestamp_ns: u64,
        input: &FrameSnapshotInput,
    ) -> FrameGraphSnapshot {
        let (joint_state_transforms, joint_state_issues) =
            self.joint_state_transforms(&input.joint_positions, timestamp_ns);
        let mut frame_freshness = std::collections::BTreeMap::new();
        let mut sources = Vec::new();
        for frame_id in self.frame_graph.all_frame_ids() {
            let Some(node) = self.frame_graph.get_frame(frame_id) else {
                continue;
            };
            let freshness = match node.source {
                FrameSource::Static => FreshnessState::Fresh,
                FrameSource::Dynamic | FrameSource::Computed => FreshnessState::Unknown,
            };
            frame_freshness.insert(frame_id.to_string(), freshness);
            if !sources.contains(&node.source) {
                sources.push(node.source.clone());
            }
        }
        let mut validation_issues = self.validation_issues.clone();
        validation_issues.extend(joint_state_issues);
        validation_issues.extend(input.validation_issues.clone());

        let dynamic_transforms: Vec<TimestampedTransform> = joint_state_transforms
            .iter()
            .chain(input.dynamic_transforms.iter())
            .filter_map(|transform| {
                let Some(node) = self.frame_graph.get_frame(&transform.frame_id) else {
                    validation_issues.push(format!(
                        "dynamic transform references missing frame `{}`",
                        transform.frame_id
                    ));
                    return None;
                };
                if let Some(parent_id) = transform.parent_id.as_deref()
                    && node.parent_id.as_deref() != Some(parent_id)
                {
                    validation_issues.push(format!(
                        "dynamic transform for `{}` declared parent `{parent_id}` but compiled graph uses `{}`",
                        transform.frame_id,
                        node.parent_id.as_deref().unwrap_or("<root>")
                    ));
                }
                if timestamp_ns != 0 && transform.transform.timestamp_ns > timestamp_ns {
                    validation_issues.push(format!(
                        "dynamic transform for `{}` has timestamp {} newer than snapshot {}",
                        transform.frame_id, transform.transform.timestamp_ns, timestamp_ns
                    ));
                }
                frame_freshness.insert(transform.frame_id.clone(), transform.freshness.clone());
                Some(transform.clone())
            })
            .collect();

        for transform in &dynamic_transforms {
            if !sources.contains(&transform.source) {
                sources.push(transform.source.clone());
            }
        }

        let world_anchors: Vec<WorldAnchor> = input
            .world_anchors
            .iter()
            .filter_map(|anchor| {
                if !self.frame_graph.frame_exists(&anchor.frame_id) {
                    validation_issues.push(format!(
                        "world anchor `{}` references missing frame `{}`",
                        anchor.anchor_id, anchor.frame_id
                    ));
                    return None;
                }
                Some(anchor.clone())
            })
            .collect();

        let freshness_inputs: Vec<&FreshnessState> = if self.watched_frames.is_empty() {
            frame_freshness.values().collect()
        } else {
            self.watched_frames
                .iter()
                .filter_map(|frame_id| frame_freshness.get(frame_id))
                .collect()
        };
        let freshness = if !freshness_inputs.is_empty() && freshness_inputs.iter().all(|state| state.is_fresh()) {
            FreshnessState::Fresh
        } else {
            FreshnessState::Unknown
        };
        validation_issues.sort();
        validation_issues.dedup();

        FrameGraphSnapshot {
            snapshot_id,
            timestamp_ns,
            clock_domain: input.clock_domain,
            frame_tree: self.frame_graph.clone(),
            freshness,
            model_digest: self.model_digest.clone(),
            calibration_digest: self.calibration_digest.clone(),
            active_calibration_id: self.active_calibration_id.clone(),
            dynamic_transforms,
            watched_frames: self.watched_frames.clone(),
            frame_freshness,
            sources,
            world_anchors,
            validation_issues,
        }
    }

    /// Whether the currently bound calibration is valid for this runtime.
    #[must_use]
    pub const fn calibration_valid(&self) -> bool {
        self.calibration.is_none() || self.active_calibration_id.is_some()
    }

    /// Whether the embodiment exposes an actively repositionable perception sensor.
    #[must_use]
    pub fn active_perception_available(&self) -> bool {
        self.model
            .sensor_mounts
            .iter()
            .any(|sensor| sensor.is_actuated && sensor.frustum.is_some())
    }

    /// Merge model and safety-overlay workspace zones into the active runtime envelope.
    #[must_use]
    pub fn workspace_envelope(&self) -> WorkspaceEnvelope {
        let mut zones = self.model.workspace_zones.clone();
        if let Some(overlay) = &self.safety_overlay {
            zones.extend(overlay.workspace_restrictions.clone());
            zones.extend(overlay.human_presence_zones.clone());
            zones.extend(overlay.contact_allowed_zones.clone());
        }
        WorkspaceEnvelope { zones }
    }

    /// Resolve a frame pose from a snapshot relative to a reference frame.
    pub fn resolve_frame_pose(
        &self,
        snapshot: &FrameGraphSnapshot,
        frame_id: &str,
        relative_to: Option<&str>,
    ) -> Result<ResolvedFramePose, String> {
        let reference_frame = relative_to
            .or_else(|| snapshot.frame_tree.root())
            .or_else(|| self.frame_graph.root())
            .ok_or_else(|| "frame snapshot missing root frame".to_string())?;
        let freshness = snapshot
            .frame_freshness
            .get(frame_id)
            .cloned()
            .unwrap_or(FreshnessState::Unknown);
        let (projection_tree, validation_issues) = Self::overlay_snapshot_tree(snapshot);
        if !projection_tree.frame_exists(frame_id) {
            return Err(format!("frame `{frame_id}` missing from snapshot frame tree"));
        }
        if !projection_tree.frame_exists(reference_frame) {
            return Err(format!(
                "reference frame `{reference_frame}` missing from snapshot frame tree"
            ));
        }
        let transform = projection_tree
            .lookup_transform(reference_frame, frame_id)
            .map_err(|error| format!("failed to resolve frame `{frame_id}` from `{reference_frame}`: {error}"))?;
        if let Some(issue) = validation_issues.first() {
            tracing::debug!(
                frame_id,
                reference_frame,
                issue,
                "snapshot projection had validation issues"
            );
        }
        Ok(ResolvedFramePose {
            frame_id: frame_id.to_string(),
            relative_to: reference_frame.to_string(),
            transform,
            freshness,
        })
    }

    /// Compute a named TCP pose from the current joint state.
    pub fn compute_tcp_pose(&self, tcp_name: &str, joint_positions: &[f64]) -> Result<Transform3D, String> {
        let snapshot = self.build_frame_snapshot_with_input(
            0,
            0,
            &FrameSnapshotInput {
                joint_positions: self.joint_position_inputs(joint_positions),
                ..FrameSnapshotInput::default()
            },
        );
        let tcp = self
            .model
            .get_tcp(tcp_name)
            .ok_or_else(|| format!("tcp `{tcp_name}` not found in embodiment model"))?;
        self.tcp_pose_in_snapshot(&snapshot, tcp, None)
            .map(|pose| pose.transform)
    }

    /// Compute the geometric Jacobian for a named TCP.
    pub fn compute_tcp_jacobian(&self, tcp_name: &str, joint_positions: &[f64]) -> Result<DMatrix<f64>, String> {
        let snapshot = self.build_frame_snapshot_with_input(
            0,
            0,
            &FrameSnapshotInput {
                joint_positions: self.joint_position_inputs(joint_positions),
                ..FrameSnapshotInput::default()
            },
        );
        let tcp = self
            .model
            .get_tcp(tcp_name)
            .ok_or_else(|| format!("tcp `{tcp_name}` not found in embodiment model"))?;
        let tcp_pose = self.tcp_pose_in_snapshot(&snapshot, tcp, None)?;
        let chain = self.joint_chain_for_frame(&tcp.parent_link)?;
        let mut jacobian = DMatrix::<f64>::zeros(6, chain.len());
        let tcp_position = Self::vector3(tcp_pose.transform.translation);

        for (column, joint_name) in chain.iter().enumerate() {
            let joint = self
                .model
                .get_joint(joint_name)
                .ok_or_else(|| format!("joint `{joint_name}` missing from embodiment model"))?;
            let parent_pose = self.resolve_frame_pose(&snapshot, &joint.parent_link, None)?;
            let joint_origin_transform = parent_pose.transform.compose(&joint.origin);
            let joint_origin = Self::vector3(joint_origin_transform.translation);
            let axis = Self::normalized_axis(joint.axis)
                .ok_or_else(|| format!("joint `{joint_name}` has invalid zero-length axis"))?;
            let axis_world = Self::unit_quaternion(joint_origin_transform.rotation) * Self::vector3(axis);

            let (linear, angular) = match joint.joint_type {
                JointType::Revolute | JointType::Continuous => {
                    (axis_world.cross(&(tcp_position - joint_origin)), axis_world)
                }
                JointType::Prismatic => (axis_world, Vector3::zeros()),
                JointType::Fixed => (Vector3::zeros(), Vector3::zeros()),
            };

            jacobian[(0, column)] = linear[0];
            jacobian[(1, column)] = linear[1];
            jacobian[(2, column)] = linear[2];
            jacobian[(3, column)] = angular[0];
            jacobian[(4, column)] = angular[1];
            jacobian[(5, column)] = angular[2];
        }

        Ok(jacobian)
    }

    /// Compute the geometric Jacobian for the primary TCP in the model.
    pub fn compute_jacobian(&self, joint_positions: &[f64]) -> Result<DMatrix<f64>, String> {
        let tcp = self
            .model
            .tcps
            .first()
            .ok_or_else(|| "embodiment model does not declare any TCPs".to_string())?;
        self.compute_tcp_jacobian(&tcp.name, joint_positions)
    }

    /// Compute a named sensor pose from the current joint state.
    pub fn compute_sensor_pose(&self, sensor_id: &str, joint_positions: &[f64]) -> Result<Transform3D, String> {
        let snapshot = self.build_frame_snapshot_with_input(
            0,
            0,
            &FrameSnapshotInput {
                joint_positions: self.joint_position_inputs(joint_positions),
                ..FrameSnapshotInput::default()
            },
        );
        let sensor = self
            .model
            .sensor_mounts
            .iter()
            .find(|sensor| sensor.sensor_id == sensor_id)
            .ok_or_else(|| format!("sensor `{sensor_id}` not found in embodiment model"))?;
        self.sensor_pose_in_snapshot(&snapshot, sensor, None)
            .map(|pose| pose.transform)
    }

    /// Resolve the actuated joint chain that repositions a sensor.
    pub fn sensor_joint_chain(&self, sensor_id: &str) -> Result<Vec<String>, String> {
        let sensor = self
            .model
            .sensor_mounts
            .iter()
            .find(|sensor| sensor.sensor_id == sensor_id)
            .ok_or_else(|| format!("sensor `{sensor_id}` not found in embodiment model"))?;
        if !sensor.is_actuated {
            return Ok(Vec::new());
        }

        let full_chain = self.joint_chain_for_frame(&sensor.parent_link)?;
        let actuation_joint = sensor
            .actuation_joint
            .as_deref()
            .ok_or_else(|| format!("actuated sensor `{sensor_id}` is missing `actuation_joint`"))?;
        let Some(index) = full_chain.iter().position(|joint| joint == actuation_joint) else {
            return Err(format!(
                "sensor `{sensor_id}` actuation joint `{actuation_joint}` is not in the parent-link chain"
            ));
        };
        Ok(full_chain[index..].to_vec())
    }

    /// Evaluate whether a sensor can be repositioned safely in the current workspace.
    pub fn evaluate_sensor_reachability(
        &self,
        sensor_id: &str,
        joint_positions: &[f64],
    ) -> Result<SensorReachability, String> {
        let snapshot = self.build_frame_snapshot_with_input(
            0,
            0,
            &FrameSnapshotInput {
                joint_positions: self.joint_position_inputs(joint_positions),
                ..FrameSnapshotInput::default()
            },
        );
        let sensor = self
            .model
            .sensor_mounts
            .iter()
            .find(|sensor| sensor.sensor_id == sensor_id)
            .ok_or_else(|| format!("sensor `{sensor_id}` not found in embodiment model"))?;
        let pose = self.sensor_pose_in_snapshot(&snapshot, sensor, None)?;
        let joint_chain = self.sensor_joint_chain(sensor_id)?;
        let envelope = self.workspace_envelope();
        let mut alerts = Vec::new();
        let mut allowed_margin = None;
        let mut restricted_clearance = None;
        let mut in_human_presence = false;

        for zone in &envelope.zones {
            let pose_in_zone = self.sensor_pose_in_snapshot(&snapshot, sensor, Some(&zone.origin_frame))?;
            let signed_margin = zone.shape.signed_margin(pose_in_zone.transform.translation) - zone.margin_m;
            let contains_origin = signed_margin >= 0.0;
            match zone.zone_type {
                super::workspace::ZoneType::Allowed => {
                    allowed_margin =
                        Some(allowed_margin.map_or(signed_margin, |margin: f64| margin.max(signed_margin)));
                }
                super::workspace::ZoneType::Restricted => {
                    let clearance = -signed_margin;
                    restricted_clearance =
                        Some(restricted_clearance.map_or(clearance, |margin: f64| margin.min(clearance)));
                    if contains_origin {
                        alerts.push(format!("sensor `{sensor_id}` entered restricted zone `{}`", zone.name));
                    }
                }
                super::workspace::ZoneType::HumanPresence => {
                    if contains_origin {
                        in_human_presence = true;
                        alerts.push(format!(
                            "sensor `{sensor_id}` is inside human-presence zone `{}`",
                            zone.name
                        ));
                    }
                }
            }
        }

        let has_allowed_zone = envelope
            .zones
            .iter()
            .any(|zone| matches!(zone.zone_type, super::workspace::ZoneType::Allowed));
        let workspace_safe = (!has_allowed_zone || allowed_margin.is_some_and(|margin| margin >= 0.0))
            && !restricted_clearance.is_some_and(|margin| margin < 0.0);
        if has_allowed_zone && !allowed_margin.is_some_and(|margin| margin >= 0.0) {
            alerts.push(format!(
                "sensor `{sensor_id}` lies outside every allowed workspace zone"
            ));
        }

        alerts.sort();
        alerts.dedup();

        let min_margin_m = match (has_allowed_zone, allowed_margin, restricted_clearance) {
            (true, Some(allowed_margin), Some(restricted_clearance)) => Some(allowed_margin.min(restricted_clearance)),
            (true, Some(allowed_margin), None) => Some(allowed_margin),
            (true, None, Some(restricted_clearance)) | (false, _, Some(restricted_clearance)) => {
                Some(restricted_clearance)
            }
            _ => None,
        };

        Ok(SensorReachability {
            sensor_id: sensor.sensor_id.clone(),
            actuated: sensor.is_actuated,
            has_frustum: sensor.frustum.is_some(),
            joint_chain,
            pose,
            workspace_safe,
            min_margin_m,
            in_human_presence,
            alerts,
        })
    }

    /// Verify that an active-perception command is reachable and workspace-safe.
    pub fn evaluate_active_perception_command(
        &self,
        command: &ActivePerceptionCommand,
        joint_positions: &[f64],
    ) -> Result<ActivePerceptionCommandCheck, String> {
        let sensor = self.evaluate_sensor_reachability(&command.sensor_id, joint_positions)?;
        let mut alerts = sensor.alerts.clone();
        let mut target_valid = true;
        let planned_pose = match &command.target {
            ViewpointTarget::LookAt { frame_id } => {
                if !self.frame_graph.frame_exists(frame_id) {
                    alerts.push(format!("active perception target frame `{frame_id}` does not exist"));
                    target_valid = false;
                }
                None
            }
            ViewpointTarget::MoveTo { pose } => Some(self.evaluate_pose_workspace(pose, &sensor.pose.relative_to)?),
            ViewpointTarget::Adjust { delta } => {
                Some(self.evaluate_pose_workspace(&sensor.pose.transform.compose(delta), &sensor.pose.relative_to)?)
            }
        };

        if !sensor.actuated {
            alerts.push(format!(
                "sensor `{}` is not actuated and cannot be repositioned",
                command.sensor_id
            ));
        }
        if !sensor.has_frustum {
            alerts.push(format!(
                "sensor `{}` has no camera frustum for active perception planning",
                command.sensor_id
            ));
        }

        if let ObservationGoal::CoverRegion { frame_id, .. } = &command.observation_goal
            && !self.frame_graph.frame_exists(frame_id)
        {
            alerts.push(format!("observation goal frame `{frame_id}` does not exist"));
            target_valid = false;
        }

        if let Some(planned_pose) = &planned_pose {
            alerts.extend(planned_pose.alerts.iter().cloned());
        }

        alerts.sort();
        alerts.dedup();
        let sensor_workspace_safe = sensor.workspace_safe;
        let sensor_actuated = sensor.actuated;
        let sensor_has_frustum = sensor.has_frustum;
        let mut execution_workspaces = Vec::new();
        if let Some(planned_pose) = planned_pose.as_ref() {
            execution_workspaces.push(planned_pose);
        }
        let execution = Self::build_execution_assessment(
            &execution_workspaces,
            if sensor_actuated {
                JacobianConditioning::WellConditioned
            } else {
                JacobianConditioning::Marginal
            },
            Some(target_valid && sensor_workspace_safe && sensor_actuated && sensor_has_frustum),
            &alerts,
        );

        Ok(ActivePerceptionCommandCheck {
            sensor,
            target_valid,
            planned_pose: planned_pose.clone(),
            execution: execution.clone(),
            safe: execution.executable && planned_pose.as_ref().is_none_or(|pose| pose.safe),
            alerts,
        })
    }

    /// Evaluate whether a planned TCP pose is workspace-safe and locally well-conditioned.
    pub fn evaluate_tcp_reachability(
        &self,
        tcp_name: &str,
        joint_positions: &[f64],
        target_pose: &Transform3D,
        relative_to: &str,
    ) -> Result<TcpReachabilityEvaluation, String> {
        let snapshot = self.build_frame_snapshot_with_input(
            0,
            0,
            &FrameSnapshotInput {
                joint_positions: self.joint_position_inputs(joint_positions),
                ..FrameSnapshotInput::default()
            },
        );
        let tcp = self
            .model
            .get_tcp(tcp_name)
            .ok_or_else(|| format!("tcp `{tcp_name}` not found in embodiment model"))?;
        let current_pose = self.tcp_pose_in_snapshot(&snapshot, tcp, None)?;
        let workspace = self.evaluate_pose_workspace(target_pose, relative_to)?;
        let reference_pose = self.resolve_frame_pose(&snapshot, relative_to, None)?;
        let root_frame = current_pose.relative_to.clone();
        let target_in_root = reference_pose.transform.compose(target_pose);
        let target_pose = ResolvedFramePose {
            frame_id: tcp.name.clone(),
            relative_to: root_frame,
            transform: target_in_root,
            freshness: FreshnessState::Unknown,
        };

        let delta = current_pose.transform.inverse().compose(&target_pose.transform);
        let translation_error_m = Self::vector3(delta.translation).norm();
        let orientation_error_rad = Self::unit_quaternion(delta.rotation).angle();

        let jacobian = self.compute_tcp_jacobian(tcp_name, joint_positions)?;
        let (jacobian_rank, min_singular_value, manipulability, conditioning) = Self::jacobian_metrics(&jacobian);
        let well_conditioned = matches!(
            conditioning,
            JacobianConditioning::Marginal | JacobianConditioning::WellConditioned
        );

        let mut alerts = workspace.alerts.clone();
        if !well_conditioned {
            alerts.push(format!(
                "tcp `{tcp_name}` jacobian is ill-conditioned near the current state"
            ));
        }
        alerts.sort();
        alerts.dedup();
        let execution = Self::build_execution_assessment(&[&workspace], conditioning, None, &alerts);

        Ok(TcpReachabilityEvaluation {
            tcp_name: tcp.name.clone(),
            current_pose,
            target_pose,
            workspace,
            translation_error_m,
            orientation_error_rad,
            jacobian_rank,
            min_singular_value,
            manipulability,
            conditioning,
            execution: execution.clone(),
            well_conditioned,
            safe: execution.executable,
            alerts,
        })
    }

    /// Plan a single bounded TCP motion step in joint space.
    ///
    /// Uses a damped-least-squares Jacobian step, clamps the result to joint
    /// limits, and evaluates both the target and projected pose against the
    /// runtime workspace envelope.
    #[allow(clippy::too_many_lines)]
    pub fn plan_tcp_step(
        &self,
        tcp_name: &str,
        joint_positions: &[f64],
        target_pose: &Transform3D,
        relative_to: &str,
        max_joint_step: f64,
    ) -> Result<TcpMotionPlan, String> {
        if !max_joint_step.is_finite() || max_joint_step < 0.0 {
            return Err("max_joint_step must be finite and non-negative".into());
        }

        let snapshot = self.build_frame_snapshot_with_input(
            0,
            0,
            &FrameSnapshotInput {
                joint_positions: self.joint_position_inputs(joint_positions),
                ..FrameSnapshotInput::default()
            },
        );
        let tcp = self
            .model
            .get_tcp(tcp_name)
            .ok_or_else(|| format!("tcp `{tcp_name}` not found in embodiment model"))?;
        let current_pose = self.tcp_pose_in_snapshot(&snapshot, tcp, Some(relative_to))?;
        let target_pose_resolved = ResolvedFramePose {
            frame_id: tcp.name.clone(),
            relative_to: relative_to.to_string(),
            transform: target_pose.clone(),
            freshness: FreshnessState::Unknown,
        };
        let target_workspace = self.evaluate_pose_workspace(target_pose, relative_to)?;

        let current_joint_positions: Vec<f64> = self
            .model
            .joints
            .iter()
            .enumerate()
            .map(|(index, _)| joint_positions.get(index).copied().unwrap_or(0.0))
            .collect();
        let chain = self.joint_chain_for_frame(&tcp.parent_link)?;
        if chain.is_empty() {
            return Err(format!("tcp `{tcp_name}` has no controllable joint chain"));
        }
        let chain_indices: Vec<usize> = chain
            .iter()
            .map(|joint_name| {
                self.model
                    .joints
                    .iter()
                    .position(|joint| joint.name == *joint_name)
                    .ok_or_else(|| format!("joint `{joint_name}` missing from embodiment model"))
            })
            .collect::<Result<_, _>>()?;

        let jacobian = self.compute_tcp_jacobian(tcp_name, &current_joint_positions)?;
        let (jacobian_rank, min_singular_value, manipulability, conditioning) = Self::jacobian_metrics(&jacobian);
        let well_conditioned = matches!(
            conditioning,
            JacobianConditioning::Marginal | JacobianConditioning::WellConditioned
        );

        let current_root_pose = self.tcp_pose_in_snapshot(&snapshot, tcp, None)?;
        let reference_root_pose = self.resolve_frame_pose(&snapshot, relative_to, None)?;
        let target_root_transform = reference_root_pose.transform.compose(target_pose);
        let delta_root = current_root_pose.transform.inverse().compose(&target_root_transform);
        let rotation_error = Self::scaled_axis(delta_root.rotation);
        let error = DVector::from_vec(vec![
            delta_root.translation[0],
            delta_root.translation[1],
            delta_root.translation[2],
            rotation_error[0],
            rotation_error[1],
            rotation_error[2],
        ]);

        let damping = 1e-3;
        let jj_t = &jacobian * jacobian.transpose();
        let system = jj_t + DMatrix::<f64>::identity(6, 6) * (damping * damping);
        let Some(system_inv) = system.try_inverse() else {
            return Err(format!(
                "failed to invert damped Jacobian system for tcp `{tcp_name}` planning"
            ));
        };
        let delta_q = jacobian.transpose() * system_inv * error;
        let max_delta = delta_q.iter().fold(0.0_f64, |acc, value| acc.max(value.abs()));
        let scale = if max_joint_step > 0.0 && max_delta > max_joint_step {
            max_joint_step / max_delta
        } else if max_joint_step == 0.0 {
            0.0
        } else {
            1.0
        };

        let mut proposed_joint_positions = current_joint_positions;
        for (delta, joint_index) in delta_q.iter().zip(chain_indices.iter()) {
            let limits = &self.model.joints[*joint_index].limits;
            proposed_joint_positions[*joint_index] = (*delta)
                .mul_add(scale, proposed_joint_positions[*joint_index])
                .clamp(limits.position_min, limits.position_max);
        }

        let projected_root_transform = self.compute_tcp_pose(tcp_name, &proposed_joint_positions)?;
        let projected_relative_transform = reference_root_pose
            .transform
            .inverse()
            .compose(&projected_root_transform);
        let projected_pose = ResolvedFramePose {
            frame_id: tcp.name.clone(),
            relative_to: relative_to.to_string(),
            transform: projected_relative_transform,
            freshness: FreshnessState::Unknown,
        };
        let projected_workspace = self.evaluate_pose_workspace(&projected_pose.transform, relative_to)?;
        let remaining_delta = projected_pose.transform.inverse().compose(target_pose);
        let remaining_translation_error_m = Self::vector3(remaining_delta.translation).norm();
        let remaining_orientation_error_rad = Self::scaled_axis(remaining_delta.rotation).norm();

        let mut alerts = target_workspace.alerts.clone();
        alerts.extend(projected_workspace.alerts.iter().cloned());
        if !well_conditioned {
            alerts.push(format!(
                "tcp `{tcp_name}` jacobian is ill-conditioned near the current state"
            ));
        }
        alerts.sort();
        alerts.dedup();
        let execution =
            Self::build_execution_assessment(&[&target_workspace, &projected_workspace], conditioning, None, &alerts);

        Ok(TcpMotionPlan {
            tcp_name: tcp.name.clone(),
            current_pose,
            target_pose: target_pose_resolved,
            projected_pose,
            proposed_joint_positions,
            target_workspace,
            projected_workspace,
            remaining_translation_error_m,
            remaining_orientation_error_rad,
            jacobian_rank,
            min_singular_value,
            manipulability,
            conditioning,
            execution: execution.clone(),
            safe: execution.executable,
            alerts,
        })
    }

    /// Iteratively solve toward a TCP target pose using bounded Jacobian steps.
    #[allow(clippy::too_many_arguments)]
    pub fn solve_tcp_ik(
        &self,
        tcp_name: &str,
        joint_positions: &[f64],
        target_pose: &Transform3D,
        relative_to: &str,
        max_joint_step: f64,
        max_iterations: usize,
        translation_tolerance_m: f64,
        orientation_tolerance_rad: f64,
    ) -> Result<TcpIkSolution, String> {
        if !translation_tolerance_m.is_finite() || translation_tolerance_m < 0.0 {
            return Err("translation_tolerance_m must be finite and non-negative".into());
        }
        if !orientation_tolerance_rad.is_finite() || orientation_tolerance_rad < 0.0 {
            return Err("orientation_tolerance_rad must be finite and non-negative".into());
        }

        let mut working_joints: Vec<f64> = self
            .model
            .joints
            .iter()
            .enumerate()
            .map(|(index, _)| joint_positions.get(index).copied().unwrap_or(0.0))
            .collect();
        let mut steps = Vec::new();
        let mut alerts = Vec::new();

        for _ in 0..max_iterations {
            let plan = self.plan_tcp_step(tcp_name, &working_joints, target_pose, relative_to, max_joint_step)?;
            working_joints.clone_from(&plan.proposed_joint_positions);
            alerts.extend(plan.alerts.iter().cloned());
            let converged = plan.remaining_translation_error_m <= translation_tolerance_m
                && plan.remaining_orientation_error_rad <= orientation_tolerance_rad;
            let safe = plan.safe;
            steps.push(plan);

            if converged || !safe {
                break;
            }
        }

        alerts.sort();
        alerts.dedup();

        if let Some(last_step) = steps.last() {
            let converged = last_step.remaining_translation_error_m <= translation_tolerance_m
                && last_step.remaining_orientation_error_rad <= orientation_tolerance_rad
                && last_step.safe;
            let execution = Self::build_execution_assessment(
                &[&last_step.projected_workspace],
                last_step.conditioning,
                Some(converged),
                &alerts,
            );
            Ok(TcpIkSolution {
                tcp_name: last_step.tcp_name.clone(),
                converged,
                iterations: steps.len(),
                final_joint_positions: working_joints,
                final_pose: last_step.projected_pose.clone(),
                final_workspace: last_step.projected_workspace.clone(),
                remaining_translation_error_m: last_step.remaining_translation_error_m,
                remaining_orientation_error_rad: last_step.remaining_orientation_error_rad,
                conditioning: last_step.conditioning,
                execution: execution.clone(),
                safe: execution.executable,
                alerts,
                steps,
            })
        } else {
            let snapshot = self.build_frame_snapshot_with_input(
                0,
                0,
                &FrameSnapshotInput {
                    joint_positions: self.joint_position_inputs(&working_joints),
                    ..FrameSnapshotInput::default()
                },
            );
            let tcp = self
                .model
                .get_tcp(tcp_name)
                .ok_or_else(|| format!("tcp `{tcp_name}` not found in embodiment model"))?;
            let final_pose = self.tcp_pose_in_snapshot(&snapshot, tcp, Some(relative_to))?;
            let final_workspace = self.evaluate_pose_workspace(&final_pose.transform, relative_to)?;
            let remaining_delta = final_pose.transform.inverse().compose(target_pose);
            let jacobian = self.compute_tcp_jacobian(tcp_name, &working_joints)?;
            let (_, min_singular_value, _, conditioning) = Self::jacobian_metrics(&jacobian);
            let execution = Self::build_execution_assessment(&[&final_workspace], conditioning, Some(false), &alerts);
            Ok(TcpIkSolution {
                tcp_name: tcp.name.clone(),
                converged: false,
                iterations: 0,
                final_joint_positions: working_joints,
                final_pose,
                final_workspace,
                remaining_translation_error_m: Self::vector3(remaining_delta.translation).norm(),
                remaining_orientation_error_rad: Self::scaled_axis(remaining_delta.rotation).norm(),
                conditioning: Self::classify_conditioning(min_singular_value),
                execution: execution.clone(),
                safe: execution.executable,
                alerts,
                steps,
            })
        }
    }

    /// Plan a multi-waypoint TCP trajectory by solving incremental IK targets.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn plan_tcp_trajectory(
        &self,
        tcp_name: &str,
        joint_positions: &[f64],
        target_pose: &Transform3D,
        relative_to: &str,
        waypoint_count: usize,
        max_joint_step: f64,
        max_iterations_per_waypoint: usize,
        translation_tolerance_m: f64,
        orientation_tolerance_rad: f64,
    ) -> Result<TcpTrajectoryPlan, String> {
        if waypoint_count == 0 {
            return Err("waypoint_count must be at least 1".into());
        }
        if waypoint_count > u32::MAX as usize {
            return Err("waypoint_count exceeds u32::MAX".into());
        }

        let snapshot = self.build_frame_snapshot_with_input(
            0,
            0,
            &FrameSnapshotInput {
                joint_positions: self.joint_position_inputs(joint_positions),
                ..FrameSnapshotInput::default()
            },
        );
        let tcp = self
            .model
            .get_tcp(tcp_name)
            .ok_or_else(|| format!("tcp `{tcp_name}` not found in embodiment model"))?;
        let start_pose = self.tcp_pose_in_snapshot(&snapshot, tcp, Some(relative_to))?;

        let mut working_joints: Vec<f64> = self
            .model
            .joints
            .iter()
            .enumerate()
            .map(|(index, _)| joint_positions.get(index).copied().unwrap_or(0.0))
            .collect();
        let mut waypoint_solutions = Vec::new();
        let mut waypoint_summaries = Vec::new();
        let mut joint_trajectory = Vec::new();
        let mut alerts = Vec::new();

        for index in 1..=waypoint_count {
            let alpha = f64::from(u32::try_from(index).expect("waypoint index exceeds u32::MAX"))
                / f64::from(u32::try_from(waypoint_count).expect("waypoint count exceeds u32::MAX"));
            let waypoint_pose = Self::interpolate_transform(&start_pose.transform, target_pose, alpha);
            let solution = self.solve_tcp_ik(
                tcp_name,
                &working_joints,
                &waypoint_pose,
                relative_to,
                max_joint_step,
                max_iterations_per_waypoint,
                translation_tolerance_m,
                orientation_tolerance_rad,
            )?;
            working_joints.clone_from(&solution.final_joint_positions);
            alerts.extend(solution.alerts.iter().cloned());
            waypoint_summaries.push(TrajectoryWaypointSummary {
                waypoint_index: index,
                converged: solution.converged,
                safe: solution.safe,
                execution: solution.execution.clone(),
                remaining_translation_error_m: solution.remaining_translation_error_m,
                remaining_orientation_error_rad: solution.remaining_orientation_error_rad,
            });
            joint_trajectory.push(PlannedJointTrajectorySample {
                waypoint_index: index,
                joint_positions: solution.final_joint_positions.clone(),
            });
            let stop = !solution.converged || !solution.safe;
            waypoint_solutions.push(solution);
            if stop {
                break;
            }
        }

        alerts.sort();
        alerts.dedup();

        let Some(last_solution) = waypoint_solutions.last() else {
            return Err("trajectory planning produced no waypoint solutions".into());
        };

        let worst_conditioning = waypoint_solutions
            .iter()
            .map(|solution| solution.conditioning)
            .min()
            .unwrap_or(JacobianConditioning::WellConditioned);
        let quality = TrajectoryQualitySummary {
            total_waypoints: waypoint_count,
            converged_waypoints: waypoint_solutions.iter().filter(|solution| solution.converged).count(),
            min_workspace_margin_m: waypoint_solutions
                .iter()
                .filter_map(|solution| solution.execution.min_workspace_margin_m)
                .reduce(f64::min),
            worst_conditioning,
            max_remaining_translation_error_m: waypoint_solutions
                .iter()
                .map(|solution| solution.remaining_translation_error_m)
                .fold(0.0, f64::max),
            max_remaining_orientation_error_rad: waypoint_solutions
                .iter()
                .map(|solution| solution.remaining_orientation_error_rad)
                .fold(0.0, f64::max),
        };
        let mut final_execution = Self::build_execution_assessment(
            &[&last_solution.final_workspace],
            worst_conditioning,
            Some(
                waypoint_solutions.len() == waypoint_count
                    && waypoint_solutions.iter().all(|solution| solution.converged),
            ),
            &alerts,
        );
        final_execution.min_workspace_margin_m = quality.min_workspace_margin_m;

        Ok(TcpTrajectoryPlan {
            tcp_name: tcp.name.clone(),
            relative_to: relative_to.to_string(),
            converged: waypoint_solutions.len() == waypoint_count
                && waypoint_solutions.iter().all(|solution| solution.converged),
            safe: final_execution.executable,
            execution: final_execution,
            quality,
            waypoint_summaries,
            joint_trajectory,
            final_joint_positions: working_joints,
            final_pose: last_solution.final_pose.clone(),
            final_workspace: last_solution.final_workspace.clone(),
            waypoint_solutions,
            alerts,
        })
    }

    /// Evaluate where a frame lies within the runtime workspace envelope.
    pub fn evaluate_workspace_frame(
        &self,
        snapshot: &FrameGraphSnapshot,
        frame_id: &str,
        relative_to: Option<&str>,
    ) -> Result<WorkspaceFrameEvaluation, String> {
        let pose = self.resolve_frame_pose(snapshot, frame_id, relative_to)?;
        let (projection_tree, _) = Self::overlay_snapshot_tree(snapshot);
        let mut matches = Vec::new();
        for zone in self.workspace_envelope().zones {
            if !projection_tree.frame_exists(&zone.origin_frame) {
                continue;
            }
            let point_in_zone = projection_tree
                .lookup_transform(&zone.origin_frame, frame_id)
                .map(|transform| transform.translation)
                .map_err(|error| {
                    format!(
                        "failed to evaluate frame `{frame_id}` in workspace zone `{}`: {error}",
                        zone.name
                    )
                })?;
            matches.push(WorkspaceFrameMatch {
                zone_name: zone.name,
                zone_type: zone.zone_type,
                contains_origin: zone.shape.contains_point(point_in_zone),
                point_in_zone,
            });
        }
        Ok(WorkspaceFrameEvaluation { pose, matches })
    }

    /// Evaluate an explicit planned pose against the runtime workspace envelope.
    pub fn evaluate_pose_workspace(
        &self,
        pose: &Transform3D,
        relative_to: &str,
    ) -> Result<WorkspacePoseEvaluation, String> {
        let envelope = self.workspace_envelope();
        if envelope.zones.is_empty() {
            return Ok(WorkspacePoseEvaluation {
                relative_to: relative_to.to_string(),
                pose: pose.clone(),
                safe: true,
                min_margin_m: None,
                in_human_presence: false,
                zone_checks: Vec::new(),
                alerts: Vec::new(),
            });
        }
        if !self.frame_graph.frame_exists(relative_to) {
            return Err(format!(
                "pose reference frame `{relative_to}` missing from embodiment runtime"
            ));
        }

        let has_allowed_zone = envelope
            .zones
            .iter()
            .any(|zone| matches!(zone.zone_type, super::workspace::ZoneType::Allowed));

        let mut alerts = Vec::new();
        let mut zone_checks = Vec::new();
        let mut allowed_margin = None;
        let mut restricted_clearance = None;
        let mut in_human_presence = false;
        let mut safe = true;

        for zone in &envelope.zones {
            if !self.frame_graph.frame_exists(&zone.origin_frame) {
                continue;
            }

            let zone_to_relative = self
                .frame_graph
                .lookup_transform(&zone.origin_frame, relative_to)
                .map_err(|error| {
                    format!(
                        "failed to evaluate pose relative to workspace zone `{}`: {error}",
                        zone.name
                    )
                })?;
            let zone_pose = zone_to_relative.compose(pose);
            let signed_margin = zone.shape.signed_margin(zone_pose.translation) - zone.margin_m;
            let contains_origin = signed_margin >= 0.0;
            zone_checks.push(WorkspaceZoneCheck {
                zone_name: zone.name.clone(),
                zone_type: zone.zone_type.clone(),
                signed_margin_m: signed_margin,
                contains_origin,
            });

            match zone.zone_type {
                super::workspace::ZoneType::Allowed => {
                    allowed_margin =
                        Some(allowed_margin.map_or(signed_margin, |margin: f64| margin.max(signed_margin)));
                }
                super::workspace::ZoneType::Restricted => {
                    let clearance = -signed_margin;
                    restricted_clearance =
                        Some(restricted_clearance.map_or(clearance, |margin: f64| margin.min(clearance)));
                }
                super::workspace::ZoneType::HumanPresence => {
                    if contains_origin {
                        in_human_presence = true;
                    }
                }
            }
        }

        if has_allowed_zone && !allowed_margin.is_some_and(|margin| margin >= 0.0) {
            alerts.push("planned pose lies outside every allowed workspace zone".into());
            safe = false;
        }
        if restricted_clearance.is_some_and(|margin| margin < 0.0) {
            alerts.push("planned pose entered a restricted workspace zone".into());
            safe = false;
        }
        if in_human_presence {
            alerts.push("planned pose is inside a human-presence workspace zone".into());
        }

        let min_margin_m = match (has_allowed_zone, allowed_margin, restricted_clearance) {
            (true, Some(allowed_margin), Some(restricted_clearance)) => Some(allowed_margin.min(restricted_clearance)),
            (true, Some(allowed_margin), None) => Some(allowed_margin),
            (true, None, Some(restricted_clearance)) | (false, _, Some(restricted_clearance)) => {
                Some(restricted_clearance)
            }
            _ => None,
        };

        Ok(WorkspacePoseEvaluation {
            relative_to: relative_to.to_string(),
            pose: pose.clone(),
            safe,
            min_margin_m,
            in_human_presence,
            zone_checks,
            alerts,
        })
    }

    /// Evaluate TCP workspace occupancy for a concrete joint-state sample.
    #[must_use]
    pub fn check_workspace(&self, joint_positions: &[f64]) -> WorkspaceCheckResult {
        let snapshot = self.build_frame_snapshot_with_input(
            0,
            0,
            &FrameSnapshotInput {
                joint_positions: self.joint_position_inputs(joint_positions),
                ..FrameSnapshotInput::default()
            },
        );
        self.check_workspace_snapshot(&snapshot)
    }

    /// Evaluate TCP workspace occupancy from an already-materialized snapshot.
    #[must_use]
    pub fn check_workspace_snapshot(&self, snapshot: &FrameGraphSnapshot) -> WorkspaceCheckResult {
        let envelope = self.workspace_envelope();
        let mut alerts = Vec::new();
        let mut frames = Vec::new();

        if self.model.tcps.is_empty() || envelope.zones.is_empty() {
            return WorkspaceCheckResult {
                safe: true,
                min_margin_m: None,
                frames,
                alerts,
            };
        }

        let has_allowed_zone = envelope
            .zones
            .iter()
            .any(|zone| matches!(zone.zone_type, super::workspace::ZoneType::Allowed));

        for tcp in &self.model.tcps {
            let mut zone_checks = Vec::new();
            let mut allowed_margin = None;
            let mut restricted_clearance = None;
            let mut in_human_presence = false;
            let mut frame_safe = true;

            for zone in &envelope.zones {
                let pose = match self.tcp_pose_in_snapshot(snapshot, tcp, Some(&zone.origin_frame)) {
                    Ok(pose) => pose,
                    Err(error) => {
                        alerts.push(format!(
                            "failed to evaluate tcp `{}` relative to workspace zone `{}`: {error}",
                            tcp.name, zone.name
                        ));
                        frame_safe = false;
                        continue;
                    }
                };

                let signed_margin = zone.shape.signed_margin(pose.transform.translation) - zone.margin_m;
                let contains_origin = signed_margin >= 0.0;
                zone_checks.push(WorkspaceZoneCheck {
                    zone_name: zone.name.clone(),
                    zone_type: zone.zone_type.clone(),
                    signed_margin_m: signed_margin,
                    contains_origin,
                });

                match zone.zone_type {
                    super::workspace::ZoneType::Allowed => {
                        allowed_margin =
                            Some(allowed_margin.map_or(signed_margin, |margin: f64| margin.max(signed_margin)));
                    }
                    super::workspace::ZoneType::Restricted => {
                        let clearance = -signed_margin;
                        restricted_clearance =
                            Some(restricted_clearance.map_or(clearance, |margin: f64| margin.min(clearance)));
                    }
                    super::workspace::ZoneType::HumanPresence => {
                        if contains_origin {
                            in_human_presence = true;
                        }
                    }
                }
            }

            if has_allowed_zone && !allowed_margin.is_some_and(|margin| margin >= 0.0) {
                alerts.push(format!("tcp `{}` lies outside every allowed workspace zone", tcp.name));
                frame_safe = false;
            }
            if restricted_clearance.is_some_and(|margin| margin < 0.0) {
                alerts.push(format!("tcp `{}` entered a restricted workspace zone", tcp.name));
                frame_safe = false;
            }
            if in_human_presence {
                alerts.push(format!("tcp `{}` is inside a human-presence workspace zone", tcp.name));
            }

            let margin_m = match (has_allowed_zone, allowed_margin, restricted_clearance) {
                (true, Some(allowed_margin), Some(restricted_clearance)) => {
                    Some(allowed_margin.min(restricted_clearance))
                }
                (true, Some(allowed_margin), None) => Some(allowed_margin),
                (true, None, Some(restricted_clearance)) | (false, _, Some(restricted_clearance)) => {
                    Some(restricted_clearance)
                }
                _ => None,
            };

            frames.push(WorkspaceCheckFrameResult {
                frame_id: tcp.name.clone(),
                margin_m,
                safe: frame_safe,
                in_human_presence,
                zone_checks,
            });
        }

        alerts.sort();
        alerts.dedup();

        WorkspaceCheckResult {
            safe: frames.iter().all(|frame| frame.safe),
            min_margin_m: frames
                .iter()
                .filter_map(|frame| frame.margin_m)
                .fold(None, |acc: Option<f64>, margin| {
                    Some(acc.map_or(margin, |current| current.min(margin)))
                }),
            frames,
            alerts,
        }
    }

    /// Build a runtime-owned tick-input projection from channel-aligned state and snapshot input.
    ///
    /// This keeps snapshot materialization, watched-pose projection, and derived-feature
    /// computation inside `EmbodimentRuntime`. Surfaces that still own transport-specific
    /// tick contract types can adapt this projection at the final boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn build_tick_input_projection(
        &self,
        tick: u64,
        monotonic_time_ns: u64,
        snapshot_timestamp_ns: u64,
        channel_names: &[String],
        positions: &[f64],
        velocities: &[f64],
        efforts: Option<&[f64]>,
        snapshot_input: &FrameSnapshotInput,
    ) -> TickInputProjection {
        let snapshot_input = if snapshot_input.joint_positions.is_empty() && !positions.is_empty() {
            FrameSnapshotInput {
                joint_positions: self.joint_positions_from_channel_values(positions),
                ..snapshot_input.clone()
            }
        } else {
            snapshot_input.clone()
        };
        let snapshot = self.build_frame_snapshot_with_input(tick, snapshot_timestamp_ns, &snapshot_input);
        let tick_projection = self.build_tick_projection_with_efforts(&snapshot, efforts);
        let joints = channel_names
            .iter()
            .enumerate()
            .map(|(index, name)| TickJointStateProjection {
                name: name.clone(),
                position: positions.get(index).copied().unwrap_or(0.0),
                velocity: velocities.get(index).copied().unwrap_or(0.0),
                effort: efforts.and_then(|values| values.get(index).copied()),
            })
            .collect();

        TickInputProjection {
            tick,
            monotonic_time_ns,
            snapshot,
            joints,
            watched_poses: tick_projection.watched_poses,
            features: tick_projection.features,
            validation_issues: tick_projection.validation_issues,
        }
    }

    /// Project a full snapshot into controller-facing watched poses and
    /// bounded derived features.
    #[must_use]
    pub fn build_tick_projection(&self, snapshot: &FrameGraphSnapshot) -> TickProjection {
        self.build_tick_projection_with_efforts(snapshot, None)
    }

    fn build_tick_projection_with_efforts(
        &self,
        snapshot: &FrameGraphSnapshot,
        efforts: Option<&[f64]>,
    ) -> TickProjection {
        let workspace_check = self.check_workspace_snapshot(snapshot);
        let (watched_poses, mut validation_issues) = self.project_watched_poses(snapshot);
        let (collision_margin, collision_issues) = self.collision_margin_for_snapshot(snapshot);
        validation_issues.extend(collision_issues);
        validation_issues.sort();
        validation_issues.dedup();

        TickProjection {
            watched_poses,
            features: TickDerivedFeaturesProjection {
                calibration_valid: self.calibration_valid(),
                workspace_margin: workspace_check.min_margin_m,
                collision_margin,
                force_margin: self.force_margin_from_efforts(efforts),
                observation_confidence: self.observation_confidence_for_snapshot(snapshot),
                active_perception_available: self.active_perception_available(),
                alerts: workspace_check.alerts,
            },
            validation_issues,
        }
    }

    /// Project watched frames from a snapshot into controller-facing poses.
    ///
    /// The returned poses are expressed relative to the snapshot root frame.
    /// Validation issues are returned separately so callers can surface them
    /// alongside the projected pose list.
    #[must_use]
    pub fn project_watched_poses(&self, snapshot: &FrameGraphSnapshot) -> (Vec<WatchedPoseProjection>, Vec<String>) {
        let reference_frame = snapshot
            .frame_tree
            .root()
            .or_else(|| self.frame_graph.root())
            .map(str::to_string);
        let Some(reference_frame) = reference_frame else {
            return (
                Vec::new(),
                vec!["frame snapshot missing root frame for watched-pose projection".into()],
            );
        };

        let (projection_tree, mut validation_issues) = Self::overlay_snapshot_tree(snapshot);

        let watched_frames = if snapshot.watched_frames.is_empty() {
            &self.watched_frames
        } else {
            &snapshot.watched_frames
        };

        let mut projections = Vec::new();
        for frame_id in watched_frames {
            if !projection_tree.frame_exists(frame_id) {
                validation_issues.push(format!("watched frame `{frame_id}` missing from snapshot frame tree"));
                continue;
            }
            match projection_tree.lookup_transform(&reference_frame, frame_id) {
                Ok(transform) => projections.push(WatchedPoseProjection {
                    frame_id: frame_id.clone(),
                    relative_to: reference_frame.clone(),
                    transform,
                    freshness: snapshot
                        .frame_freshness
                        .get(frame_id)
                        .cloned()
                        .unwrap_or(FreshnessState::Unknown),
                }),
                Err(error) => validation_issues.push(format!(
                    "failed to project watched frame `{frame_id}` from `{reference_frame}`: {error}"
                )),
            }
        }

        validation_issues.sort();
        validation_issues.dedup();
        (projections, validation_issues)
    }

    /// Project command-channel-indexed sensor values into canonical joint positions.
    #[must_use]
    pub fn joint_positions_from_channel_values(&self, channel_values: &[f64]) -> BTreeMap<String, f64> {
        let mut positions = BTreeMap::new();
        for binding in &self.model.channel_bindings {
            let Some(index) = usize::try_from(binding.channel_index).ok() else {
                continue;
            };
            let Some(value) = channel_values.get(index).copied() else {
                continue;
            };
            if self.model.get_joint(&binding.physical_name).is_some() {
                positions.insert(binding.physical_name.clone(), value);
            }
        }
        positions
    }

    /// Resolve the ordered root-to-leaf joint chain that influences a frame.
    pub fn joint_chain_for_frame(&self, frame_id: &str) -> Result<Vec<String>, String> {
        let Some(mut link_name) = self.link_frame_for_frame(frame_id) else {
            return Err(format!("frame `{frame_id}` does not map to a known link frame"));
        };
        let mut chain = Vec::new();
        let mut visited = BTreeSet::new();

        loop {
            if !visited.insert(link_name.clone()) {
                return Err(format!(
                    "cycle detected while resolving joint chain for frame `{frame_id}`"
                ));
            }
            let Some(link) = self.model.get_link(&link_name) else {
                return Err(format!("link `{link_name}` missing from embodiment model"));
            };
            let Some(parent_joint_name) = link.parent_joint.as_ref() else {
                break;
            };
            let Some(joint) = self.model.get_joint(parent_joint_name) else {
                return Err(format!(
                    "link `{}` references missing parent joint `{}`",
                    link.name, parent_joint_name
                ));
            };
            chain.push(joint.name.clone());
            link_name = joint.parent_link.clone();
        }

        chain.reverse();
        Ok(chain)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::embodiment::binding::{BindingType, ChannelBinding};
    use crate::embodiment::frame_tree::{FrameSource, FrameTree, Transform3D};
    use crate::embodiment::limits::JointSafetyLimits;
    use crate::embodiment::model::{
        CollisionBody, Geometry, Joint, JointType, Link, SensorMount, SensorType, TcpType, ToolCenterPoint,
    };
    use crate::embodiment::workspace::{WorkspaceShape, WorkspaceZone, ZoneType};

    fn simple_model() -> EmbodimentModel {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        tree.add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();

        let mut model = EmbodimentModel {
            model_id: "test-v1".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links: vec![Link {
                name: "base".into(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            }],
            joints: vec![Joint {
                name: "j0".into(),
                joint_type: JointType::Revolute,
                parent_link: "base".into(),
                child_link: "base".into(),
                axis: [0.0, 0.0, 1.0],
                origin: Transform3D::identity(),
                limits: JointSafetyLimits {
                    joint_name: "j0".into(),
                    max_velocity: 2.0,
                    max_acceleration: 5.0,
                    max_jerk: 50.0,
                    position_min: -3.14,
                    position_max: 3.14,
                    max_torque: None,
                },
            }],
            frame_tree: tree,
            collision_bodies: vec![],
            allowed_collision_pairs: vec![],
            tcps: vec![ToolCenterPoint {
                name: "tool0".into(),
                parent_link: "base".into(),
                offset: Transform3D::identity(),
                tcp_type: TcpType::Tool,
            }],
            sensor_mounts: vec![SensorMount {
                sensor_id: "cam0".into(),
                parent_link: "base".into(),
                offset: Transform3D::identity(),
                sensor_type: SensorType::Camera,
                is_actuated: false,
                actuation_joint: None,
                frustum: None,
            }],
            workspace_zones: vec![WorkspaceZone {
                name: "safe".into(),
                shape: WorkspaceShape::Sphere { radius: 1.0 },
                origin_frame: "world".into(),
                zone_type: ZoneType::Allowed,
                margin_m: 0.1,
            }],
            watched_frames: vec!["world".into(), "base".into()],
            channel_bindings: vec![ChannelBinding {
                physical_name: "j0".into(),
                channel_index: 0,
                binding_type: BindingType::JointVelocity,
                frame_id: "base".into(),
                units: "rad/s".into(),
                semantic_role: None,
            }],
        };
        model.stamp_digest();
        model
    }

    fn model_with_computed_watch_frame() -> EmbodimentModel {
        let mut model = simple_model();
        model
            .frame_tree
            .add_frame("fk_tip", "base", Transform3D::identity(), FrameSource::Computed)
            .unwrap();
        model.watched_frames.push("fk_tip".into());
        model.channel_bindings.push(ChannelBinding {
            physical_name: "tip_watch".into(),
            channel_index: 1,
            binding_type: BindingType::Command,
            frame_id: "fk_tip".into(),
            units: "m".into(),
            semantic_role: None,
        });
        model.stamp_digest();
        model
    }

    fn articulated_model() -> EmbodimentModel {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        tree.add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();
        tree.add_frame("arm", "base", Transform3D::identity(), FrameSource::Computed)
            .unwrap();

        let mut model = EmbodimentModel {
            model_id: "articulated-v1".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links: vec![
                Link {
                    name: "base".into(),
                    parent_joint: None,
                    inertial: None,
                    visual_geometry: None,
                    collision_geometry: None,
                },
                Link {
                    name: "arm".into(),
                    parent_joint: Some("slide_y".into()),
                    inertial: None,
                    visual_geometry: None,
                    collision_geometry: None,
                },
            ],
            joints: vec![Joint {
                name: "slide_y".into(),
                joint_type: JointType::Prismatic,
                parent_link: "base".into(),
                child_link: "arm".into(),
                axis: [0.0, 1.0, 0.0],
                origin: Transform3D::identity(),
                limits: JointSafetyLimits {
                    joint_name: "slide_y".into(),
                    max_velocity: 1.0,
                    max_acceleration: 3.0,
                    max_jerk: 20.0,
                    position_min: -1.0,
                    position_max: 1.0,
                    max_torque: None,
                },
            }],
            frame_tree: tree,
            collision_bodies: vec![],
            allowed_collision_pairs: vec![],
            tcps: vec![],
            sensor_mounts: vec![],
            workspace_zones: vec![],
            watched_frames: vec!["world".into(), "base".into(), "arm".into()],
            channel_bindings: vec![ChannelBinding {
                physical_name: "slide_y".into(),
                channel_index: 0,
                binding_type: BindingType::JointVelocity,
                frame_id: "arm".into(),
                units: "m/s".into(),
                semantic_role: None,
            }],
        };
        model.stamp_digest();
        model
    }

    fn revolute_tcp_model() -> EmbodimentModel {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        tree.add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();
        tree.add_frame("arm", "base", Transform3D::identity(), FrameSource::Computed)
            .unwrap();

        let mut model = EmbodimentModel {
            model_id: "revolute-v1".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links: vec![
                Link {
                    name: "base".into(),
                    parent_joint: None,
                    inertial: None,
                    visual_geometry: None,
                    collision_geometry: None,
                },
                Link {
                    name: "arm".into(),
                    parent_joint: Some("yaw".into()),
                    inertial: None,
                    visual_geometry: None,
                    collision_geometry: None,
                },
            ],
            joints: vec![Joint {
                name: "yaw".into(),
                joint_type: JointType::Revolute,
                parent_link: "base".into(),
                child_link: "arm".into(),
                axis: [0.0, 0.0, 1.0],
                origin: Transform3D::identity(),
                limits: JointSafetyLimits {
                    joint_name: "yaw".into(),
                    max_velocity: 2.0,
                    max_acceleration: 5.0,
                    max_jerk: 50.0,
                    position_min: -3.14,
                    position_max: 3.14,
                    max_torque: None,
                },
            }],
            frame_tree: tree,
            collision_bodies: vec![],
            allowed_collision_pairs: vec![],
            tcps: vec![ToolCenterPoint {
                name: "tool0".into(),
                parent_link: "arm".into(),
                offset: Transform3D {
                    translation: [1.0, 0.0, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                tcp_type: TcpType::Tool,
            }],
            sensor_mounts: vec![],
            workspace_zones: vec![],
            watched_frames: vec!["world".into(), "base".into(), "arm".into()],
            channel_bindings: vec![],
        };
        model.stamp_digest();
        model
    }

    fn articulated_model_with_actuated_sensor() -> EmbodimentModel {
        let mut model = articulated_model();
        model.sensor_mounts.push(SensorMount {
            sensor_id: "cam1".into(),
            parent_link: "arm".into(),
            offset: Transform3D {
                translation: [0.1, 0.0, 0.0],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            sensor_type: SensorType::Camera,
            is_actuated: true,
            actuation_joint: Some("slide_y".into()),
            frustum: Some(crate::embodiment::model::CameraFrustum {
                fov_horizontal_deg: 90.0,
                fov_vertical_deg: 60.0,
                near_clip_m: 0.1,
                far_clip_m: 10.0,
                resolution: Some((640, 480)),
            }),
        });
        model.stamp_digest();
        model
    }

    #[test]
    fn compile_no_overlays() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model.clone(), None, None);
        assert!(!rt.combined_digest.is_empty());
        assert_eq!(rt.combined_digest.len(), 64);
        assert!(rt.calibration.is_none());
        assert!(rt.safety_overlay.is_none());
        assert_eq!(rt.model.model_id, "test-v1");
        assert_eq!(rt.model_digest, rt.model.model_digest);
        assert_eq!(rt.calibration_digest, "none");
        assert_eq!(rt.safety_digest, "none");
        assert_eq!(rt.joint_count, 1);
        assert_eq!(rt.tcp_count, 1);
        assert!(rt.frame_graph.frame_exists("world"));
        assert!(rt.watched_frames.iter().any(|frame| frame == "world"));
        assert!(rt.validation_issues.is_empty());
    }

    #[test]
    fn compile_legacy_model_infers_watched_frames_and_records_issue() {
        let mut model = simple_model();
        model.watched_frames.clear();

        let rt = EmbodimentRuntime::compile(model, None, None);

        assert!(rt.uses_legacy_watched_frame_inference());
        assert!(rt.watched_frames.iter().any(|frame| frame == "world"));
        assert!(
            rt.validation_issues
                .iter()
                .any(|issue| issue.contains("watched frames inferred from legacy model fields"))
        );
    }

    #[test]
    fn compile_digest_changes_with_calibration() {
        let model = simple_model();
        let rt_none = EmbodimentRuntime::compile(model.clone(), None, None);

        let mut cal = CalibrationOverlay {
            calibration_id: "cal-1".into(),
            calibration_digest: String::new(),
            calibrated_at: chrono::Utc::now(),
            stale_after: None,
            joint_offsets: Default::default(),
            frame_corrections: Default::default(),
            sensor_calibrations: Default::default(),
            temperature_range: None,
            valid_for_model_digest: model.model_digest.clone(),
        };
        cal.stamp_digest();
        let rt_cal = EmbodimentRuntime::compile(model, Some(cal), None);
        assert_ne!(rt_none.combined_digest, rt_cal.combined_digest);
    }

    #[test]
    fn build_frame_snapshot_contains_tree() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let snap = rt.build_frame_snapshot();
        assert!(snap.frame_tree.frame_exists("world"));
        assert!(snap.frame_tree.frame_exists("base"));
        assert_eq!(snap.clock_domain, ClockDomain::Monotonic);
        assert_eq!(snap.model_digest, rt.model_digest);
        assert_eq!(snap.watched_frames, rt.watched_frames);
        assert_eq!(snap.validation_issues, rt.validation_issues);
        assert!(snap.frame_freshness.contains_key("world"));
    }

    #[test]
    fn compile_applies_valid_calibration_frame_corrections() {
        let model = simple_model();
        let corrected = Transform3D {
            translation: [0.2, 0.0, 0.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 42,
        };
        let mut calibration = CalibrationOverlay {
            calibration_id: "cal-1".into(),
            calibration_digest: String::new(),
            calibrated_at: chrono::Utc::now(),
            stale_after: None,
            joint_offsets: Default::default(),
            frame_corrections: std::collections::BTreeMap::from([("base".into(), corrected.clone())]),
            sensor_calibrations: Default::default(),
            temperature_range: None,
            valid_for_model_digest: model.model_digest.clone(),
        };
        calibration.stamp_digest();

        let rt = EmbodimentRuntime::compile(model, Some(calibration.clone()), None);
        let base = rt.frame_graph.get_frame("base").expect("compiled base frame");
        assert_eq!(base.static_transform, corrected);
        assert_eq!(rt.active_calibration_id.as_deref(), Some("cal-1"));
        assert_eq!(rt.calibration_digest, calibration.calibration_digest);
        assert!(rt.watched_frames.iter().any(|frame| frame == "base"));
    }

    #[test]
    fn compile_invalid_calibration_is_reported_and_not_applied() {
        let mut model = simple_model();
        model.model_digest = "stale-model-digest".into();
        let corrected = Transform3D {
            translation: [0.9, 0.0, 0.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 7,
        };
        let calibration = CalibrationOverlay {
            calibration_id: "cal-invalid".into(),
            calibration_digest: String::new(),
            calibrated_at: chrono::Utc::now(),
            stale_after: None,
            joint_offsets: Default::default(),
            frame_corrections: std::collections::BTreeMap::from([("base".into(), corrected)]),
            sensor_calibrations: Default::default(),
            temperature_range: None,
            valid_for_model_digest: "wrong-model".into(),
        };

        let rt = EmbodimentRuntime::compile(model, Some(calibration), None);
        let base = rt.frame_graph.get_frame("base").expect("compiled base frame");
        assert_eq!(base.static_transform, Transform3D::identity());
        assert_eq!(rt.calibration_digest, "none");
        assert!(rt.active_calibration_id.is_none());
        assert!(
            rt.validation_issues
                .iter()
                .any(|issue| issue.contains("calibration `cal-invalid` was not applied"))
        );
        assert!(
            rt.validation_issues
                .iter()
                .any(|issue| issue.contains("model digest was normalized at runtime"))
        );
    }

    #[test]
    fn build_frame_snapshot_tracks_dynamic_transforms() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let dynamic = Transform3D {
            translation: [0.0, 1.0, 0.0],
            rotation: [1.0, 0.0, 0.0, 0.0],
            timestamp_ns: 55,
        };
        let snap = rt.build_frame_snapshot_with_dynamic_transforms(9, 55, &[("base".into(), dynamic.clone())]);
        assert_eq!(snap.snapshot_id, 9);
        assert_eq!(snap.timestamp_ns, 55);
        assert_eq!(snap.clock_domain, ClockDomain::Monotonic);
        assert_eq!(snap.dynamic_transforms.len(), 1);
        assert_eq!(snap.dynamic_transforms[0].transform, dynamic);
        assert_eq!(snap.frame_freshness.get("base"), Some(&FreshnessState::Fresh));
        assert!(snap.sources.iter().any(|source| matches!(source, FrameSource::Static)));
        assert_eq!(snap.freshness, FreshnessState::Fresh);
    }

    #[test]
    fn build_frame_snapshot_with_input_preserves_explicit_provenance_and_anchors() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let snap = rt.build_frame_snapshot_with_input(
            11,
            100,
            &FrameSnapshotInput {
                clock_domain: ClockDomain::SensorClock,
                joint_positions: BTreeMap::new(),
                dynamic_transforms: vec![TimestampedTransform {
                    frame_id: "base".into(),
                    parent_id: Some("world".into()),
                    transform: Transform3D {
                        translation: [0.0, 0.25, 0.0],
                        rotation: [1.0, 0.0, 0.0, 0.0],
                        timestamp_ns: 90,
                    },
                    freshness: FreshnessState::Fresh,
                    source: FrameSource::Dynamic,
                }],
                world_anchors: vec![WorldAnchor {
                    anchor_id: "anchor-1".into(),
                    frame_id: "world".into(),
                    transform: Transform3D::identity(),
                    source: "slam".into(),
                    confidence: 0.9,
                }],
                validation_issues: vec!["external replay anchor injected".into()],
            },
        );

        assert_eq!(snap.snapshot_id, 11);
        assert_eq!(snap.clock_domain, ClockDomain::SensorClock);
        assert_eq!(snap.dynamic_transforms.len(), 1);
        assert_eq!(snap.dynamic_transforms[0].source, FrameSource::Dynamic);
        assert_eq!(snap.world_anchors.len(), 1);
        assert!(
            snap.validation_issues
                .iter()
                .any(|issue| issue.contains("external replay anchor injected"))
        );
    }

    #[test]
    fn project_watched_poses_uses_snapshot_dynamic_overrides() {
        let model = model_with_computed_watch_frame();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let snapshot = rt.build_frame_snapshot_with_input(
            5,
            100,
            &FrameSnapshotInput {
                clock_domain: ClockDomain::Monotonic,
                joint_positions: BTreeMap::new(),
                dynamic_transforms: vec![TimestampedTransform {
                    frame_id: "fk_tip".into(),
                    parent_id: Some("base".into()),
                    transform: Transform3D {
                        translation: [0.0, 0.75, 0.0],
                        rotation: [1.0, 0.0, 0.0, 0.0],
                        timestamp_ns: 100,
                    },
                    freshness: FreshnessState::Fresh,
                    source: FrameSource::Computed,
                }],
                world_anchors: Vec::new(),
                validation_issues: Vec::new(),
            },
        );

        let (projections, issues) = rt.project_watched_poses(&snapshot);
        assert!(issues.is_empty(), "unexpected projection issues: {issues:?}");
        let fk_tip = projections
            .iter()
            .find(|projection| projection.frame_id == "fk_tip")
            .expect("fk_tip should be projected");
        assert_eq!(fk_tip.relative_to, "world");
        assert_eq!(fk_tip.transform.translation, [0.0, 0.75, 0.0]);
        assert_eq!(fk_tip.freshness, FreshnessState::Fresh);
    }

    #[test]
    fn build_frame_snapshot_with_input_applies_joint_state_fk() {
        let rt = EmbodimentRuntime::compile(articulated_model(), None, None);
        let snapshot = rt.build_frame_snapshot_with_input(
            8,
            100,
            &FrameSnapshotInput {
                clock_domain: ClockDomain::Monotonic,
                joint_positions: BTreeMap::from([("slide_y".into(), 0.4)]),
                dynamic_transforms: Vec::new(),
                world_anchors: Vec::new(),
                validation_issues: Vec::new(),
            },
        );

        let pose = rt
            .resolve_frame_pose(&snapshot, "arm", Some("world"))
            .expect("arm pose should resolve from joint-state FK");

        assert_eq!(pose.transform.translation, [0.0, 0.4, 0.0]);
        assert_eq!(snapshot.frame_freshness.get("arm"), Some(&FreshnessState::Fresh));
        assert!(
            snapshot
                .dynamic_transforms
                .iter()
                .any(|transform| transform.frame_id == "arm" && transform.source == FrameSource::Computed)
        );
    }

    #[test]
    fn explicit_dynamic_transform_overrides_joint_state_fk() {
        let rt = EmbodimentRuntime::compile(articulated_model(), None, None);
        let snapshot = rt.build_frame_snapshot_with_input(
            9,
            100,
            &FrameSnapshotInput {
                clock_domain: ClockDomain::Monotonic,
                joint_positions: BTreeMap::from([("slide_y".into(), 0.4)]),
                dynamic_transforms: vec![TimestampedTransform {
                    frame_id: "arm".into(),
                    parent_id: Some("base".into()),
                    transform: Transform3D {
                        translation: [0.0, 0.9, 0.0],
                        rotation: [1.0, 0.0, 0.0, 0.0],
                        timestamp_ns: 100,
                    },
                    freshness: FreshnessState::Fresh,
                    source: FrameSource::Dynamic,
                }],
                world_anchors: Vec::new(),
                validation_issues: Vec::new(),
            },
        );

        let pose = rt
            .resolve_frame_pose(&snapshot, "arm", Some("world"))
            .expect("explicit dynamic transform should resolve");

        assert_eq!(pose.transform.translation, [0.0, 0.9, 0.0]);
        assert_eq!(snapshot.frame_freshness.get("arm"), Some(&FreshnessState::Fresh));
    }

    #[test]
    fn joint_chain_for_frame_returns_root_to_leaf_joints() {
        let rt = EmbodimentRuntime::compile(articulated_model(), None, None);
        let chain = rt.joint_chain_for_frame("arm").expect("joint chain should resolve");
        assert_eq!(chain, vec!["slide_y".to_string()]);
    }

    #[test]
    fn joint_positions_from_channel_values_uses_channel_bindings() {
        let rt = EmbodimentRuntime::compile(articulated_model(), None, None);
        let positions = rt.joint_positions_from_channel_values(&[0.6, 9.9]);
        assert_eq!(positions.get("slide_y"), Some(&0.6));
        assert_eq!(positions.len(), 1);
    }

    #[test]
    fn resolve_frame_pose_uses_snapshot_dynamic_overrides() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let snapshot = rt.build_frame_snapshot_with_input(
            12,
            100,
            &FrameSnapshotInput {
                clock_domain: ClockDomain::Monotonic,
                joint_positions: BTreeMap::new(),
                dynamic_transforms: vec![TimestampedTransform {
                    frame_id: "base".into(),
                    parent_id: Some("world".into()),
                    transform: Transform3D {
                        translation: [0.25, 0.0, 0.0],
                        rotation: [1.0, 0.0, 0.0, 0.0],
                        timestamp_ns: 100,
                    },
                    freshness: FreshnessState::Fresh,
                    source: FrameSource::Dynamic,
                }],
                world_anchors: Vec::new(),
                validation_issues: Vec::new(),
            },
        );

        let pose = rt
            .resolve_frame_pose(&snapshot, "base", Some("world"))
            .expect("base pose should resolve");

        assert_eq!(pose.frame_id, "base");
        assert_eq!(pose.relative_to, "world");
        assert_eq!(pose.transform.translation, [0.25, 0.0, 0.0]);
        assert_eq!(pose.freshness, FreshnessState::Fresh);
    }

    #[test]
    fn workspace_envelope_merges_model_and_safety_overlay_zones() {
        let model = simple_model();
        let overlay = SafetyOverlay {
            overlay_digest: "overlay-1".into(),
            workspace_restrictions: vec![WorkspaceZone {
                name: "restricted".into(),
                shape: WorkspaceShape::Box {
                    half_extents: [0.5, 0.5, 0.5],
                },
                origin_frame: "world".into(),
                zone_type: ZoneType::Restricted,
                margin_m: 0.0,
            }],
            joint_limit_overrides: Default::default(),
            max_payload_kg: None,
            human_presence_zones: vec![WorkspaceZone {
                name: "human".into(),
                shape: WorkspaceShape::Sphere { radius: 0.4 },
                origin_frame: "world".into(),
                zone_type: ZoneType::HumanPresence,
                margin_m: 0.0,
            }],
            force_limits: None,
            contact_force_envelopes: Vec::new(),
            contact_allowed_zones: Vec::new(),
            force_rate_limits: Default::default(),
        };
        let rt = EmbodimentRuntime::compile(model, None, Some(overlay));

        let envelope = rt.workspace_envelope();
        let zone_names: Vec<&str> = envelope.zones.iter().map(|zone| zone.name.as_str()).collect();

        assert!(zone_names.contains(&"safe"));
        assert!(zone_names.contains(&"restricted"));
        assert!(zone_names.contains(&"human"));
    }

    #[test]
    fn evaluate_workspace_frame_reports_membership() {
        let model = simple_model();
        let overlay = SafetyOverlay {
            overlay_digest: "overlay-2".into(),
            workspace_restrictions: Vec::new(),
            joint_limit_overrides: Default::default(),
            max_payload_kg: None,
            human_presence_zones: vec![WorkspaceZone {
                name: "human".into(),
                shape: WorkspaceShape::Sphere { radius: 0.5 },
                origin_frame: "world".into(),
                zone_type: ZoneType::HumanPresence,
                margin_m: 0.0,
            }],
            force_limits: None,
            contact_force_envelopes: Vec::new(),
            contact_allowed_zones: Vec::new(),
            force_rate_limits: Default::default(),
        };
        let rt = EmbodimentRuntime::compile(model, None, Some(overlay));
        let snapshot = rt.build_frame_snapshot_with_input(
            13,
            100,
            &FrameSnapshotInput {
                clock_domain: ClockDomain::Monotonic,
                joint_positions: BTreeMap::new(),
                dynamic_transforms: vec![TimestampedTransform {
                    frame_id: "base".into(),
                    parent_id: Some("world".into()),
                    transform: Transform3D {
                        translation: [0.2, 0.0, 0.0],
                        rotation: [1.0, 0.0, 0.0, 0.0],
                        timestamp_ns: 100,
                    },
                    freshness: FreshnessState::Fresh,
                    source: FrameSource::Dynamic,
                }],
                world_anchors: Vec::new(),
                validation_issues: Vec::new(),
            },
        );

        let evaluation = rt
            .evaluate_workspace_frame(&snapshot, "base", Some("world"))
            .expect("workspace evaluation should succeed");

        assert_eq!(evaluation.pose.transform.translation, [0.2, 0.0, 0.0]);
        assert!(
            evaluation
                .matches
                .iter()
                .any(|entry| entry.zone_name == "safe" && entry.contains_origin)
        );
        assert!(
            evaluation
                .matches
                .iter()
                .any(|entry| entry.zone_name == "human" && entry.contains_origin)
        );
    }

    #[test]
    fn compute_tcp_pose_uses_joint_state_and_tcp_offset() {
        let mut model = articulated_model();
        model.tcps.push(ToolCenterPoint {
            name: "tool0".into(),
            parent_link: "arm".into(),
            offset: Transform3D {
                translation: [0.0, 0.0, 0.2],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            tcp_type: TcpType::Tool,
        });
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let pose = rt
            .compute_tcp_pose("tool0", &[0.4])
            .expect("tcp pose should resolve from joint state");

        assert_eq!(pose.translation, [0.0, 0.4, 0.2]);
    }

    #[test]
    fn compute_tcp_jacobian_for_prismatic_joint_is_linear_axis_only() {
        let mut model = articulated_model();
        model.tcps.push(ToolCenterPoint {
            name: "tool0".into(),
            parent_link: "arm".into(),
            offset: Transform3D::identity(),
            tcp_type: TcpType::Tool,
        });
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let jacobian = rt
            .compute_tcp_jacobian("tool0", &[0.4])
            .expect("jacobian should resolve");

        assert_eq!(jacobian.nrows(), 6);
        assert_eq!(jacobian.ncols(), 1);
        assert!((jacobian[(0, 0)] - 0.0).abs() < f64::EPSILON);
        assert!((jacobian[(1, 0)] - 1.0).abs() < f64::EPSILON);
        assert!((jacobian[(2, 0)] - 0.0).abs() < f64::EPSILON);
        assert!((jacobian[(3, 0)] - 0.0).abs() < f64::EPSILON);
        assert!((jacobian[(4, 0)] - 0.0).abs() < f64::EPSILON);
        assert!((jacobian[(5, 0)] - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluate_tcp_reachability_combines_workspace_and_jacobian_conditioning() {
        let mut model = articulated_model();
        model.tcps.push(ToolCenterPoint {
            name: "tool0".into(),
            parent_link: "arm".into(),
            offset: Transform3D::identity(),
            tcp_type: TcpType::Tool,
        });
        model.workspace_zones.push(WorkspaceZone {
            name: "safe".into(),
            shape: WorkspaceShape::Sphere { radius: 1.0 },
            origin_frame: "world".into(),
            zone_type: ZoneType::Allowed,
            margin_m: 0.0,
        });
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let evaluation = rt
            .evaluate_tcp_reachability(
                "tool0",
                &[0.2],
                &Transform3D {
                    translation: [0.0, 0.6, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                "world",
            )
            .expect("tcp reachability should evaluate");

        assert!(evaluation.workspace.safe);
        assert!(evaluation.well_conditioned);
        assert!(evaluation.safe);
        assert_eq!(evaluation.conditioning, JacobianConditioning::WellConditioned);
        assert!(evaluation.execution.executable);
        assert!(evaluation.execution.degradation_reasons.is_empty());
        assert!((evaluation.translation_error_m - 0.4).abs() < 1e-9);
        assert_eq!(evaluation.jacobian_rank, 1);
        assert_eq!(evaluation.min_singular_value, Some(1.0));
        assert_eq!(evaluation.current_pose.transform.translation, [0.0, 0.2, 0.0]);
        assert_eq!(evaluation.target_pose.transform.translation, [0.0, 0.6, 0.0]);
    }

    #[test]
    fn plan_tcp_step_moves_toward_target_with_bounded_joint_delta() {
        let mut model = articulated_model();
        model.tcps.push(ToolCenterPoint {
            name: "tool0".into(),
            parent_link: "arm".into(),
            offset: Transform3D::identity(),
            tcp_type: TcpType::Tool,
        });
        model.workspace_zones.push(WorkspaceZone {
            name: "safe".into(),
            shape: WorkspaceShape::Sphere { radius: 1.0 },
            origin_frame: "world".into(),
            zone_type: ZoneType::Allowed,
            margin_m: 0.0,
        });
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let plan = rt
            .plan_tcp_step(
                "tool0",
                &[0.2],
                &Transform3D {
                    translation: [0.0, 0.6, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                "world",
                0.25,
            )
            .expect("tcp step planning should succeed");

        assert!(plan.safe);
        assert_eq!(plan.conditioning, JacobianConditioning::WellConditioned);
        assert!(plan.execution.executable);
        assert!(plan.target_workspace.safe);
        assert!(plan.projected_workspace.safe);
        assert_eq!(plan.jacobian_rank, 1);
        assert_eq!(plan.min_singular_value, Some(1.0));
        assert!((plan.proposed_joint_positions[0] - 0.45).abs() < 1e-6);
        assert_eq!(plan.current_pose.transform.translation, [0.0, 0.2, 0.0]);
        assert_eq!(plan.projected_pose.transform.translation, [0.0, 0.45, 0.0]);
        assert!((plan.remaining_translation_error_m - 0.15).abs() < 1e-6);
        assert!((plan.remaining_orientation_error_rad - 0.0).abs() < 1e-9);
    }

    #[test]
    fn solve_tcp_ik_converges_for_prismatic_target() {
        let mut model = articulated_model();
        model.tcps.push(ToolCenterPoint {
            name: "tool0".into(),
            parent_link: "arm".into(),
            offset: Transform3D::identity(),
            tcp_type: TcpType::Tool,
        });
        model.workspace_zones.push(WorkspaceZone {
            name: "safe".into(),
            shape: WorkspaceShape::Sphere { radius: 1.0 },
            origin_frame: "world".into(),
            zone_type: ZoneType::Allowed,
            margin_m: 0.0,
        });
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let solution = rt
            .solve_tcp_ik(
                "tool0",
                &[0.2],
                &Transform3D {
                    translation: [0.0, 0.6, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                "world",
                0.25,
                4,
                1e-6,
                1e-6,
            )
            .expect("ik solve should succeed");

        assert!(solution.converged);
        assert!(solution.safe);
        assert_eq!(solution.conditioning, JacobianConditioning::WellConditioned);
        assert!(solution.execution.executable);
        assert_eq!(solution.iterations, 2);
        assert_eq!(solution.steps.len(), 2);
        assert!((solution.final_joint_positions[0] - 0.6).abs() < 1e-6);
        assert!((solution.final_pose.transform.translation[0] - 0.0).abs() < 1e-9);
        assert!((solution.final_pose.transform.translation[1] - 0.6).abs() < 1e-6);
        assert!((solution.final_pose.transform.translation[2] - 0.0).abs() < 1e-9);
        assert!(solution.remaining_translation_error_m <= 1e-6);
        assert!(solution.remaining_orientation_error_rad <= 1e-6);
    }

    #[test]
    fn plan_tcp_trajectory_converges_across_waypoints() {
        let mut model = articulated_model();
        model.tcps.push(ToolCenterPoint {
            name: "tool0".into(),
            parent_link: "arm".into(),
            offset: Transform3D::identity(),
            tcp_type: TcpType::Tool,
        });
        model.workspace_zones.push(WorkspaceZone {
            name: "safe".into(),
            shape: WorkspaceShape::Sphere { radius: 1.0 },
            origin_frame: "world".into(),
            zone_type: ZoneType::Allowed,
            margin_m: 0.0,
        });
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let trajectory = rt
            .plan_tcp_trajectory(
                "tool0",
                &[0.2],
                &Transform3D {
                    translation: [0.0, 0.8, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                "world",
                3,
                0.25,
                4,
                1e-6,
                1e-6,
            )
            .expect("trajectory planning should succeed");

        assert!(trajectory.converged);
        assert!(trajectory.safe);
        assert!(trajectory.execution.executable);
        assert_eq!(trajectory.quality.total_waypoints, 3);
        assert_eq!(trajectory.quality.converged_waypoints, 3);
        assert_eq!(
            trajectory.quality.worst_conditioning,
            JacobianConditioning::WellConditioned
        );
        assert_eq!(trajectory.waypoint_summaries.len(), 3);
        assert_eq!(trajectory.joint_trajectory.len(), 3);
        assert_eq!(trajectory.waypoint_solutions.len(), 3);
        assert!(trajectory.waypoint_solutions.iter().all(|solution| solution.converged));
        assert!((trajectory.final_joint_positions[0] - 0.8).abs() < 1e-6);
        assert!((trajectory.final_pose.transform.translation[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn compute_jacobian_for_primary_tcp_uses_revolute_chain() {
        let rt = EmbodimentRuntime::compile(revolute_tcp_model(), None, None);

        let jacobian = rt.compute_jacobian(&[0.0]).expect("jacobian should resolve");

        assert_eq!(jacobian.nrows(), 6);
        assert_eq!(jacobian.ncols(), 1);
        assert!((jacobian[(0, 0)] - 0.0).abs() < 1e-9);
        assert!((jacobian[(1, 0)] - 1.0).abs() < 1e-9);
        assert!((jacobian[(2, 0)] - 0.0).abs() < 1e-9);
        assert!((jacobian[(3, 0)] - 0.0).abs() < 1e-9);
        assert!((jacobian[(4, 0)] - 0.0).abs() < 1e-9);
        assert!((jacobian[(5, 0)] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn sensor_joint_chain_and_pose_use_sensor_actuation_joint() {
        let rt = EmbodimentRuntime::compile(articulated_model_with_actuated_sensor(), None, None);

        let chain = rt.sensor_joint_chain("cam1").expect("sensor chain should resolve");
        let pose = rt
            .compute_sensor_pose("cam1", &[0.4])
            .expect("sensor pose should resolve");

        assert_eq!(chain, vec!["slide_y".to_string()]);
        assert_eq!(pose.translation, [0.1, 0.4, 0.0]);
    }

    #[test]
    fn evaluate_sensor_reachability_reports_workspace_safety() {
        let model = simple_model();
        let overlay = SafetyOverlay {
            overlay_digest: "overlay-sensor".into(),
            workspace_restrictions: vec![WorkspaceZone {
                name: "restricted".into(),
                shape: WorkspaceShape::Sphere { radius: 0.2 },
                origin_frame: "world".into(),
                zone_type: ZoneType::Restricted,
                margin_m: 0.0,
            }],
            joint_limit_overrides: Default::default(),
            max_payload_kg: None,
            human_presence_zones: vec![WorkspaceZone {
                name: "human".into(),
                shape: WorkspaceShape::Sphere { radius: 0.3 },
                origin_frame: "world".into(),
                zone_type: ZoneType::HumanPresence,
                margin_m: 0.0,
            }],
            force_limits: None,
            contact_force_envelopes: Vec::new(),
            contact_allowed_zones: Vec::new(),
            force_rate_limits: Default::default(),
        };
        let rt = EmbodimentRuntime::compile(model, None, Some(overlay));

        let reachability = rt
            .evaluate_sensor_reachability("cam0", &[])
            .expect("sensor reachability should resolve");

        assert!(!reachability.actuated);
        assert!(!reachability.has_frustum);
        assert!(!reachability.workspace_safe);
        assert!(reachability.in_human_presence);
        assert!(reachability.min_margin_m.is_some_and(|margin| margin < 0.0));
        assert!(
            reachability
                .alerts
                .iter()
                .any(|alert| alert.contains("restricted zone"))
        );
    }

    #[test]
    fn evaluate_pose_workspace_reports_restricted_and_human_zone_alerts() {
        let model = simple_model();
        let overlay = SafetyOverlay {
            overlay_digest: "overlay-pose".into(),
            workspace_restrictions: vec![WorkspaceZone {
                name: "restricted".into(),
                shape: WorkspaceShape::Sphere { radius: 0.2 },
                origin_frame: "world".into(),
                zone_type: ZoneType::Restricted,
                margin_m: 0.0,
            }],
            joint_limit_overrides: Default::default(),
            max_payload_kg: None,
            human_presence_zones: vec![WorkspaceZone {
                name: "human".into(),
                shape: WorkspaceShape::Sphere { radius: 0.3 },
                origin_frame: "world".into(),
                zone_type: ZoneType::HumanPresence,
                margin_m: 0.0,
            }],
            force_limits: None,
            contact_force_envelopes: Vec::new(),
            contact_allowed_zones: Vec::new(),
            force_rate_limits: Default::default(),
        };
        let rt = EmbodimentRuntime::compile(model, None, Some(overlay));

        let evaluation = rt
            .evaluate_pose_workspace(&Transform3D::identity(), "world")
            .expect("pose workspace evaluation should resolve");

        assert!(!evaluation.safe);
        assert!(evaluation.in_human_presence);
        assert!(evaluation.min_margin_m.is_some_and(|margin| margin < 0.0));
        assert!(
            evaluation
                .alerts
                .iter()
                .any(|alert| alert.contains("restricted workspace zone"))
        );
        assert!(
            evaluation
                .alerts
                .iter()
                .any(|alert| alert.contains("human-presence workspace zone"))
        );
    }

    #[test]
    fn evaluate_active_perception_command_accepts_reachable_actuated_sensor() {
        let rt = EmbodimentRuntime::compile(articulated_model_with_actuated_sensor(), None, None);
        let command = ActivePerceptionCommand {
            sensor_id: "cam1".into(),
            target: ViewpointTarget::LookAt { frame_id: "arm".into() },
            observation_goal: ObservationGoal::CoverRegion {
                frame_id: "arm".into(),
                radius: 0.2,
            },
        };

        let check = rt
            .evaluate_active_perception_command(&command, &[0.2])
            .expect("active perception check should resolve");

        assert!(check.sensor.actuated);
        assert!(check.target_valid);
        assert!(check.planned_pose.is_none());
        assert!(check.safe);
        assert!(check.execution.executable);
        assert!(check.alerts.is_empty());
    }

    #[test]
    fn evaluate_active_perception_command_rejects_non_actuated_missing_target() {
        let rt = EmbodimentRuntime::compile(simple_model(), None, None);
        let command = ActivePerceptionCommand {
            sensor_id: "cam0".into(),
            target: ViewpointTarget::LookAt {
                frame_id: "missing_target".into(),
            },
            observation_goal: ObservationGoal::ReduceUncertainty {
                entity_id: "cup_3".into(),
            },
        };

        let check = rt
            .evaluate_active_perception_command(&command, &[])
            .expect("active perception check should resolve");

        assert!(!check.sensor.actuated);
        assert!(!check.target_valid);
        assert!(check.planned_pose.is_none());
        assert!(!check.safe);
        assert!(!check.execution.executable);
        assert!(check.alerts.iter().any(|alert| alert.contains("not actuated")));
        assert!(check.alerts.iter().any(|alert| alert.contains("does not exist")));
    }

    #[test]
    fn evaluate_active_perception_command_move_to_rejects_restricted_planned_pose() {
        let overlay = SafetyOverlay {
            overlay_digest: "overlay-active-perception".into(),
            workspace_restrictions: vec![WorkspaceZone {
                name: "restricted".into(),
                shape: WorkspaceShape::Sphere { radius: 0.15 },
                origin_frame: "world".into(),
                zone_type: ZoneType::Restricted,
                margin_m: 0.0,
            }],
            joint_limit_overrides: Default::default(),
            max_payload_kg: None,
            human_presence_zones: Vec::new(),
            force_limits: None,
            contact_force_envelopes: Vec::new(),
            contact_allowed_zones: Vec::new(),
            force_rate_limits: Default::default(),
        };
        let rt = EmbodimentRuntime::compile(articulated_model_with_actuated_sensor(), None, Some(overlay));
        let command = ActivePerceptionCommand {
            sensor_id: "cam1".into(),
            target: ViewpointTarget::MoveTo {
                pose: Transform3D::identity(),
            },
            observation_goal: ObservationGoal::VerifyCondition {
                description: "inspect target".into(),
            },
        };

        let check = rt
            .evaluate_active_perception_command(&command, &[0.2])
            .expect("active perception check should resolve");

        assert!(check.sensor.workspace_safe);
        assert!(!check.safe);
        let planned_pose = check
            .planned_pose
            .as_ref()
            .expect("move_to should materialize a planned pose evaluation");
        assert!(!check.execution.executable);
        assert_eq!(planned_pose.relative_to, "world");
        assert_eq!(planned_pose.pose.translation, [0.0, 0.0, 0.0]);
        assert!(!planned_pose.safe);
        assert!(planned_pose.min_margin_m.is_some_and(|margin| margin < 0.0));
        assert!(
            check
                .alerts
                .iter()
                .any(|alert| alert.contains("restricted workspace zone"))
        );
    }

    #[test]
    fn check_workspace_reports_restricted_and_human_zone_alerts() {
        let model = simple_model();
        let overlay = SafetyOverlay {
            overlay_digest: "overlay-3".into(),
            workspace_restrictions: vec![WorkspaceZone {
                name: "restricted".into(),
                shape: WorkspaceShape::Sphere { radius: 0.2 },
                origin_frame: "world".into(),
                zone_type: ZoneType::Restricted,
                margin_m: 0.0,
            }],
            joint_limit_overrides: Default::default(),
            max_payload_kg: None,
            human_presence_zones: vec![WorkspaceZone {
                name: "human".into(),
                shape: WorkspaceShape::Sphere { radius: 0.3 },
                origin_frame: "world".into(),
                zone_type: ZoneType::HumanPresence,
                margin_m: 0.0,
            }],
            force_limits: None,
            contact_force_envelopes: Vec::new(),
            contact_allowed_zones: Vec::new(),
            force_rate_limits: Default::default(),
        };
        let rt = EmbodimentRuntime::compile(model, None, Some(overlay));

        let check = rt.check_workspace(&[]);

        assert!(!check.safe);
        assert_eq!(check.frames.len(), 1);
        assert!(check.frames[0].in_human_presence);
        assert!(check.min_margin_m.is_some_and(|margin| margin < 0.0));
        assert!(
            check
                .alerts
                .iter()
                .any(|alert| alert.contains("restricted workspace zone"))
        );
        assert!(
            check
                .alerts
                .iter()
                .any(|alert| alert.contains("human-presence workspace zone"))
        );
    }

    #[test]
    fn runtime_reports_calibration_and_active_perception_capabilities() {
        let mut model = simple_model();
        model.sensor_mounts[0].is_actuated = true;
        model.sensor_mounts[0].frustum = Some(crate::embodiment::model::CameraFrustum {
            fov_horizontal_deg: 90.0,
            fov_vertical_deg: 60.0,
            near_clip_m: 0.1,
            far_clip_m: 10.0,
            resolution: Some((640, 480)),
        });
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        assert!(rt.calibration_valid());
        assert!(rt.active_perception_available());
    }

    #[test]
    fn build_tick_projection_uses_runtime_owned_projection_surfaces() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let snapshot = rt.build_frame_snapshot();

        let projection = rt.build_tick_projection(&snapshot);

        assert_eq!(projection.watched_poses.len(), 2);
        assert!(projection.validation_issues.is_empty());
        assert!(projection.features.calibration_valid);
        assert_eq!(projection.features.workspace_margin, Some(0.9));
        assert_eq!(projection.features.observation_confidence, Some(1.0));
        assert!(!projection.features.active_perception_available);
        assert!(projection.features.alerts.is_empty());
    }

    #[test]
    fn build_tick_projection_surfaces_collision_margin_from_runtime_collision_bodies() {
        let mut model = articulated_model();
        model.collision_bodies = vec![
            CollisionBody {
                link_name: "base".into(),
                geometry: Geometry::Sphere { radius: 0.2 },
                origin: Transform3D::identity(),
            },
            CollisionBody {
                link_name: "arm".into(),
                geometry: Geometry::Sphere { radius: 0.2 },
                origin: Transform3D {
                    translation: [0.75, 0.0, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
            },
        ];
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let projection = rt.build_tick_projection(&rt.build_frame_snapshot());

        assert_eq!(projection.features.collision_margin, Some(0.35));
        assert!(projection.validation_issues.is_empty());
    }

    #[test]
    fn build_tick_input_projection_materializes_snapshot_and_joint_states() {
        let mut model = articulated_model();
        model.joints[0].limits.max_torque = Some(2.0);
        model.stamp_digest();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let projection = rt.build_tick_input_projection(
            22,
            1_234,
            2_468,
            &["slide_y".into()],
            &[0.4],
            &[0.05],
            Some(&[1.2]),
            &FrameSnapshotInput::default(),
        );

        assert_eq!(projection.tick, 22);
        assert_eq!(projection.monotonic_time_ns, 1_234);
        assert_eq!(projection.joints.len(), 1);
        assert_eq!(projection.joints[0].name, "slide_y");
        assert_eq!(projection.joints[0].position, 0.4);
        assert_eq!(projection.joints[0].velocity, 0.05);
        assert_eq!(projection.joints[0].effort, Some(1.2));
        assert!(projection.validation_issues.is_empty());
        assert_eq!(projection.features.workspace_margin, None);
        assert_eq!(projection.features.force_margin, Some(0.8));

        let pose = rt
            .resolve_frame_pose(&projection.snapshot, "arm", Some("world"))
            .expect("arm pose should resolve from inferred joint positions");
        assert_eq!(pose.transform.translation, [0.0, 0.4, 0.0]);
        assert!(projection.watched_poses.iter().any(|watch| watch.frame_id == "arm"));
    }

    #[test]
    fn build_frame_snapshot_uses_watched_frame_freshness() {
        let model = model_with_computed_watch_frame();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let snap_without_dynamic = rt.build_frame_snapshot();
        assert_eq!(
            snap_without_dynamic.frame_freshness.get("fk_tip"),
            Some(&FreshnessState::Unknown)
        );
        assert_eq!(snap_without_dynamic.freshness, FreshnessState::Unknown);

        let snap_with_dynamic = rt.build_frame_snapshot_with_dynamic_transforms(
            1,
            100,
            &[(
                "fk_tip".into(),
                Transform3D {
                    translation: [0.0, 0.5, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 100,
                },
            )],
        );
        assert_eq!(
            snap_with_dynamic.frame_freshness.get("fk_tip"),
            Some(&FreshnessState::Fresh)
        );
        assert_eq!(snap_with_dynamic.freshness, FreshnessState::Fresh);
    }

    #[test]
    fn build_frame_snapshot_reports_invalid_dynamic_transform_provenance() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);

        let snap = rt.build_frame_snapshot_with_dynamic_transforms(
            3,
            50,
            &[(
                "missing".into(),
                Transform3D {
                    translation: [0.0, 0.0, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 75,
                },
            )],
        );

        assert!(snap.dynamic_transforms.is_empty());
        assert!(
            snap.validation_issues
                .iter()
                .any(|issue| issue.contains("dynamic transform references missing frame `missing`"))
        );
    }

    #[test]
    fn serde_roundtrip() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let json = serde_json::to_string(&rt).unwrap();
        let back: EmbodimentRuntime = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.combined_digest, back.combined_digest);
        assert_eq!(rt.model.model_id, back.model.model_id);
        assert_eq!(rt.model_digest, back.model_digest);
        assert_eq!(rt.calibration_digest, back.calibration_digest);
        assert_eq!(rt.safety_digest, back.safety_digest);
        assert_eq!(rt.joint_count, back.joint_count);
        assert_eq!(rt.tcp_count, back.tcp_count);
        assert_eq!(rt.watched_frames, back.watched_frames);
        assert_eq!(rt.frame_graph, back.frame_graph);
        assert_eq!(rt.validation_issues, back.validation_issues);
    }
}
