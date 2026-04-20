//! Thin wrapper over upstream `mavlink[signing]`.
//!
//! Per Phase 25 D-01' (post-research override), this module does NOT
//! reimplement HMAC-SHA256. It loads the 32-byte signing seed, allocates
//! link IDs per D-04, dispatches SETUP_SIGNING + tracks ACK, and surfaces
//! readiness degradation on ACK timeout (D-14). Upstream
//! `mavlink::SigningConfig::new(secret_key, link_id, sign_outgoing,
//! allow_unsigned)` owns the crypto primitives.
//!
//! Wave 1 plan `25-05-signing-wrapper` populates this module.
