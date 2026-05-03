//! `.tlog` readiness replay.
//!
//! This complements the existing JSON unit fixtures with real MAVLink log
//! replay once direct-SITL readiness captures are checked in.

mod common;

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use mavlink::common::MavMessage;
use roz_copper::io_grpc::proto::ReadinessState;
use roz_mavlink::readiness::ReadinessBuilder;

use common::tlog::{TlogFrame, load_tlog};

#[test]
fn px4_readiness_tlogs_replay_to_expected_states() -> Result<()> {
    let root = fixture_root();
    let cases = readiness_cases();

    if cases.iter().all(|case| resolve_case_path(&root, case).is_none()) {
        eprintln!(
            "SKIP: no PX4 readiness .tlog fixtures found under {}; run the direct-SITL fixture recorder first",
            root.display()
        );
        return Ok(());
    }

    for case in cases {
        let path = resolve_case_path(&root, &case).with_context(|| {
            format!(
                "missing required readiness fixture for {} under {}",
                case.name,
                root.display()
            )
        })?;
        let frames = load_tlog(&path)?;
        assert!(
            !frames.is_empty(),
            "{} fixture must contain at least one MAVLink frame",
            case.name
        );

        let snapshot = replay_readiness(&frames);
        assert_expectation(&case, &snapshot);
    }

    Ok(())
}

fn replay_readiness(frames: &[TlogFrame]) -> ReadinessState {
    let first_timestamp = frames.first().map_or(0, |frame| frame.timestamp_usec);
    let base = Instant::now();
    let mut last_rx_at = base;
    let mut builder = ReadinessBuilder::new();

    for frame in frames {
        let delta_usec = frame.timestamp_usec.saturating_sub(first_timestamp);
        let rx_at = base + Duration::from_micros(delta_usec);
        last_rx_at = rx_at;

        match &frame.message {
            MavMessage::HEARTBEAT(data) => builder.apply_heartbeat_at_for_tests(data, rx_at),
            MavMessage::GPS_RAW_INT(data) => builder.apply_gps_raw_int(data),
            MavMessage::ESTIMATOR_STATUS(data) => builder.apply_estimator_status(data),
            _ => {}
        }
    }

    builder.snapshot_at_for_tests(last_rx_at)
}

fn assert_expectation(case: &ReadinessCase, snapshot: &ReadinessState) {
    match case.expectation {
        ReadinessExpectation::ReadyToArm => {
            assert!(snapshot.heartbeat_alive, "{} heartbeat must be alive", case.name);
            assert!(!snapshot.armed, "{} ready-to-arm fixture should be unarmed", case.name);
            assert!(snapshot.has_gps_fix, "{} must have a 3D GPS fix", case.name);
            assert!(snapshot.ekf_converged, "{} EKF must be converged", case.name);
            assert!(snapshot.ready_to_arm, "{} must be ready to arm", case.name);
            assert!(snapshot.fully_operational, "{} must be fully operational", case.name);
        }
        ReadinessExpectation::NotReady => {
            assert!(
                !snapshot.ready_to_arm,
                "{} must not be ready to arm: {snapshot:?}",
                case.name
            );
        }
        ReadinessExpectation::Degraded => {
            assert!(
                snapshot.heartbeat_alive,
                "{} degraded fixture still needs live heartbeat",
                case.name
            );
            assert!(
                !snapshot.fully_operational,
                "{} degraded fixture must not report fully operational: {snapshot:?}",
                case.name
            );
        }
    }
}

#[derive(Debug)]
struct ReadinessCase {
    name: &'static str,
    fixture_names: &'static [&'static str],
    expectation: ReadinessExpectation,
}

#[derive(Debug, Clone, Copy)]
enum ReadinessExpectation {
    ReadyToArm,
    NotReady,
    Degraded,
}

fn readiness_cases() -> Vec<ReadinessCase> {
    vec![
        ReadinessCase {
            name: "ready",
            fixture_names: &["ready.tlog"],
            expectation: ReadinessExpectation::ReadyToArm,
        },
        ReadinessCase {
            name: "not_ready",
            fixture_names: &["not_ready.tlog", "not-ready.tlog"],
            expectation: ReadinessExpectation::NotReady,
        },
        ReadinessCase {
            name: "degraded",
            fixture_names: &["degraded.tlog"],
            expectation: ReadinessExpectation::Degraded,
        },
    ]
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("readiness")
        .join("px4")
}

fn resolve_case_path(root: &Path, case: &ReadinessCase) -> Option<PathBuf> {
    case.fixture_names
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.exists())
}
