//! Safety policy enforcement types (FS-01).
//!
//! This module carries the v1.0 serde shape of `roz_safety_policies` rows and
//! the enforcement-outcome / error vocabulary used by the pre-dispatch gate
//! (Plan 24-05) and the 100 Hz copper safety filter (Plan 24-05). Field shape
//! is locked by `.planning/research/DEEP-FS.md §"Schema Definition (Industry
//! Alignment)"` and D-03 / D-04 in this phase's CONTEXT.md.
//!
//! The serde struct uses `#[serde(deny_unknown_fields)]` at every level per
//! 24-RESEARCH.md §Anti-Patterns — unknown shapes are a regression signal, not
//! forward-compat room.
//!
//! This module ships TYPES ONLY — the `enforce_policy` function body lands in
//! Plan 24-05.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Locked policy JSON shape v1.0.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PolicyV1 {
    pub policy_id: Uuid,
    pub version: u32,
    pub enforcement_mode: EnforcementMode,
    pub limits: PolicyLimits,
    #[serde(default)]
    pub geofences: Vec<Geofence>,
    #[serde(default)]
    pub interlocks: Vec<Interlock>,
    pub deadman_timers: DeadmanTimers,
}

/// `enforcement_mode`: reject / clamp / halt (locked, D-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementMode {
    Reject,
    Clamp,
    Halt,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PolicyLimits {
    pub max_velocity: VelocityLimits,
    pub max_acceleration: AccelerationLimits,
    pub max_force: ForceLimits,
    #[serde(default)]
    pub joint_limits: Vec<JointLimit>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VelocityLimits {
    pub linear_m_per_s: f64,
    pub angular_rad_per_s: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AccelerationLimits {
    pub linear_m_per_s2: f64,
    pub angular_rad_per_s2: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ForceLimits {
    pub newtons: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JointLimit {
    pub name: String,
    pub min_rad: f64,
    pub max_rad: f64,
}

/// Geofence shape per DEEP-FS v1.0. `kind` is the internally-tagged
/// discriminant; polygons carry `vertices_lat_lon` + ceiling + breach action.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Geofence {
    Polygon {
        vertices_lat_lon: Vec<[f64; 2]>,
        altitude_ceiling_m: f64,
        action_on_breach: OnBreachAction,
    },
}

/// Action values accepted by `action_on_breach` / `action_on_missing` / `on_expire`
/// per D-03: `halt | hold_position | land | return_to_launch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnBreachAction {
    Halt,
    HoldPosition,
    Land,
    ReturnToLaunch,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Interlock {
    pub name: String,
    pub required_states: Vec<String>,
    pub action_on_missing: OnBreachAction,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeadmanTimers {
    pub command_timeout_ms: u64,
    pub on_expire: OnBreachAction,
}

/// Outcome of running a command through the enforcement gate.
///
/// Shape mirrors the `TaskOutcome` tagged-enum idiom at
/// `crates/roz-server/src/restate/task_workflow.rs:23-43`.
#[derive(Debug)]
pub enum EnforcementOutcome {
    /// Command passes all policy checks unchanged.
    Allow,
    /// Command exceeds a soft limit; a clamped substitute is produced.
    Clamp { clamped_details: serde_json::Value },
    /// Command violates policy under `reject` mode.
    Reject(PolicyEnforcementError),
    /// Command violates policy under `halt` mode — motion must stop.
    Halt(PolicyEnforcementError),
}

/// All failure modes surfaced by policy enforcement. `thiserror` at the crate
/// boundary per CLAUDE.md §Error Handling.
#[derive(Debug, Error)]
pub enum PolicyEnforcementError {
    #[error("limit exceeded: channel='{channel}' value={value} max={max}")]
    LimitExceeded { channel: String, value: f64, max: f64 },

    #[error("geofence breach at coordinates {coords}")]
    GeofenceBreach { coords: String },

    #[error("required interlock '{name}' missing")]
    InterlockMissing { name: String },

    #[error("policy JSON parse: {0}")]
    PolicyParse(#[from] serde_json::Error),

    #[error("policy cache stale: age {age_secs}s exceeds 30 s freshness window")]
    PolicyStale { age_secs: u64 },
}

/// Convert a database row into a validated [`PolicyV1`].
///
/// The `policy_json` JSONB column is the source of truth; the other JSONB
/// columns (`limits`, `geofences`, `interlocks`, `deadman_timers`) are
/// denormalized accelerators that this function does NOT currently consult
/// (A-10 in 24-RESEARCH).
///
/// # Errors
///
/// [`PolicyEnforcementError::PolicyParse`] on any shape mismatch or unknown
/// field — the serde layer enforces `deny_unknown_fields` at every nested
/// level, so malformed policy JSON surfaces here rather than corrupting the
/// cache (T-24-12).
pub fn parse_policy_from_row(
    row: &roz_db::safety_policies::SafetyPolicyRow,
) -> Result<PolicyV1, PolicyEnforcementError> {
    let policy: PolicyV1 = serde_json::from_value(row.policy_json.clone())?;
    Ok(policy)
}

/// Evaluate a single velocity+force command against policy limits + interlocks.
///
/// Used by the copper 100 Hz tick filter via a thin wrapper in
/// `crates/roz-copper/src/safety_filter.rs`. Hot path — avoids allocations on
/// the pass path and performs at worst one `String::clone` per
/// interlock/limit violation.
///
/// `interlock_states` is a slice of currently-asserted state names (e.g.,
/// `["arm.extended", "gripper.closed"]`) — the caller assembles this from
/// `TelemetryFrame.state` or copper's `ControllerState` before invoking.
///
/// **Geofence checks are not performed here.** Full geofence geometry lands when
/// the MAVLink backend (Phase 25) surfaces `lat_lon_alt` in the command frame
/// per 24-CONTEXT D-03.
#[must_use]
pub fn enforce_command(
    policy: &PolicyV1,
    velocity_linear_m_per_s: f64,
    velocity_angular_rad_per_s: f64,
    force_newtons: Option<f64>,
    interlock_states: &[String],
) -> EnforcementOutcome {
    let max_linear = policy.limits.max_velocity.linear_m_per_s;
    let max_angular = policy.limits.max_velocity.angular_rad_per_s;
    let max_force = policy.limits.max_force.newtons;

    // Linear velocity
    if velocity_linear_m_per_s.abs() > max_linear {
        let err = PolicyEnforcementError::LimitExceeded {
            channel: "linear_velocity".to_string(),
            value: velocity_linear_m_per_s,
            max: max_linear,
        };
        return dispatch_mode(policy.enforcement_mode, err, || {
            serde_json::json!({
                "channel": "linear_velocity",
                "clamped_to": max_linear.copysign(velocity_linear_m_per_s),
            })
        });
    }

    // Angular velocity
    if velocity_angular_rad_per_s.abs() > max_angular {
        let err = PolicyEnforcementError::LimitExceeded {
            channel: "angular_velocity".to_string(),
            value: velocity_angular_rad_per_s,
            max: max_angular,
        };
        return dispatch_mode(policy.enforcement_mode, err, || {
            serde_json::json!({
                "channel": "angular_velocity",
                "clamped_to": max_angular.copysign(velocity_angular_rad_per_s),
            })
        });
    }

    // Force (optional — not all commands carry force)
    if let Some(f) = force_newtons
        && f.abs() > max_force
    {
        let err = PolicyEnforcementError::LimitExceeded {
            channel: "force".to_string(),
            value: f,
            max: max_force,
        };
        return dispatch_mode(policy.enforcement_mode, err, || {
            serde_json::json!({
                "channel": "force",
                "clamped_to": max_force.copysign(f),
            })
        });
    }

    // Interlocks: every required state must be asserted; any missing → halt.
    for interlock in &policy.interlocks {
        for required in &interlock.required_states {
            if !interlock_states.iter().any(|s| s == required) {
                return EnforcementOutcome::Halt(PolicyEnforcementError::InterlockMissing {
                    name: interlock.name.clone(),
                });
            }
        }
    }

    EnforcementOutcome::Allow
}

/// Route a detected violation through the policy's configured enforcement
/// mode. Under `clamp`, the caller-supplied closure produces the clamped
/// details (lazy to avoid building the JSON object when `reject`/`halt` are
/// in effect).
fn dispatch_mode(
    mode: EnforcementMode,
    err: PolicyEnforcementError,
    clamp_details_fn: impl FnOnce() -> serde_json::Value,
) -> EnforcementOutcome {
    match mode {
        EnforcementMode::Reject => EnforcementOutcome::Reject(err),
        EnforcementMode::Halt => EnforcementOutcome::Halt(err),
        EnforcementMode::Clamp => EnforcementOutcome::Clamp {
            clamped_details: clamp_details_fn(),
        },
    }
}

/// Project a worker-side [`PolicyV1`] into the minimal
/// [`roz_copper::policy::CopperPolicy`] shape read by the 100 Hz copper
/// safety filter (Plan 24-05).
///
/// Copper does NOT depend on `roz-worker`, so this projection is the only
/// supported way to feed live policy data into the copper hot-swap pointer.
/// Called from the worker's `roz.policy.{worker_id}` subscriber in `main.rs`
/// after a policy push arrives and parses cleanly.
#[must_use]
pub fn project_to_copper_policy(p: &PolicyV1) -> roz_copper::policy::CopperPolicy {
    roz_copper::policy::CopperPolicy {
        max_linear_m_per_s: p.limits.max_velocity.linear_m_per_s,
        max_angular_rad_per_s: p.limits.max_velocity.angular_rad_per_s,
        max_force_newtons: p.limits.max_force.newtons,
        enforcement_mode: match p.enforcement_mode {
            EnforcementMode::Reject => roz_copper::policy::CopperEnforcementMode::Reject,
            EnforcementMode::Clamp => roz_copper::policy::CopperEnforcementMode::Clamp,
            EnforcementMode::Halt => roz_copper::policy::CopperEnforcementMode::Halt,
        },
    }
}

/// Pre-dispatch enforcement entrypoint — called by the worker dispatch layer
/// before handing a `TaskInvocation` to the agent loop.
///
/// Checks the invocation's declared velocity parameters (if any) against
/// policy limits. Interlocks and geofences are NOT evaluated here — they are
/// live-state checks that only make sense in the 100 Hz copper tick via
/// [`enforce_command`].
///
/// `None` declared parameters are treated as zero: the gate does not attempt
/// to infer motion intent from an omitted field. When Plan 24-09 threads the
/// actual `TaskInvocation.declared_*` fields through dispatch, the caller
/// feeds them in directly.
///
/// Cheap: no allocations on the pass path.
#[must_use]
pub fn enforce_invocation(
    policy: &PolicyV1,
    declared_velocity_linear: Option<f64>,
    declared_velocity_angular: Option<f64>,
) -> EnforcementOutcome {
    let vl = declared_velocity_linear.unwrap_or(0.0);
    let va = declared_velocity_angular.unwrap_or(0.0);
    enforce_command(policy, vl, va, None, &[])
}

/// Apply a verified [`SafetyPolicyRow`] to the worker's in-memory policy
/// surfaces: [`PolicyCache`], [`HotPolicy`], and the copper [`HotCopperPolicy`]
/// `ArcSwap`. Best-effort emits a [`CheckpointTrigger::DegradationChange`]
/// trigger on the provided sender (drop on full channel matches production
/// semantics — mirrors main.rs:1713 pre-refactor).
///
/// This is the "apply" half of the policy-push subscriber in main.rs: the
/// signature-verify + row-parse half stays inline in the subscribe loop
/// because the signing context and `serde_json::from_slice` error branches
/// are loop-specific. Plan 24-14 Task 2 extracts the apply half so the
/// Task 3 end-to-end test can drive the cache/hot/copper_hot fan-out
/// without replicating the production wiring.
///
/// [`SafetyPolicyRow`]: roz_db::safety_policies::SafetyPolicyRow
/// [`PolicyCache`]: crate::policy_cache::PolicyCache
/// [`HotPolicy`]: crate::policy_cache::HotPolicy
/// [`HotCopperPolicy`]: roz_copper::policy::HotCopperPolicy
/// [`CheckpointTrigger::DegradationChange`]: crate::checkpoint_writer::CheckpointTrigger::DegradationChange
///
/// # Errors
///
/// Propagates [`PolicyEnforcementError::PolicyParse`] when the row's
/// `policy_json` column cannot be parsed as a `PolicyV1` (unknown field,
/// wrong enum, etc. — the `deny_unknown_fields` fence at every level).
pub async fn apply_policy_push(
    row: &roz_db::safety_policies::SafetyPolicyRow,
    cache: &crate::policy_cache::PolicyCache,
    hot: &crate::policy_cache::HotPolicy,
    copper_hot: &roz_copper::policy::HotCopperPolicy,
    ckpt_tx: Option<&tokio::sync::mpsc::Sender<crate::checkpoint_writer::CheckpointTrigger>>,
) -> Result<(), PolicyEnforcementError> {
    let policy = parse_policy_from_row(row)?;
    let policy_arc = cache.insert(row.id, policy.clone()).await;
    hot.store((*policy_arc).clone());
    let cp = project_to_copper_policy(&policy);
    copper_hot.store(std::sync::Arc::new(cp));
    if let Some(tx) = ckpt_tx {
        let trigger = crate::checkpoint_writer::CheckpointTrigger::DegradationChange {
            task_id: String::new(),
            step_counter: 0,
            from: "unknown".into(),
            to: format!("policy_v{}", row.version),
        };
        let _ = tx.try_send(trigger);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_policy_json() -> serde_json::Value {
        serde_json::json!({
            "policy_id": "00000000-0000-0000-0000-000000000001",
            "version": 1,
            "enforcement_mode": "reject",
            "limits": {
                "max_velocity": { "linear_m_per_s": 3.0, "angular_rad_per_s": 1.5 },
                "max_acceleration": { "linear_m_per_s2": 2.0, "angular_rad_per_s2": 1.0 },
                "max_force": { "newtons": 50.0 }
            },
            "deadman_timers": {
                "command_timeout_ms": 5000,
                "on_expire": "halt"
            }
        })
    }

    #[test]
    fn policy_v1_parses_minimal_shape() {
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        assert_eq!(p.version, 1);
        assert_eq!(p.enforcement_mode, EnforcementMode::Reject);
        assert_eq!(p.deadman_timers.on_expire, OnBreachAction::Halt);
        assert!(p.geofences.is_empty());
        assert!(p.interlocks.is_empty());
    }

    #[test]
    fn policy_v1_parses_full_shape() {
        let full = serde_json::json!({
            "policy_id": "00000000-0000-0000-0000-000000000001",
            "version": 1,
            "enforcement_mode": "halt",
            "limits": {
                "max_velocity": { "linear_m_per_s": 3.0, "angular_rad_per_s": 1.5 },
                "max_acceleration": { "linear_m_per_s2": 2.0, "angular_rad_per_s2": 1.0 },
                "max_force": { "newtons": 50.0 },
                "joint_limits": [{ "name": "j1", "min_rad": -1.5, "max_rad": 1.5 }]
            },
            "geofences": [{
                "kind": "polygon",
                "vertices_lat_lon": [[40.7, -74.0], [40.8, -74.1]],
                "altitude_ceiling_m": 120.0,
                "action_on_breach": "return_to_launch"
            }],
            "interlocks": [{
                "name": "arm_gripper_exclusive",
                "required_states": ["arm.extended", "gripper.closed"],
                "action_on_missing": "halt"
            }],
            "deadman_timers": { "command_timeout_ms": 5000, "on_expire": "land" }
        });
        let p: PolicyV1 = serde_json::from_value(full).unwrap();
        assert_eq!(p.geofences.len(), 1);
        assert_eq!(p.interlocks.len(), 1);
        assert_eq!(p.deadman_timers.on_expire, OnBreachAction::Land);
        assert_eq!(p.limits.joint_limits.len(), 1);
    }

    #[test]
    fn policy_v1_rejects_unknown_field() {
        let mut v = minimal_policy_json();
        v.as_object_mut()
            .unwrap()
            .insert("attack".into(), serde_json::json!(true));
        let err = serde_json::from_value::<PolicyV1>(v).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "unexpected err: {err}");
    }

    #[test]
    fn policy_v1_rejects_unknown_enforcement_mode() {
        let mut v = minimal_policy_json();
        v["enforcement_mode"] = serde_json::json!("ignore");
        assert!(serde_json::from_value::<PolicyV1>(v).is_err());
    }

    #[test]
    fn policy_v1_rejects_unknown_on_expire_action() {
        let mut v = minimal_policy_json();
        v["deadman_timers"]["on_expire"] = serde_json::json!("shutdown");
        assert!(serde_json::from_value::<PolicyV1>(v).is_err());
    }

    #[test]
    fn policy_v1_round_trip_serde() {
        let original: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let json = serde_json::to_value(&original).unwrap();
        let parsed: PolicyV1 = serde_json::from_value(json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn enforcement_outcome_variants_construct() {
        let allow = EnforcementOutcome::Allow;
        let clamp = EnforcementOutcome::Clamp {
            clamped_details: serde_json::json!({"v": 1.0}),
        };
        let reject = EnforcementOutcome::Reject(PolicyEnforcementError::LimitExceeded {
            channel: "velocity".into(),
            value: 5.0,
            max: 3.0,
        });
        let halt = EnforcementOutcome::Halt(PolicyEnforcementError::GeofenceBreach {
            coords: "40.7,-74.0".into(),
        });
        assert!(matches!(allow, EnforcementOutcome::Allow));
        assert!(matches!(clamp, EnforcementOutcome::Clamp { .. }));
        assert!(matches!(reject, EnforcementOutcome::Reject(_)));
        assert!(matches!(halt, EnforcementOutcome::Halt(_)));
    }

    #[test]
    fn policy_enforcement_error_display() {
        let e = PolicyEnforcementError::LimitExceeded {
            channel: "velocity".into(),
            value: 5.0,
            max: 3.0,
        };
        assert_eq!(e.to_string(), "limit exceeded: channel='velocity' value=5 max=3");
    }

    #[test]
    fn policy_stale_error_display() {
        let e = PolicyEnforcementError::PolicyStale { age_secs: 45 };
        assert!(e.to_string().contains("45"));
    }

    #[test]
    fn parse_policy_from_row_builds_policyv1() {
        let policy_id = uuid::Uuid::new_v4();
        let mut policy_json = minimal_policy_json();
        policy_json["policy_id"] = serde_json::json!(policy_id);
        let row = roz_db::safety_policies::SafetyPolicyRow {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            name: "test".into(),
            version: 1,
            policy_json,
            limits: serde_json::json!({}),
            geofences: serde_json::json!([]),
            interlocks: serde_json::json!([]),
            deadman_timers: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let parsed = parse_policy_from_row(&row).expect("parse");
        assert_eq!(parsed.policy_id, policy_id);
        assert_eq!(parsed.enforcement_mode, EnforcementMode::Reject);
    }

    // ------------------------------------------------------------------
    // Plan 24-05 Task 1: enforce_command / enforce_invocation tests.
    // minimal_policy_json() has enforcement_mode=reject and
    // max_velocity.linear_m_per_s=3.0, angular_rad_per_s=1.5, max_force=50 N.
    // ------------------------------------------------------------------

    #[test]
    fn enforce_command_allow_under_limit() {
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let out = enforce_command(&p, 1.5, 0.5, None, &[]);
        assert!(matches!(out, EnforcementOutcome::Allow), "got {out:?}");
    }

    #[test]
    fn enforce_command_reject_over_limit_in_reject_mode() {
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let out = enforce_command(&p, 5.0, 0.0, None, &[]);
        match out {
            EnforcementOutcome::Reject(PolicyEnforcementError::LimitExceeded { channel, value, max }) => {
                assert_eq!(channel, "linear_velocity");
                assert!((value - 5.0).abs() < 1e-9);
                assert!((max - 3.0).abs() < 1e-9);
            }
            other => panic!("expected Reject(LimitExceeded), got {other:?}"),
        }
    }

    #[test]
    fn enforce_command_clamp_over_limit_in_clamp_mode() {
        let mut v = minimal_policy_json();
        v["enforcement_mode"] = serde_json::json!("clamp");
        let p: PolicyV1 = serde_json::from_value(v).unwrap();
        let out = enforce_command(&p, 5.0, 0.0, None, &[]);
        match out {
            EnforcementOutcome::Clamp { clamped_details } => {
                assert_eq!(clamped_details["channel"], serde_json::json!("linear_velocity"));
                assert!((clamped_details["clamped_to"].as_f64().unwrap() - 3.0).abs() < 1e-9);
            }
            other => panic!("expected Clamp, got {other:?}"),
        }
    }

    #[test]
    fn enforce_command_halt_over_limit_in_halt_mode() {
        let mut v = minimal_policy_json();
        v["enforcement_mode"] = serde_json::json!("halt");
        let p: PolicyV1 = serde_json::from_value(v).unwrap();
        let out = enforce_command(&p, 5.0, 0.0, None, &[]);
        assert!(matches!(out, EnforcementOutcome::Halt(_)), "got {out:?}");
    }

    #[test]
    fn enforce_command_angular_over_limit_in_reject_mode() {
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let out = enforce_command(&p, 0.0, 3.0, None, &[]);
        match out {
            EnforcementOutcome::Reject(PolicyEnforcementError::LimitExceeded { channel, .. }) => {
                assert_eq!(channel, "angular_velocity");
            }
            other => panic!("expected Reject(LimitExceeded), got {other:?}"),
        }
    }

    #[test]
    fn enforce_command_force_over_limit_in_reject_mode() {
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let out = enforce_command(&p, 0.0, 0.0, Some(100.0), &[]);
        match out {
            EnforcementOutcome::Reject(PolicyEnforcementError::LimitExceeded { channel, .. }) => {
                assert_eq!(channel, "force");
            }
            other => panic!("expected Reject(LimitExceeded), got {other:?}"),
        }
    }

    #[test]
    fn enforce_command_missing_interlock_halts() {
        let mut v = minimal_policy_json();
        v["interlocks"] = serde_json::json!([
            {"name": "arm_safety", "required_states": ["gripper.closed"], "action_on_missing": "halt"}
        ]);
        let p: PolicyV1 = serde_json::from_value(v).unwrap();
        let out = enforce_command(&p, 0.0, 0.0, None, &[]);
        match out {
            EnforcementOutcome::Halt(PolicyEnforcementError::InterlockMissing { name }) => {
                assert_eq!(name, "arm_safety");
            }
            other => panic!("expected Halt(InterlockMissing), got {other:?}"),
        }
    }

    #[test]
    fn enforce_command_satisfied_interlock_allows() {
        let mut v = minimal_policy_json();
        v["interlocks"] = serde_json::json!([
            {"name": "arm_safety", "required_states": ["gripper.closed"], "action_on_missing": "halt"}
        ]);
        let p: PolicyV1 = serde_json::from_value(v).unwrap();
        let out = enforce_command(&p, 0.0, 0.0, None, &["gripper.closed".to_string()]);
        assert!(matches!(out, EnforcementOutcome::Allow), "got {out:?}");
    }

    #[test]
    fn enforce_invocation_allow_when_under_limits() {
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let out = enforce_invocation(&p, Some(1.0), Some(0.5));
        assert!(matches!(out, EnforcementOutcome::Allow), "got {out:?}");
    }

    #[test]
    fn enforce_invocation_reject_when_declared_over_limit() {
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let out = enforce_invocation(&p, Some(5.0), Some(0.0));
        assert!(matches!(out, EnforcementOutcome::Reject(_)), "got {out:?}");
    }

    #[test]
    fn enforce_invocation_none_declared_is_allow() {
        // Pre-dispatch only inspects declared parameters; with None both,
        // the effective velocity is 0 → Allow.
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let out = enforce_invocation(&p, None, None);
        assert!(matches!(out, EnforcementOutcome::Allow), "got {out:?}");
    }

    #[test]
    fn project_to_copper_policy_maps_every_field() {
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let cp = project_to_copper_policy(&p);
        assert!((cp.max_linear_m_per_s - 3.0).abs() < f64::EPSILON);
        assert!((cp.max_angular_rad_per_s - 1.5).abs() < f64::EPSILON);
        assert!((cp.max_force_newtons - 50.0).abs() < f64::EPSILON);
        assert_eq!(cp.enforcement_mode, roz_copper::policy::CopperEnforcementMode::Reject);
    }

    #[test]
    fn project_to_copper_policy_maps_halt_mode() {
        let mut v = minimal_policy_json();
        v["enforcement_mode"] = serde_json::json!("halt");
        let p: PolicyV1 = serde_json::from_value(v).unwrap();
        let cp = project_to_copper_policy(&p);
        assert_eq!(cp.enforcement_mode, roz_copper::policy::CopperEnforcementMode::Halt);
    }

    #[test]
    fn enforce_invocation_under_10ms_budget() {
        // 10 000 iterations must complete well under 100 ms so per-call
        // amortises to << 10 µs — leaves massive headroom under the 10 ms gate.
        let p: PolicyV1 = serde_json::from_value(minimal_policy_json()).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            let _ = enforce_invocation(&p, Some(1.0), Some(0.5));
        }
        let elapsed = start.elapsed();
        eprintln!(
            "bench: enforce_invocation x10_000 = {elapsed:?} (~{} ns/call)",
            elapsed.as_nanos() / 10_000
        );
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "enforce_invocation x10k should be << 100 ms, took {elapsed:?}"
        );
    }
}
