//! MAVLink -> `io_grpc::proto::ReadinessState` translation (MAV-03).
//!
//! Ingests HEARTBEAT (0), GPS_RAW_INT (24), and ESTIMATOR_STATUS (230)
//! messages into a mutable [`ReadinessBuilder`]. `snapshot()` returns a
//! fresh `ReadinessState` with derived flags computed per DEEP-MAV §4.
//!
//! The builder is created fresh per MAVLink session; it does NOT persist
//! across backend restarts. Pre-first-HEARTBEAT state is "not alive, not
//! armed, no GPS, no EKF" -- consumers treat this as the startup posture.
//!
//! # Deviation from plan 25-07 (Rule 1)
//!
//! The plan imports `roz_copper::proto_v2::ReadinessState`, but v2 proto
//! explicitly excludes `ReadinessState` per 25-CONTEXT.md D-05' (stays in
//! v1 as an additive `autopilot` field). We import from
//! `roz_copper::io_grpc::proto` instead. `MavAutopilot` exists in both v1
//! and v2; we use the v1 variant here since `ReadinessState.autopilot` is
//! typed as the v1 enum.

use std::time::{Duration, Instant};

use mavlink::common::{
    ESTIMATOR_STATUS_DATA, GPS_RAW_INT_DATA, HEARTBEAT_DATA, MavAutopilot as UpstreamMavAutopilot, MavModeFlag,
};
use roz_copper::io_grpc::proto::{MavAutopilot as ProtoMavAutopilot, ReadinessState};

/// Heartbeat is considered alive if last receipt was within this window.
/// PX4 + ArduPilot emit HEARTBEAT at 1 Hz per spec; 3 s tolerates one
/// missed beat without false-degrading readiness.
const HEARTBEAT_ALIVE_WINDOW: Duration = Duration::from_secs(3);

/// GPS fix types of interest (from MAVLink `GPS_FIX_TYPE` enum).
const GPS_FIX_TYPE_3D_FIX: u32 = 3;

/// Mandatory EKF status flag bits for `ekf_converged`. Derived from
/// DEEP-MAV §4 (matches PX4's internal arm-gate check).
/// Source: <https://mavlink.io/en/messages/common.html#ESTIMATOR_STATUS_FLAGS>
const EKF_CONVERGED_MASK: u32 = (1 << 0)  // ESTIMATOR_ATTITUDE
  | (1 << 1)  // ESTIMATOR_VELOCITY_HORIZ
  | (1 << 3)  // ESTIMATOR_POS_HORIZ_REL
  | (1 << 6); // ESTIMATOR_PRED_POS_HORIZ_REL

/// Accumulates MAVLink readiness inputs and emits [`ReadinessState`].
#[derive(Debug, Default)]
#[allow(
    clippy::struct_field_names,
    reason = "`last_*` prefix communicates the 'most recent' semantics for each source \
              message; renaming would obscure the builder's meaning."
)]
pub struct ReadinessBuilder {
    last_heartbeat: Option<HeartbeatState>,
    last_gps: Option<GpsState>,
    last_ekf: Option<EkfState>,
}

#[derive(Debug, Clone, Copy)]
struct HeartbeatState {
    rx_at: Instant,
    armed: bool,
    system_status: u32,
    autopilot: ProtoMavAutopilot,
}

#[derive(Debug, Clone, Copy)]
struct GpsState {
    fix_type: u32,
}

#[derive(Debug, Clone, Copy)]
struct EkfState {
    flags: u32,
}

