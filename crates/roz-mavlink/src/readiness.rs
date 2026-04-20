//! `ReadinessState` builder from MAVLink HEARTBEAT (0) / GPS_RAW_INT (24) /
//! ESTIMATOR_STATUS (230) messages.
//!
//! Produces `substrate.sim.v2.ReadinessState` (the v2 proto type — adds
//! `MavAutopilot autopilot` field vs v1 per D-09).
//!
//! Wave 1 plan `25-07-readiness-builder` populates this module.
