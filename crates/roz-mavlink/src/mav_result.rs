//! `MavResult` wire-boundary helpers.
//!
//! Per D-08' (post-research amendment), v2 proto `MavResult` uses a proto3-
//! safe shift: `MAV_RESULT_UNSPECIFIED=0, ACCEPTED=1, ..., CANCELLED=7`.
//! MAVLink's wire `MAV_RESULT` uses `ACCEPTED=0..CANCELLED=6`. This module
//! provides `mav_result_from_wire(u8) -> MavResult` and
//! `mav_result_to_wire(MavResult) -> u8` — a one-step shift.
//!
//! The `MavResult` Rust enum itself (no UNSPECIFIED variant) lives in
//! `roz_copper::io` so `DiscreteCommandSink<FlightCommand>::send` can return it without
//! pulling a proto dep. The proto3 enum lives in the v2 proto (plan 25-03).
//!
//! Wave 1 plan `25-05-signing-wrapper` populates this module (paired
//! because the scope is tiny — two helper functions).
