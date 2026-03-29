//! Edge deployment types for production robot operation.

pub mod clock;
pub mod ota;
pub mod power;
pub mod recovery;
pub mod vision;

/// Resolves the effective agent placement from the proto enum value.
///
/// - `AGENT_PLACEMENT_EDGE` (2) -> `true` (run on worker)
/// - `AGENT_PLACEMENT_CLOUD` (1) -> `false` (run on server)
/// - `AGENT_PLACEMENT_AUTO` (0) or unknown -> `false` (default to cloud)
///
/// AUTO defaults to Cloud for now. `OodaReAct` -> Edge auto-selection
/// will come when robot-type metadata is available.
pub const fn resolve_placement(placement: i32, _has_host: bool) -> bool {
    placement == 2 // AGENT_PLACEMENT_EDGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_placement_edge() {
        assert!(resolve_placement(2, true));
        assert!(resolve_placement(2, false));
    }

    #[test]
    fn resolve_placement_cloud() {
        assert!(!resolve_placement(1, true));
        assert!(!resolve_placement(1, false));
    }

    #[test]
    fn resolve_placement_auto_defaults_to_cloud() {
        assert!(!resolve_placement(0, true));
        assert!(!resolve_placement(0, false));
    }

    #[test]
    fn resolve_placement_unknown_defaults_to_cloud() {
        assert!(!resolve_placement(99, true));
        assert!(!resolve_placement(-1, false));
    }
}
