//! Safety-telemetry event schemas published on the `safety.*` NATS
//! subject tree. Kept in `roz-nats` so producers (roz-worker) and
//! consumers (roz-safety) share a single definition.

use serde::{Deserialize, Serialize};

/// Event payload published on `safety.trust_failure.{worker_id}` when
/// a `.cwasm` signature fails verification. Complements `tracing::error!`.
///
/// Field semantics mirror `roz_copper::wasm_signature::WasmLoadError`
/// but this type does NOT depend on roz-copper (avoids cycles) — the
/// worker translates the error into this event at the publish site.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WasmTrustFailure {
    pub worker_id: String,
    /// Empty or "<unknown>" when the failure happened before `key_id`
    /// parsing (e.g. envelope decode failure).
    pub key_id: String,
    /// "<unknown>" when `module_id` was not successfully parsed.
    pub module_id: String,
    /// "<unknown>" when `version` was not successfully parsed.
    pub version: String,
    /// Short static reason string (e.g. "ed25519 verify failed",
    /// "cwasm sha256 mismatch with signed manifest").
    pub reason: String,
    /// RFC3339 UTC timestamp of the failure.
    pub occurred_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasm_trust_failure_round_trips_through_json() {
        let evt = WasmTrustFailure {
            worker_id: "w1".into(),
            key_id: "k1".into(),
            module_id: "m1".into(),
            version: "1.2.3".into(),
            reason: "ed25519 verify failed".into(),
            occurred_at: "2026-04-12T00:00:00Z".into(),
        };
        let bytes = serde_json::to_vec(&evt).unwrap();
        let back: WasmTrustFailure = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn wasm_trust_failure_from_unknown_key_id_fills_unknowns() {
        let evt = WasmTrustFailure {
            worker_id: "w1".into(),
            key_id: "rotated".into(),
            module_id: "<unknown>".into(),
            version: "<unknown>".into(),
            reason: "unknown key_id".into(),
            occurred_at: "2026-04-12T00:00:00Z".into(),
        };
        assert_eq!(evt.module_id, "<unknown>");
        assert_eq!(evt.version, "<unknown>");
        assert_eq!(evt.key_id, "rotated");
    }
}
