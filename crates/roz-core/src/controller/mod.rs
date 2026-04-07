//! Controller types — artifacts, evidence bundles, deployment state machine,
//! safety interventions, and verification.

pub mod artifact;
pub mod deployment;
pub mod evidence;
pub mod intervention;
pub mod verification;

// Stub modules exported for consistent public API; items populated by subsequent tasks.
#[allow(unused_imports)]
pub use artifact::*;
#[allow(unused_imports)]
pub use deployment::*;
#[allow(unused_imports)]
pub use evidence::*;
#[allow(unused_imports)]
pub use intervention::*;
#[allow(unused_imports)]
pub use verification::*;
