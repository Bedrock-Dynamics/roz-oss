//! PX4 readiness fixture replay with exact full-state assertions.

use std::time::{Duration, Instant};

use mavlink::common::{
    EstimatorStatusFlags, GpsFixType, MavAutopilot, MavModeFlag, MavState, MavType, ESTIMATOR_STATUS_DATA,
    GPS_RAW_INT_DATA, HEARTBEAT_DATA,
};
use roz_copper::io_grpc::proto::{MavAutopilot as ProtoMavAutopilot, ReadinessState};
use roz_mavlink::readiness::ReadinessBuilder;

const TAKEOFF_FIXTURE: &str = include_str!("fixtures/readiness/px4/takeoff.json");
const LAND_FIXTURE: &str = include_str!("fixtures/readiness/px4/land.json");

#[test]
fn px4_takeoff_readiness_matches_expected_full_state() {
    let fixture = parse_fixture(TAKEOFF_FIXTURE);
    let actual = replay_fixture(&fixture);
    let expected = fixture.expected;

    assert_eq!(actual, expected, "TAKEOFF readiness fixture drifted");
}

#[test]
fn px4_land_readiness_matches_expected_full_state() {
    let fixture = parse_fixture(LAND_FIXTURE);
    let actual = replay_fixture(&fixture);
    let expected = fixture.expected;

    assert_eq!(actual, expected, "LAND readiness fixture drifted");
}

fn replay_fixture(fixture: &ReadinessFixture) -> ReadinessState {
    let heartbeat = HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_QUADROTOR,
        autopilot: fixture.heartbeat.autopilot,
        base_mode: if fixture.heartbeat.armed {
            MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED
        } else {
            MavModeFlag::from_bits_truncate(0)
        },
        system_status: fixture.heartbeat.system_status,
        mavlink_version: 3,
    };
    let gps = GPS_RAW_INT_DATA {
        time_usec: 0,
        lat: 0,
        lon: 0,
        alt: 0,
        eph: 0,
        epv: 0,
        vel: 0,
        cog: 0,
        fix_type: fixture.gps_raw_int.fix_type,
        satellites_visible: 12,
    };
    let estimator = ESTIMATOR_STATUS_DATA {
        time_usec: 0,
        vel_ratio: 0.0,
        pos_horiz_ratio: 0.0,
        pos_vert_ratio: 0.0,
        mag_ratio: 0.0,
        hagl_ratio: 0.0,
        tas_ratio: 0.0,
        pos_horiz_accuracy: 0.0,
        pos_vert_accuracy: 0.0,
        flags: fixture.estimator_status.flags,
    };

    let heartbeat_at = Instant::now();
    let snapshot_at = heartbeat_at + Duration::from_millis(fixture.expected.heartbeat_age_ms);
    let mut builder = ReadinessBuilder::new();
    builder.apply_heartbeat_at_for_tests(&heartbeat, heartbeat_at);
    builder.apply_gps_raw_int(&gps);
    builder.apply_estimator_status(&estimator);
    builder.snapshot_at_for_tests(snapshot_at)
}

#[derive(Debug)]
struct ReadinessFixture {
    heartbeat: HeartbeatFixture,
    gps_raw_int: GpsFixture,
    estimator_status: EstimatorFixture,
    expected: ReadinessState,
}

#[derive(Debug)]
struct HeartbeatFixture {
    autopilot: MavAutopilot,
    armed: bool,
    system_status: MavState,
}

#[derive(Debug)]
struct GpsFixture {
    fix_type: GpsFixType,
}

#[derive(Debug)]
struct EstimatorFixture {
    flags: EstimatorStatusFlags,
}

fn parse_fixture(json: &str) -> ReadinessFixture {
    ReadinessFixture {
        heartbeat: HeartbeatFixture {
            autopilot: parse_autopilot(json_string(json, "autopilot")),
            armed: json_bool(json, "armed"),
            system_status: parse_mav_state(json_u32(json, "system_status")),
        },
        gps_raw_int: GpsFixture {
            fix_type: parse_gps_fix_type(json_u32(json, "fix_type")),
        },
        estimator_status: EstimatorFixture {
            flags: EstimatorStatusFlags::from_bits_truncate(json_u32(json, "flags") as u16),
        },
        expected: ReadinessState {
            heartbeat_alive: json_bool(json, "heartbeat_alive"),
            heartbeat_age_ms: u64::from(json_u32(json, "heartbeat_age_ms")),
            armed: expected_bool(json, "armed"),
            system_status: json_u32(json, "system_status"),
            gps_fix_type: json_u32(json, "gps_fix_type"),
            has_gps_fix: json_bool(json, "has_gps_fix"),
            ekf_flags: json_u32(json, "ekf_flags"),
            ekf_converged: json_bool(json, "ekf_converged"),
            ready_to_arm: json_bool(json, "ready_to_arm"),
            fully_operational: json_bool(json, "fully_operational"),
            autopilot: ProtoMavAutopilot::Px4 as i32,
        },
    }
}

fn parse_autopilot(value: &str) -> MavAutopilot {
    match value {
        "px4" => MavAutopilot::MAV_AUTOPILOT_PX4,
        other => panic!("unsupported fixture autopilot {other:?}"),
    }
}

fn parse_mav_state(value: u32) -> MavState {
    match value {
        4 => MavState::MAV_STATE_ACTIVE,
        other => panic!("unsupported fixture system_status {other}"),
    }
}

fn parse_gps_fix_type(value: u32) -> GpsFixType {
    match value {
        3 => GpsFixType::GPS_FIX_TYPE_3D_FIX,
        other => panic!("unsupported fixture gps fix_type {other}"),
    }
}

fn json_bool(json: &str, key: &str) -> bool {
    let value = raw_json_value(json, key);
    match value {
        "true" => true,
        "false" => false,
        other => panic!("expected boolean for {key:?}, got {other:?}"),
    }
}

fn expected_bool(json: &str, key: &str) -> bool {
    let expected = expected_object(json);
    json_bool(expected, key)
}

fn json_u32(json: &str, key: &str) -> u32 {
    let value = raw_json_value(json, key);
    value
        .parse()
        .unwrap_or_else(|_| panic!("expected u32 for {key:?}, got {value:?}"))
}

fn json_string<'a>(json: &'a str, key: &str) -> &'a str {
    let value = raw_json_value(json, key);
    value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or_else(|| panic!("expected string for {key:?}, got {value:?}"))
}

fn raw_json_value<'a>(json: &'a str, key: &str) -> &'a str {
    let marker = format!("\"{key}\":");
    let start = json
        .find(&marker)
        .unwrap_or_else(|| panic!("missing fixture key {key:?}"))
        + marker.len();
    let rest = json[start..].trim_start();
    let end = rest
        .find([',', '\n', '}'])
        .unwrap_or_else(|| panic!("unterminated fixture value for {key:?}"));
    rest[..end].trim()
}

fn expected_object(json: &str) -> &str {
    let marker = "\"expected\":";
    let start = json.find(marker).expect("missing expected object") + marker.len();
    let rest = json[start..].trim_start();
    rest.strip_prefix('{').expect("expected object must start with {")
}
