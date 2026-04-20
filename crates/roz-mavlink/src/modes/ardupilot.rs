//! ArduCopter + ArduPlane mode tables.
//!
//! Source: ArduPilot `ArduCopter/mode.h` + `ArduPlane/mode.h`.
//! Verified against upstream as of 2026-04-19 per 25-RESEARCH.md §Code Examples.
//!
//! Unlike PX4, ArduPilot uses a single u8 mode (no main/sub split). The
//! `MAV_CMD_DO_SET_MODE` dispatch places it in param2 as a simple u32
//! (zero-extended).

// ============================================================
// ArduCopter
// ============================================================

#[must_use]
pub fn ardupilot_copter_mode_from_string(mode: &str) -> Option<u8> {
    match mode {
        "STABILIZE" => Some(0),
        "ACRO" => Some(1),
        "ALT_HOLD" => Some(2),
        "AUTO" => Some(3),
        "GUIDED" => Some(4),
        "LOITER" => Some(5),
        "RTL" => Some(6),
        "CIRCLE" => Some(7),
        // 8 = reserved (not assigned)
        "LAND" => Some(9),
        // 10 = reserved
        "DRIFT" => Some(11),
        "SPORT" => Some(13),
        "FLIP" => Some(14),
        "AUTOTUNE" => Some(15),
        "POSHOLD" => Some(16),
        "BRAKE" => Some(17),
        "THROW" => Some(18),
        "AVOID_ADSB" => Some(19),
        "GUIDED_NOGPS" => Some(20),
        "SMART_RTL" => Some(21),
        "FLOWHOLD" => Some(22),
        "FOLLOW" => Some(23),
        "ZIGZAG" => Some(24),
        "SYSTEMID" => Some(25),
        "AUTOROTATE" => Some(26),
        "AUTO_RTL" => Some(27),
        "TURTLE" => Some(28),
        _ => None,
    }
}

#[must_use]
pub fn ardupilot_copter_string_from_mode(mode: u8) -> Option<&'static str> {
    match mode {
        0 => Some("STABILIZE"),
        1 => Some("ACRO"),
        2 => Some("ALT_HOLD"),
        3 => Some("AUTO"),
        4 => Some("GUIDED"),
        5 => Some("LOITER"),
        6 => Some("RTL"),
        7 => Some("CIRCLE"),
        9 => Some("LAND"),
        11 => Some("DRIFT"),
        13 => Some("SPORT"),
        14 => Some("FLIP"),
        15 => Some("AUTOTUNE"),
        16 => Some("POSHOLD"),
        17 => Some("BRAKE"),
        18 => Some("THROW"),
        19 => Some("AVOID_ADSB"),
        20 => Some("GUIDED_NOGPS"),
        21 => Some("SMART_RTL"),
        22 => Some("FLOWHOLD"),
        23 => Some("FOLLOW"),
        24 => Some("ZIGZAG"),
        25 => Some("SYSTEMID"),
        26 => Some("AUTOROTATE"),
        27 => Some("AUTO_RTL"),
        28 => Some("TURTLE"),
        _ => None,
    }
}

// ============================================================
// ArduPlane
// ============================================================

#[must_use]
pub fn ardupilot_plane_mode_from_string(mode: &str) -> Option<u8> {
    match mode {
        "MANUAL" => Some(0),
        "CIRCLE" => Some(1),
        "STABILIZE" => Some(2),
        "TRAINING" => Some(3),
        "ACRO" => Some(4),
        "FLY_BY_WIRE_A" => Some(5),
        "FLY_BY_WIRE_B" => Some(6),
        "CRUISE" => Some(7),
        "AUTOTUNE" => Some(8),
        // 9 = reserved
        "AUTO" => Some(10),
        "RTL" => Some(11),
        "LOITER" => Some(12),
        "TAKEOFF" => Some(13),
        "AVOID_ADSB" => Some(14),
        "GUIDED" => Some(15),
        "INITIALISING" => Some(16),
        _ => None,
    }
}

#[must_use]
pub fn ardupilot_plane_string_from_mode(mode: u8) -> Option<&'static str> {
    match mode {
        0 => Some("MANUAL"),
        1 => Some("CIRCLE"),
        2 => Some("STABILIZE"),
        3 => Some("TRAINING"),
        4 => Some("ACRO"),
        5 => Some("FLY_BY_WIRE_A"),
        6 => Some("FLY_BY_WIRE_B"),
        7 => Some("CRUISE"),
        8 => Some("AUTOTUNE"),
        10 => Some("AUTO"),
        11 => Some("RTL"),
        12 => Some("LOITER"),
        13 => Some("TAKEOFF"),
        14 => Some("AVOID_ADSB"),
        15 => Some("GUIDED"),
        16 => Some("INITIALISING"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copter_guided_round_trip() {
        assert_eq!(ardupilot_copter_mode_from_string("GUIDED"), Some(4));
        assert_eq!(ardupilot_copter_string_from_mode(4), Some("GUIDED"));
    }

    #[test]
    fn copter_auto_rtl_integer_is_27() {
        // AUTO_RTL sits at 27 — notable because older firmware treated it as aliased to RTL.
        assert_eq!(ardupilot_copter_mode_from_string("AUTO_RTL"), Some(27));
        assert_eq!(ardupilot_copter_string_from_mode(27), Some("AUTO_RTL"));
    }

    #[test]
    fn copter_reserved_slots_return_none() {
        assert!(ardupilot_copter_string_from_mode(8).is_none());
        assert!(ardupilot_copter_string_from_mode(10).is_none());
        assert!(ardupilot_copter_string_from_mode(12).is_none());
    }

    #[test]
    fn plane_guided_round_trip() {
        assert_eq!(ardupilot_plane_mode_from_string("GUIDED"), Some(15));
        assert_eq!(ardupilot_plane_string_from_mode(15), Some("GUIDED"));
    }

    #[test]
    fn plane_auto_integer_is_10() {
        // Plane AUTO sits at 10, NOT 3 (Copter's AUTO) — classic confusion point.
        assert_eq!(ardupilot_plane_mode_from_string("AUTO"), Some(10));
    }

    #[test]
    fn plane_reserved_slot_9_returns_none() {
        assert!(ardupilot_plane_string_from_mode(9).is_none());
    }

    #[test]
    fn unknown_mode_returns_none() {
        assert!(ardupilot_copter_mode_from_string("WARP_SPEED").is_none());
        assert!(ardupilot_plane_mode_from_string("WARP_SPEED").is_none());
    }
}
