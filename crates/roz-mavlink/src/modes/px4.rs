//! PX4 main + sub mode tables.
//!
//! Source: PX4-Autopilot `src/modules/commander/px4_custom_mode.h`.
//! Verified against upstream as of 2026-04-19 per 25-RESEARCH.md §Code Examples.
//!
//! Consumed by `crate::flight_command` (plan 25-09) when dispatching
//! `MAV_CMD_DO_SET_MODE` — param2 takes the packed u32 custom_mode.

// Main mode values — upper 8 bits of the custom_mode u32.
pub const PX4_MAIN_MANUAL: u8 = 1;
pub const PX4_MAIN_ALTCTL: u8 = 2;
pub const PX4_MAIN_POSCTL: u8 = 3;
pub const PX4_MAIN_AUTO: u8 = 4;
pub const PX4_MAIN_ACRO: u8 = 5;
pub const PX4_MAIN_OFFBOARD: u8 = 6;
pub const PX4_MAIN_STABILIZED: u8 = 7;
/// Renamed RATTITUDE_LEGACY in upstream; retained as deprecated-but-valid.
pub const PX4_MAIN_RATTITUDE: u8 = 8;
pub const PX4_MAIN_SIMPLE: u8 = 9;
pub const PX4_MAIN_TERMINATION: u8 = 10;
pub const PX4_MAIN_ALT_CRUISE: u8 = 11;

// Sub-mode values for AUTO (main_mode == 4) — byte 2 of custom_mode.
pub const PX4_SUB_AUTO_READY: u8 = 1;
pub const PX4_SUB_AUTO_TAKEOFF: u8 = 2;
pub const PX4_SUB_AUTO_LOITER: u8 = 3;
pub const PX4_SUB_AUTO_MISSION: u8 = 4;
pub const PX4_SUB_AUTO_RTL: u8 = 5;
pub const PX4_SUB_AUTO_LAND: u8 = 6;
// 7 = RESERVED_DO_NOT_USE (was RTGS, removed upstream 2020-03-05).
pub const PX4_SUB_AUTO_FOLLOW_TARGET: u8 = 8;
pub const PX4_SUB_AUTO_PRECLAND: u8 = 9;
pub const PX4_SUB_AUTO_VTOL_TAKEOFF: u8 = 10;

/// Translate a canonical PX4 mode string to `(main, sub)`.
/// Returns `None` for unrecognized strings.
///
/// Canonical strings are ALL-CAPS with optional `.` separator for AUTO sub-modes
/// (e.g. `"AUTO.TAKEOFF"`, `"OFFBOARD"`). Case-insensitive lookup is NOT
/// provided — callers normalize upstream.
#[must_use]
pub fn px4_mode_from_string(mode: &str) -> Option<(u8, u8)> {
    match mode {
        "MANUAL" => Some((PX4_MAIN_MANUAL, 0)),
        "ALTCTL" => Some((PX4_MAIN_ALTCTL, 0)),
        "POSCTL" => Some((PX4_MAIN_POSCTL, 0)),
        "OFFBOARD" => Some((PX4_MAIN_OFFBOARD, 0)),
        "STABILIZED" => Some((PX4_MAIN_STABILIZED, 0)),
        "ACRO" => Some((PX4_MAIN_ACRO, 0)),
        "SIMPLE" => Some((PX4_MAIN_SIMPLE, 0)),
        "TERMINATION" => Some((PX4_MAIN_TERMINATION, 0)),
        "ALT_CRUISE" => Some((PX4_MAIN_ALT_CRUISE, 0)),
        "RATTITUDE" => Some((PX4_MAIN_RATTITUDE, 0)),
        "AUTO.READY" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_READY)),
        "AUTO.TAKEOFF" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_TAKEOFF)),
        "AUTO.LOITER" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_LOITER)),
        "AUTO.MISSION" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_MISSION)),
        "AUTO.RTL" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_RTL)),
        "AUTO.LAND" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_LAND)),
        "AUTO.FOLLOW_TARGET" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_FOLLOW_TARGET)),
        "AUTO.PRECLAND" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_PRECLAND)),
        "AUTO.VTOL_TAKEOFF" => Some((PX4_MAIN_AUTO, PX4_SUB_AUTO_VTOL_TAKEOFF)),
        _ => None,
    }
}

