//! Edge deployment types for production robot operation.

pub mod clock;
pub mod ota;
pub mod power;
pub mod recovery;
pub mod vision;

/// Resolves the effective agent placement.
///
/// AUTO defaults to edge when the session has both a host and the host's
/// worker is controller-capable (capability advertisement includes
/// joints/controller surfaces) — per Phase 26.10 FW-04 / CONTEXT-locked
/// decision. Codex M2 fix: use host capability instead of session-start
/// tool list to avoid timing issues with worker/server-registered physical
/// tools.
///
/// `placement` semantics (matches `roz_v1::AgentPlacement`):
///   - 0 (or unknown) — AUTO: edge if (`has_host` AND `controller_capable_worker`), else cloud
///   - 1 — CLOUD: always cloud
///   - 2 — EDGE: always edge
#[must_use]
pub const fn resolve_placement(placement: i32, has_host: bool, controller_capable_worker: bool) -> bool {
    match placement {
        2 => true,                                  // EDGE explicit
        1 => false,                                 // CLOUD explicit
        _ => has_host && controller_capable_worker, // AUTO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EDGE explicit (placement == 2) routes edge regardless of host or capability.
    #[test]
    fn resolve_placement_edge_explicit() {
        assert!(resolve_placement(2, false, false));
        assert!(resolve_placement(2, true, true));
        assert!(resolve_placement(2, true, false));
        assert!(resolve_placement(2, false, true));
    }

    /// CLOUD explicit (placement == 1) routes cloud regardless of host or capability.
    #[test]
    fn resolve_placement_cloud_explicit() {
        assert!(!resolve_placement(1, true, true));
        assert!(!resolve_placement(1, false, false));
        assert!(!resolve_placement(1, true, false));
        assert!(!resolve_placement(1, false, true));
    }

    /// AUTO with no host and no capability → cloud (default).
    #[test]
    fn resolve_placement_auto_no_host_no_capability_cloud() {
        assert!(!resolve_placement(0, false, false));
    }

    /// AUTO with host but worker lacks controller capability → cloud.
    /// **Pitfall 5 regression** — host alone is not enough to route edge.
    #[test]
    fn resolve_placement_auto_host_no_capability_cloud() {
        assert!(!resolve_placement(0, true, false));
    }

    /// AUTO with no host (capability flag set is meaningless without host) → cloud.
    #[test]
    fn resolve_placement_auto_no_host_capable_cloud() {
        assert!(!resolve_placement(0, false, true));
    }

    /// AUTO with both host and controller-capable worker → edge.
    #[test]
    fn resolve_placement_auto_host_and_capable_edge() {
        assert!(resolve_placement(0, true, true));
    }

    /// Unknown placement values fall through the AUTO branch.
    #[test]
    fn resolve_placement_unknown_falls_through_to_auto() {
        // Unknown + capable + host → edge (AUTO semantics)
        assert!(resolve_placement(99, true, true));
        // Unknown + missing host → cloud
        assert!(!resolve_placement(99, false, true));
        assert!(!resolve_placement(-1, false, false));
    }
}