impl ReadinessBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest a HEARTBEAT -- updates `heartbeat_alive`, `heartbeat_age_ms`,
    /// `armed`, `system_status`, `autopilot`.
    pub fn apply_heartbeat(&mut self, msg: &HEARTBEAT_DATA) {
        let safety_armed_bit = MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED.bits();
        let armed = (msg.base_mode.bits() & safety_armed_bit) != 0;
        self.last_heartbeat = Some(HeartbeatState {
            rx_at: Instant::now(),
            armed,
            system_status: msg.system_status as u32,
            autopilot: upstream_autopilot_to_proto(msg.autopilot),
        });
    }

    /// Ingest a GPS_RAW_INT -- updates `gps_fix_type`, `has_gps_fix`.
    pub fn apply_gps_raw_int(&mut self, msg: &GPS_RAW_INT_DATA) {
        self.last_gps = Some(GpsState {
            fix_type: msg.fix_type as u32,
        });
    }

    /// Ingest an ESTIMATOR_STATUS -- updates `ekf_flags`, `ekf_converged`.
    pub fn apply_estimator_status(&mut self, msg: &ESTIMATOR_STATUS_DATA) {
        self.last_ekf = Some(EkfState {
            flags: u32::from(msg.flags.bits()),
        });
    }

    /// Compute a fresh [`ReadinessState`] snapshot from the current builder state.
    #[must_use]
    pub fn snapshot(&self) -> ReadinessState {
        let now = Instant::now();
        let (heartbeat_alive, heartbeat_age_ms, armed, system_status, autopilot) =
            self.last_heartbeat
                .map_or((false, 0, false, 0, ProtoMavAutopilot::Unspecified as i32), |hb| {
                    let age = now.duration_since(hb.rx_at);
                    let alive = age <= HEARTBEAT_ALIVE_WINDOW;
                    (
                        alive,
                        u64::try_from(age.as_millis()).unwrap_or(u64::MAX),
                        hb.armed,
                        hb.system_status,
                        hb.autopilot as i32,
                    )
                });

        let (gps_fix_type, has_gps_fix) = self
            .last_gps
            .map_or((0, false), |gps| (gps.fix_type, gps.fix_type >= GPS_FIX_TYPE_3D_FIX));

        let (ekf_flags, ekf_converged) = self.last_ekf.map_or((0, false), |ekf| {
            (ekf.flags, (ekf.flags & EKF_CONVERGED_MASK) == EKF_CONVERGED_MASK)
        });

        let ready_to_arm = heartbeat_alive && has_gps_fix && ekf_converged && !armed;
        let fully_operational = ready_to_arm || (armed && ekf_converged && has_gps_fix && heartbeat_alive);

        ReadinessState {
            heartbeat_alive,
            heartbeat_age_ms,
            armed,
            system_status,
            gps_fix_type,
            has_gps_fix,
            ekf_flags,
            ekf_converged,
            ready_to_arm,
            fully_operational,
            autopilot,
        }
    }
}

