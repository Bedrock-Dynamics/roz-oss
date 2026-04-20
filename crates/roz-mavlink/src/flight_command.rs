//! `DiscreteCommandSink<FlightCommand>` dispatch — discrete ARM/DISARM/TAKEOFF/LAND/RTL/
//! SET_MODE/GOTO commands (Phase 25 D-19).
//!
//! Maps each `roz_copper::io::FlightCommand` variant to the canonical
//! MAV_CMD_* + param1..7 layout (see 25-RESEARCH.md §Code Examples).
//! Non-zero `vehicle_index` short-circuits to `MavResult::Unsupported`
//! per D-16.
//!
//! Wave 1 plan `25-09-flight-command-module` populates this module.
