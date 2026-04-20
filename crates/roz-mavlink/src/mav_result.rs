//! Wire-boundary `MavResult` shift helpers.
//!
//! Per Phase 25 D-08' (post-research reconciliation), v2 proto `MavResult`
//! uses a proto3-safe shift vs. MAVLink's wire values:
//!
//! | MAVLink wire | v2 proto value           | roz_copper::io::MavResult      |
//! |-------------:|--------------------------|--------------------------------|
//! | (n/a)        | MAV_RESULT_UNSPECIFIED=0 | (n/a - sentinel only)          |
//! | 0            | MAV_RESULT_ACCEPTED=1    | MavResult::Accepted            |
//! | 1            | ...TEMPORARILY_REJECTED=2| MavResult::TemporarilyRejected |
//! | 2            | MAV_RESULT_DENIED=3      | MavResult::Denied              |
//! | 3            | MAV_RESULT_UNSUPPORTED=4 | MavResult::Unsupported         |
//! | 4            | MAV_RESULT_FAILED=5      | MavResult::Failed              |
//! | 5            | MAV_RESULT_IN_PROGRESS=6 | MavResult::InProgress          |
//! | 6            | MAV_RESULT_CANCELLED=7   | MavResult::Cancelled           |
//!
//! This module provides:
//! * [`mav_result_from_wire`] - inbound MAVLink `u8` → `proto_v2::MavResult`.
//!   Unknown wire values map to `MavResult::Unspecified` sentinel.
//! * [`mav_result_to_wire`] - outbound `proto_v2::MavResult` → `Option<u8>`.
//!   Returns `None` for the sentinel (must never be emitted from the backend).
//! * [`io_mav_result_from_wire`] - inbound MAVLink `u8` → `roz_copper::io::MavResult`
//!   (the non-proto Rust enum used by `DiscreteCommandSink<FlightCommand>::send_command`).
//!   Unknown wire values map to `MavResult::Failed` (fail-closed).
//!
//! Callers SHOULD prefer `io_mav_result_from_wire` inside the backend's
//! `DiscreteCommandSink<FlightCommand>::send_command` path (the trait signature
//! takes the Rust enum, not the proto one). The proto helpers are for when the
//! backend emits a `FlightCommandResponse` proto message directly (gRPC
//! streaming path, not in Phase 25 scope).

use roz_copper::io::MavResult as IoMavResult;
use roz_copper::proto_v2::MavResult as ProtoMavResult;

/// Convert an inbound MAVLink wire value (`0..=6`) to the proto3 shifted enum.
///
/// Unknown wire values → `MavResult::Unspecified` (proto3 sentinel).
/// Consumers treat the sentinel as an unparseable response and surface it as
/// an error.
#[must_use]
pub fn mav_result_from_wire(wire: u8) -> ProtoMavResult {
    match wire {
        0 => ProtoMavResult::Accepted,
        1 => ProtoMavResult::TemporarilyRejected,
        2 => ProtoMavResult::Denied,
        3 => ProtoMavResult::Unsupported,
        4 => ProtoMavResult::Failed,
        5 => ProtoMavResult::InProgress,
        6 => ProtoMavResult::Cancelled,
        _ => ProtoMavResult::Unspecified,
    }
}

/// Convert a `roz_copper::proto_v2::MavResult` back to the MAVLink wire value.
///
/// Returns `None` for the `Unspecified` sentinel — the backend MUST
/// never emit the sentinel on the wire. Callers should log + fail-closed
/// when they see `None`.
#[must_use]
pub fn mav_result_to_wire(proto: ProtoMavResult) -> Option<u8> {
    match proto {
        ProtoMavResult::Unspecified => None,
        ProtoMavResult::Accepted => Some(0),
        ProtoMavResult::TemporarilyRejected => Some(1),
        ProtoMavResult::Denied => Some(2),
        ProtoMavResult::Unsupported => Some(3),
        ProtoMavResult::Failed => Some(4),
        ProtoMavResult::InProgress => Some(5),
        ProtoMavResult::Cancelled => Some(6),
    }
}

/// Convert an inbound MAVLink wire value to the `roz_copper::io::MavResult`
/// Rust enum used by `DiscreteCommandSink<FlightCommand>::send_command`.
///
/// Unknown wire values → `MavResult::Failed` (fail-closed — better than
/// silently masquerading as `Accepted`).
#[must_use]
#[expect(
    clippy::match_same_arms,
    reason = "wire=4 and unknown wire both map to Failed by design — explicit 4 arm documents the \
              MAV_RESULT_FAILED wire value; wildcard is the fail-closed policy for unknowns"
)]
pub fn io_mav_result_from_wire(wire: u8) -> IoMavResult {
    match wire {
        0 => IoMavResult::Accepted,
        1 => IoMavResult::TemporarilyRejected,
        2 => IoMavResult::Denied,
        3 => IoMavResult::Unsupported,
        4 => IoMavResult::Failed,
        5 => IoMavResult::InProgress,
        6 => IoMavResult::Cancelled,
        _ => IoMavResult::Failed,
    }
}

/// Convert `roz_copper::io::MavResult` to the proto3 shifted enum.
///
/// Infallible (no sentinel case in the Rust enum).
#[must_use]
pub fn proto_from_io(io: IoMavResult) -> ProtoMavResult {
    match io {
        IoMavResult::Accepted => ProtoMavResult::Accepted,
        IoMavResult::TemporarilyRejected => ProtoMavResult::TemporarilyRejected,
        IoMavResult::Denied => ProtoMavResult::Denied,
        IoMavResult::Unsupported => ProtoMavResult::Unsupported,
        IoMavResult::Failed => ProtoMavResult::Failed,
        IoMavResult::InProgress => ProtoMavResult::InProgress,
        IoMavResult::Cancelled => ProtoMavResult::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip_through_proto() {
        for wire in 0u8..=6u8 {
            let proto = mav_result_from_wire(wire);
            assert_ne!(proto, ProtoMavResult::Unspecified);
            let back = mav_result_to_wire(proto).expect("non-sentinel → Some(wire)");
            assert_eq!(back, wire, "wire {wire} round-trip");
        }
    }

    #[test]
    fn unknown_wire_maps_to_unspecified_in_proto() {
        assert_eq!(mav_result_from_wire(42), ProtoMavResult::Unspecified);
        assert_eq!(mav_result_from_wire(255), ProtoMavResult::Unspecified);
    }

    #[test]
    fn unknown_wire_maps_to_failed_in_io() {
        assert_eq!(io_mav_result_from_wire(42), IoMavResult::Failed);
        assert_eq!(io_mav_result_from_wire(255), IoMavResult::Failed);
    }

    #[test]
    fn proto_sentinel_has_no_wire_value() {
        assert!(mav_result_to_wire(ProtoMavResult::Unspecified).is_none());
    }

    #[test]
    fn io_accepted_is_wire_zero() {
        // D-08' anchor: MAVLink's MAV_RESULT_ACCEPTED is wire value 0.
        assert_eq!(io_mav_result_from_wire(0), IoMavResult::Accepted);
    }

    #[test]
    fn io_to_proto_round_trip() {
        for io in [
            IoMavResult::Accepted,
            IoMavResult::TemporarilyRejected,
            IoMavResult::Denied,
            IoMavResult::Unsupported,
            IoMavResult::Failed,
            IoMavResult::InProgress,
            IoMavResult::Cancelled,
        ] {
            let proto = proto_from_io(io);
            let wire = mav_result_to_wire(proto).expect("non-sentinel → Some(wire)");
            assert_eq!(io_mav_result_from_wire(wire), io, "io round-trip via proto+wire");
        }
    }
}
