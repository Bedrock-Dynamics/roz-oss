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
}