/// Inverse of [`px4_mode_from_string`]. Returns the canonical ALL-CAPS string,
/// or `None` for integer pairs outside the known map.
#[must_use]
pub fn px4_string_from_mode(main: u8, sub: u8) -> Option<&'static str> {
    match (main, sub) {
        (PX4_MAIN_MANUAL, _) => Some("MANUAL"),
        (PX4_MAIN_ALTCTL, _) => Some("ALTCTL"),
        (PX4_MAIN_POSCTL, _) => Some("POSCTL"),
        (PX4_MAIN_OFFBOARD, _) => Some("OFFBOARD"),
        (PX4_MAIN_STABILIZED, _) => Some("STABILIZED"),
        (PX4_MAIN_ACRO, _) => Some("ACRO"),
        (PX4_MAIN_SIMPLE, _) => Some("SIMPLE"),
        (PX4_MAIN_TERMINATION, _) => Some("TERMINATION"),
        (PX4_MAIN_ALT_CRUISE, _) => Some("ALT_CRUISE"),
        (PX4_MAIN_RATTITUDE, _) => Some("RATTITUDE"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_READY) => Some("AUTO.READY"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_TAKEOFF) => Some("AUTO.TAKEOFF"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_LOITER) => Some("AUTO.LOITER"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_MISSION) => Some("AUTO.MISSION"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_RTL) => Some("AUTO.RTL"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_LAND) => Some("AUTO.LAND"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_FOLLOW_TARGET) => Some("AUTO.FOLLOW_TARGET"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_PRECLAND) => Some("AUTO.PRECLAND"),
        (PX4_MAIN_AUTO, PX4_SUB_AUTO_VTOL_TAKEOFF) => Some("AUTO.VTOL_TAKEOFF"),
        _ => None,
    }
}

/// Pack `(main, sub)` into the u32 custom_mode wire format.
///
/// PX4 layout:
/// * byte 3 (MSB): `main`
/// * byte 2:       `sub`
/// * bytes 1-0:    reserved (0)
///
/// Source: PX4-Autopilot `src/modules/commander/px4_custom_mode.h`.
#[must_use]
pub const fn px4_pack_custom_mode(main: u8, sub: u8) -> u32 {
    ((main as u32) << 16) | ((sub as u32) << 24)
}

/// Convenience: canonical string → packed u32 custom_mode for
/// `MAV_CMD_DO_SET_MODE` param2.
#[must_use]
pub fn px4_custom_mode_from_string(mode: &str) -> Option<u32> {
    px4_mode_from_string(mode).map(|(m, s)| px4_pack_custom_mode(m, s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offboard_round_trip() {
        let (main, sub) = px4_mode_from_string("OFFBOARD").unwrap();
        assert_eq!(main, PX4_MAIN_OFFBOARD);
        assert_eq!(sub, 0);
        assert_eq!(px4_string_from_mode(main, sub), Some("OFFBOARD"));
    }

    #[test]
    fn auto_takeoff_round_trip() {
        let (main, sub) = px4_mode_from_string("AUTO.TAKEOFF").unwrap();
        assert_eq!(main, PX4_MAIN_AUTO);
        assert_eq!(sub, PX4_SUB_AUTO_TAKEOFF);
        assert_eq!(px4_string_from_mode(main, sub), Some("AUTO.TAKEOFF"));
    }

    #[test]
    fn pack_layout_matches_spec() {
        // main=6 (OFFBOARD), sub=0: byte 3 = 6 → 0x0006_0000
        assert_eq!(px4_pack_custom_mode(6, 0), 0x0006_0000);
        // main=4 (AUTO), sub=2 (TAKEOFF): byte 3 = 4, byte 2 = 2 → 0x0204_0000
        assert_eq!(px4_pack_custom_mode(4, 2), 0x0204_0000);
    }

    #[test]
    fn custom_mode_from_string_is_composition() {
        assert_eq!(px4_custom_mode_from_string("OFFBOARD"), Some(0x0006_0000));
        assert_eq!(px4_custom_mode_from_string("AUTO.RTL"), Some(0x0504_0000));
    }

    #[test]
    fn unknown_mode_returns_none() {
        assert!(px4_mode_from_string("QUANTUM_FLIGHT").is_none());
        assert!(px4_custom_mode_from_string("QUANTUM_FLIGHT").is_none());
    }
}
