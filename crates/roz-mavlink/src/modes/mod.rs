//! Vendor-specific MAVLink mode integer ↔ string translation tables.
//!
//! Source-of-truth: PX4 `src/modules/commander/px4_custom_mode.h` +
//! ArduPilot `ArduCopter/mode.h` / `ArduPlane/mode.h`. Reproduced
//! verbatim in Rust; see 25-RESEARCH.md §Code Examples.
//!
//! Wave 1 plan `25-08-modes-tables` populates submodules.

pub mod ardupilot;
pub mod px4;
