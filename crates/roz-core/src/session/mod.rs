//! Session runtime types — events, snapshots, activity states, control modes,
//! and operator feedback.

pub mod activity;
pub mod control;
pub mod event;
pub mod feedback;
pub mod snapshot;

// Stub modules exported for consistent public API; items populated by subsequent tasks.
#[allow(unused_imports)]
pub use activity::*;
#[allow(unused_imports)]
pub use control::*;
#[allow(unused_imports)]
pub use event::*;
#[allow(unused_imports)]
pub use feedback::*;
#[allow(unused_imports)]
pub use snapshot::*;
