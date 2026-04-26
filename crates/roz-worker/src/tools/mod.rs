//! FW-03 — Worker-side controller lifecycle tools (promote, stop, status).
//!
//! Mirrors `crates/roz-local/src/tools/{promote,stop,controller_status}_controller.rs`.
//! Drift between these mirrors is a bug — both must register the same
//! [`roz_copper::channels::ControllerCommand`] shapes and the same canonical
//! tool-name strings.
//!
//! Canonical registered tool names (must match roz-local exactly):
//!   - `promote_controller`
//!   - `stop_controller`
//!   - `controller_status`   (NOT `get_controller_status` — Codex review naming drift fix)

pub mod controller_status;
pub mod promote_controller;
pub mod stop_controller;
