//! Canonical embodiment model — physical structure, frame tree, calibration,
//! safety overlays, workspace zones, and control interface bindings.

pub mod binding;
pub mod calibration;
pub mod contact;
pub mod frame_tree;
pub mod limits;
pub mod model;
pub mod perception;
pub mod prediction;
pub mod safety_overlay;
pub mod workspace;

pub use binding::*;
pub use calibration::*;
pub use contact::*;
pub use frame_tree::*;
pub use limits::*;
pub use model::*;
pub use perception::*;
pub use prediction::*;
pub use safety_overlay::*;
pub use workspace::*;
