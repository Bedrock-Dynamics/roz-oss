//! Safety policy enforcement types (FS-01).
//!
//! This module carries the v1.0 serde shape of `roz_safety_policies` rows and
//! the enforcement-outcome / error vocabulary used by the pre-dispatch gate
//! (Plan 24-05) and the 100 Hz copper safety filter (Plan 24-05). Field shape
//! is locked by `.planning/research/DEEP-FS.md §"Schema Definition (Industry
//! Alignment)"` and D-03 / D-04 in this phase's CONTEXT.md.
//!
//! This file currently only declares tests (TDD RED phase for Plan 24-02 Task 1).
//! The types themselves land in the GREEN commit.

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
