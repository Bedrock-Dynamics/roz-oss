//! Canonical embodiment model — physical structure, frame tree, calibration,
//! safety overlays, workspace zones, and control interface bindings.

pub mod binding;
pub mod calibration;
pub mod contact;
pub mod embodiment_runtime;
pub mod frame_snapshot;
pub mod frame_tree;
pub mod limits;
pub mod model;
pub mod perception;
pub mod prediction;
pub mod retargeting;
pub mod safety_overlay;
// Phase 26.10 Plan 08 (FW-07) — manipulator_runtime helper. Gated `test-fixtures`
// so production binaries do not link the helper. Tests within roz-core enable it
// implicitly via the `cfg(test)` clause; downstream crates (roz-copper, roz-worker)
// enable it through `roz-core/test-fixtures` feature propagation.
#[cfg(any(test, feature = "test-fixtures"))]
pub mod test_fixtures;
pub mod workspace;

// Stub modules are exported here for consistent public API; items will be populated by
// subsequent tasks. Allow unused imports until the stubs are filled in.
#[allow(unused_imports)]
pub use binding::*;
#[allow(unused_imports)]
pub use calibration::*;
#[allow(unused_imports)]
pub use contact::*;
#[allow(unused_imports)]
pub use embodiment_runtime::*;
#[allow(unused_imports)]
pub use frame_snapshot::*;
#[allow(unused_imports)]
pub use frame_tree::*;
pub use limits::*;
#[allow(unused_imports)]
pub use model::*;
#[allow(unused_imports)]
pub use perception::*;
#[allow(unused_imports)]
pub use prediction::*;
#[allow(unused_imports)]
pub use retargeting::*;
#[allow(unused_imports)]
pub use safety_overlay::*;
pub use workspace::*;
