//! Re-export of `CopperHandle` from `roz-copper`.
//!
//! The implementation lives in `roz_copper::handle` so that `roz-local` can
//! use it without creating a circular dependency (`roz-worker` depends on
//! `roz-local`, so `roz-local` cannot depend on `roz-worker`).

pub use roz_copper::handle::CopperHandle;
