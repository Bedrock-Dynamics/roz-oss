//! `MavlinkBackend` — implements `SensorSource + ActuatorSink + DiscreteCommandSink<FlightCommand>`.
//!
//! Wave 2 plan `25-12-backend-assembly` populates this module. Wave 1 modules
//! ([`crate::signing`], [`crate::transport`], [`crate::readiness`],
//! [`crate::flight_command`], [`crate::modes`]) produce the helpers this
//! module assembles.
