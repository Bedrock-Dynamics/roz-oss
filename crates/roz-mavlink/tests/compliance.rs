//! Direct-MAVLink compliance replay.
//!
//! These tests are fixture-gated. When no `.tlog` fixtures are checked in,
//! the suite reports an explicit skip. Once the nightly/direct-SITL captures
//! land, the same tests compare decoded command payloads against the
//! dispatcher bytes that Roz emits.

mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use mavlink::{
    MavHeader,
    common::{COMMAND_LONG_DATA, MavCmd, MavMessage},
    write_v2_msg,
};
use roz_copper::io::{FlightCommand, FlightCommandParams, MavFrame};
use roz_mavlink::flight_command::{AutopilotHint, FlightCommandDispatcher};

use common::tlog::{
    command_int_payload_equal, command_long_payload_equal, find_command_ack, find_command_int, find_command_long,
    load_tlog,
};

#[test]
fn tlog_reader_decodes_upstream_mavlink_v2_frame() -> Result<()> {
    let expected = COMMAND_LONG_DATA {
        param1: 1.0,
        param2: 0.0,
        param3: 0.0,
        param4: f32::NAN,
        param5: 0.0,
        param6: 0.0,
        param7: 5.0,
        command: MavCmd::MAV_CMD_NAV_TAKEOFF,
        target_system: 1,
        target_component: 1,
        confirmation: 0,
    };
    let mut mavlink_bytes = Vec::new();
    write_v2_msg(
        &mut mavlink_bytes,
        MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 42,
        },
        &MavMessage::COMMAND_LONG(expected.clone()),
    )?;

    let mut tlog_bytes = 1_234_567_u64.to_be_bytes().to_vec();
    tlog_bytes.extend_from_slice(&mavlink_bytes);

    let path = temp_tlog_path();
    fs::write(&path, tlog_bytes).with_context(|| format!("write {}", path.display()))?;
    let frames = load_tlog(&path)?;
    let _ = fs::remove_file(&path);

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].timestamp_usec, 1_234_567);
    let captured = find_command_long(&frames, MavCmd::MAV_CMD_NAV_TAKEOFF).expect("decoded COMMAND_LONG");
    assert!(command_long_payload_equal(captured, &expected));

    Ok(())
}

#[test]
fn px4_command_tlogs_match_dispatcher_wire_payloads() -> Result<()> {
    let root = fixture_root();
    let cases = px4_cases();

    if cases.iter().all(|case| resolve_case_path(&root, case).is_none()) {
        eprintln!(
            "SKIP: no PX4 command .tlog fixtures found under {}; run the direct-SITL fixture recorder first",
            root.display()
        );
        return Ok(());
    }

    let dispatcher = FlightCommandDispatcher::new_for_tests(AutopilotHint::Px4);

    for case in cases {
        let path = resolve_case_path(&root, &case)
            .with_context(|| format!("missing required fixture for {} under {}", case.name, root.display()))?;
        let frames = load_tlog(&path)?;
        let (expected_cmd, expected_msg) = dispatcher
            .build_message(&case.command)
            .with_context(|| format!("build expected MAVLink message for {}", case.name))?;

        assert_eq!(
            expected_cmd, case.mav_cmd,
            "{} fixture case points at the wrong expected MAV_CMD",
            case.name
        );
        assert_payload_matches(case.name, &frames, expected_cmd, &expected_msg)?;
        assert!(
            find_command_ack(&frames, expected_cmd).is_some(),
            "{} fixture must include COMMAND_ACK for {:?}",
            case.name,
            expected_cmd
        );
    }

    Ok(())
}

fn assert_payload_matches(
    case_name: &str,
    frames: &[common::tlog::TlogFrame],
    expected_cmd: MavCmd,
    expected_msg: &MavMessage,
) -> Result<()> {
    match expected_msg {
        MavMessage::COMMAND_LONG(expected) => {
            let captured = find_command_long(frames, expected_cmd)
                .with_context(|| format!("{case_name} fixture missing COMMAND_LONG {expected_cmd:?}"))?;
            assert!(
                command_long_payload_equal(captured, expected),
                "{case_name} COMMAND_LONG payload drifted\ncaptured: {captured:?}\nexpected: {expected:?}"
            );
        }
        MavMessage::COMMAND_INT(expected) => {
            let captured = find_command_int(frames, expected_cmd)
                .with_context(|| format!("{case_name} fixture missing COMMAND_INT {expected_cmd:?}"))?;
            assert!(
                command_int_payload_equal(captured, expected),
                "{case_name} COMMAND_INT payload drifted\ncaptured: {captured:?}\nexpected: {expected:?}"
            );
        }
        other => bail!("{case_name} expected COMMAND_LONG/COMMAND_INT, got {other:?}"),
    }

    Ok(())
}

#[derive(Debug)]
struct ComplianceCase {
    name: &'static str,
    fixture_names: &'static [&'static str],
    mav_cmd: MavCmd,
    command: FlightCommand,
}

fn px4_cases() -> Vec<ComplianceCase> {
    vec![
        ComplianceCase {
            name: "arm",
            fixture_names: &["arm.tlog"],
            mav_cmd: MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            command: FlightCommand::Arm(FlightCommandParams::default()),
        },
        ComplianceCase {
            name: "disarm",
            fixture_names: &["disarm.tlog"],
            mav_cmd: MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
            command: FlightCommand::Disarm(FlightCommandParams::default()),
        },
        ComplianceCase {
            name: "takeoff",
            fixture_names: &["takeoff.tlog"],
            mav_cmd: MavCmd::MAV_CMD_NAV_TAKEOFF,
            command: FlightCommand::Takeoff(FlightCommandParams {
                altitude_m: 5.0,
                ..FlightCommandParams::default()
            }),
        },
        ComplianceCase {
            name: "land",
            fixture_names: &["land.tlog"],
            mav_cmd: MavCmd::MAV_CMD_NAV_LAND,
            command: FlightCommand::Land(FlightCommandParams::default()),
        },
        ComplianceCase {
            name: "return_to_launch",
            fixture_names: &["return_to_launch.tlog", "rtl.tlog"],
            mav_cmd: MavCmd::MAV_CMD_NAV_RETURN_TO_LAUNCH,
            command: FlightCommand::ReturnToLaunch(FlightCommandParams::default()),
        },
        ComplianceCase {
            name: "set_mode_offboard",
            fixture_names: &["set_mode_offboard.tlog", "set_mode.tlog"],
            mav_cmd: MavCmd::MAV_CMD_DO_SET_MODE,
            command: FlightCommand::SetMode(FlightCommandParams {
                mode: "OFFBOARD".to_string(),
                ..FlightCommandParams::default()
            }),
        },
        ComplianceCase {
            name: "goto_global_relative_alt_int",
            fixture_names: &["goto_global_relative_alt_int.tlog", "goto.tlog"],
            mav_cmd: MavCmd::MAV_CMD_DO_REPOSITION,
            command: FlightCommand::Goto(FlightCommandParams {
                x: 47.3977,
                y: 8.5456,
                z: 50.0,
                frame: Some(MavFrame::GlobalRelativeAltInt),
                ..FlightCommandParams::default()
            }),
        },
    ]
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("compliance")
        .join("px4")
}

fn resolve_case_path(root: &Path, case: &ComplianceCase) -> Option<PathBuf> {
    case.fixture_names
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.exists())
}

fn temp_tlog_path() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("roz-mavlink-tlog-test-{}-{unique}.tlog", std::process::id()))
}
