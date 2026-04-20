//! MAVLink transport adapters (serial / UDP) on top of upstream
//! `mavlink::connect`.
//!
//! Wave 1 plan `25-06-transports` populates submodules.

pub mod serial;
pub mod udp;