/// Upstream [`UpstreamMavAutopilot`] -> proto [`ProtoMavAutopilot`].
///
/// Upstream uses wire values verbatim (`PX4 = 12`, `ARDUPILOTMEGA = 3`,
/// `GENERIC = 0`, `INVALID = 8`); proto uses the shifted/subset enumeration
/// from plan 25-03 (`Unspecified = 0`, `Generic = 1`, `Px4 = 2`,
/// `Ardupilotmega = 3`, `Invalid = 4`). Unknown upstream variants fall
/// through to `Invalid` per D-05'.
fn upstream_autopilot_to_proto(upstream: UpstreamMavAutopilot) -> ProtoMavAutopilot {
    // MAV_AUTOPILOT_INVALID collapses into the default `_ => Invalid` arm; we
    // keep only the three autopilots the proto explicitly enumerates and map
    // every other variant (SLUGS, OpenPilot, PPZ, ..., INVALID itself) to
    // `Invalid` -- consistent with D-05' "unknown vendor" semantics.
    match upstream {
        UpstreamMavAutopilot::MAV_AUTOPILOT_GENERIC => ProtoMavAutopilot::Generic,
        UpstreamMavAutopilot::MAV_AUTOPILOT_PX4 => ProtoMavAutopilot::Px4,
        UpstreamMavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA => ProtoMavAutopilot::Ardupilotmega,
        _ => ProtoMavAutopilot::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mavlink::common::{EstimatorStatusFlags, GpsFixType, MavState, MavType};

    fn make_heartbeat(armed: bool, autopilot: UpstreamMavAutopilot) -> HEARTBEAT_DATA {
        let base_mode = if armed {
            MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED
        } else {
            MavModeFlag::from_bits_truncate(0)
        };
        HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_QUADROTOR,
            autopilot,
            base_mode,
            system_status: MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        }
    }

    fn make_gps(fix_type: u8) -> GPS_RAW_INT_DATA {
        let fix = match fix_type {
            0 => GpsFixType::GPS_FIX_TYPE_NO_GPS,
            1 => GpsFixType::GPS_FIX_TYPE_NO_FIX,
            2 => GpsFixType::GPS_FIX_TYPE_2D_FIX,
            // 3 and higher all map to a 3D fix so has_gps_fix evaluates true.
            _ => GpsFixType::GPS_FIX_TYPE_3D_FIX,
        };
        GPS_RAW_INT_DATA {
            time_usec: 0,
            lat: 0,
            lon: 0,
            alt: 0,
            eph: 0,
            epv: 0,
            vel: 0,
            cog: 0,
            fix_type: fix,
            satellites_visible: 12,
        }
    }

    fn make_ekf(converged: bool) -> ESTIMATOR_STATUS_DATA {
        let flags = if converged {
            EstimatorStatusFlags::from_bits_truncate((EKF_CONVERGED_MASK & 0xFFFF) as u16)
        } else {
            EstimatorStatusFlags::from_bits_truncate(0)
        };
        ESTIMATOR_STATUS_DATA {
            time_usec: 0,
            vel_ratio: 0.0,
            pos_horiz_ratio: 0.0,
            pos_vert_ratio: 0.0,
            mag_ratio: 0.0,
            hagl_ratio: 0.0,
            tas_ratio: 0.0,
            pos_horiz_accuracy: 0.0,
            pos_vert_accuracy: 0.0,
            flags,
        }
    }

    #[test]
    fn empty_builder_is_not_ready() {
        let r = ReadinessBuilder::new().snapshot();
        assert!(!r.heartbeat_alive);
        assert!(!r.has_gps_fix);
        assert!(!r.ekf_converged);
        assert!(!r.ready_to_arm);
        assert!(!r.fully_operational);
        assert_eq!(r.autopilot, ProtoMavAutopilot::Unspecified as i32);
    }

    #[test]
    fn full_ready_state() {
        let mut b = ReadinessBuilder::new();
        b.apply_heartbeat(&make_heartbeat(false, UpstreamMavAutopilot::MAV_AUTOPILOT_PX4));
        b.apply_gps_raw_int(&make_gps(3));
        b.apply_estimator_status(&make_ekf(true));
        let r = b.snapshot();
        assert!(r.heartbeat_alive);
        assert!(r.has_gps_fix);
        assert_eq!(r.gps_fix_type, 3);
        assert!(r.ekf_converged);
        assert!(r.ready_to_arm);
        assert!(!r.armed);
        assert!(r.fully_operational); // ready_to_arm implies fully_operational
        assert_eq!(r.autopilot, ProtoMavAutopilot::Px4 as i32);
    }

    #[test]
    fn armed_and_converged_is_fully_operational_but_not_ready_to_arm() {
        let mut b = ReadinessBuilder::new();
        b.apply_heartbeat(&make_heartbeat(true, UpstreamMavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA));
        b.apply_gps_raw_int(&make_gps(3));
        b.apply_estimator_status(&make_ekf(true));
        let r = b.snapshot();
        assert!(r.armed);
        assert!(r.ekf_converged);
        assert!(r.has_gps_fix);
        assert!(!r.ready_to_arm, "armed flights are not ready_to_arm");
        assert!(
            r.fully_operational,
            "armed + converged + gps + heartbeat = fully_operational"
        );
        assert_eq!(r.autopilot, ProtoMavAutopilot::Ardupilotmega as i32);
    }

    #[test]
    fn not_ready_without_gps() {
        let mut b = ReadinessBuilder::new();
        b.apply_heartbeat(&make_heartbeat(false, UpstreamMavAutopilot::MAV_AUTOPILOT_PX4));
        b.apply_gps_raw_int(&make_gps(2)); // 2D_FIX -- insufficient
        b.apply_estimator_status(&make_ekf(true));
        let r = b.snapshot();
        assert!(!r.has_gps_fix);
        assert!(!r.ready_to_arm);
        assert!(!r.fully_operational);
    }

    #[test]
    fn not_ready_without_ekf_convergence() {
        let mut b = ReadinessBuilder::new();
        b.apply_heartbeat(&make_heartbeat(false, UpstreamMavAutopilot::MAV_AUTOPILOT_PX4));
        b.apply_gps_raw_int(&make_gps(3));
        b.apply_estimator_status(&make_ekf(false));
        let r = b.snapshot();
        assert!(r.has_gps_fix);
        assert!(!r.ekf_converged);
        assert!(!r.ready_to_arm);
    }

    #[test]
    fn heartbeat_ages_out_after_window() {
        // Immediately after apply, alive should be true. Full
        // window-expiry coverage requires live wall-time simulation
        // which is gated on plan 25-15's tlog-replay harness.
        let mut b = ReadinessBuilder::new();
        b.apply_heartbeat(&make_heartbeat(false, UpstreamMavAutopilot::MAV_AUTOPILOT_PX4));
        assert!(b.snapshot().heartbeat_alive);
    }

    #[test]
    fn unknown_autopilot_maps_to_invalid() {
        let mut b = ReadinessBuilder::new();
        b.apply_heartbeat(&make_heartbeat(false, UpstreamMavAutopilot::MAV_AUTOPILOT_RESERVED));
        let r = b.snapshot();
        assert_eq!(r.autopilot, ProtoMavAutopilot::Invalid as i32);
    }
}
