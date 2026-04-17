//! Canonical reasoning-content tagging emitted by provider stream parsers.
//!
//! `AgentLoop` consumes [`ThinkingConfig`] — not provider-specific reasoning
//! types. This module also exposes the cross-turn stripping helper that
//! downstream provider history serializers (Plan 19-07 and Plan 19-10) MUST
//! invoke before re-sending prior assistant turns.

use serde::{Deserialize, Serialize};

/// Canonical reasoning-content tagging emitted by provider stream parsers.
/// `AgentLoop` consumes THIS enum — not provider-specific reasoning types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// Provider signs and re-sends reasoning verbatim (Anthropic native).
    Signed,
    /// Reasoning is tagged in-content (e.g. `<think>…</think>`, DeepSeek, QwQ).
    /// UNSIGNED — must NOT be re-sent cross-turn. `open_tag` / `close_tag`
    /// record the literal markers for downstream stripping.
    UnsignedTagged { open_tag: String, close_tag: String },
    /// No reasoning segment.
    None,
}

/// A minimal cross-turn transcript representation used by the test surface.
///
/// Real provider history types stay in `roz-agent`; this is the type-level
/// contract that Plan 19-10 must satisfy when serializing prior assistant
/// turns into a new request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTurn {
    pub text: String,
    pub thinking: ThinkingConfig,
    /// The raw reasoning segment as emitted by the model on this turn (may be
    /// empty when `thinking == None`). Held SEPARATELY from `text` so the
    /// stripper can drop it without touching user-visible content.
    pub reasoning_segment: String,
}

/// Strip [`ThinkingConfig::UnsignedTagged`] reasoning segments before cross-turn re-send.
///
/// [`ThinkingConfig::Signed`] reasoning is preserved (Anthropic requires
/// verbatim re-send). [`ThinkingConfig::None`] is a no-op.
///
/// SC2 / OWM-04 contract: returned [`AssistantTurn`]s NEVER contain a
/// non-empty `reasoning_segment` when their `thinking ==
/// ThinkingConfig::UnsignedTagged { .. }`.
///
/// Used by Plan 19-07 (provider history assembly) and Plan 19-10
/// (`CompletionResponse` → next-turn input). Both plans MUST call this before
/// serializing prior assistant turns into a new request.
#[must_use]
pub fn strip_unsigned_for_cross_turn(turns: &[AssistantTurn]) -> Vec<AssistantTurn> {
    turns
        .iter()
        .map(|t| {
            let mut out = t.clone();
            match &t.thinking {
                ThinkingConfig::UnsignedTagged { .. } => {
                    out.reasoning_segment = String::new();
                }
                ThinkingConfig::None => {
                    if !t.reasoning_segment.is_empty() {
                        // Mis-tagged turn: caller declared no reasoning yet
                        // attached a segment. Surface to logs to aid debugging
                        // (not a hard error — defense in depth only).
                        tracing::warn!(
                            target: "roz_core::thinking",
                            "AssistantTurn has thinking=None but non-empty reasoning_segment; \
                             this is a programmer error and the segment will be preserved as-is."
                        );
                    }
                }
                ThinkingConfig::Signed => {
                    // Preserve verbatim — Anthropic requires re-sending signed
                    // reasoning blocks for multi-turn extended thinking.
                }
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_tagged_roundtrips_serde() {
        let cfg = ThinkingConfig::UnsignedTagged {
            open_tag: "<think>".into(),
            close_tag: "</think>".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ThinkingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn signed_roundtrips_serde() {
        let cfg = ThinkingConfig::Signed;
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ThinkingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn none_roundtrips_serde() {
        let cfg = ThinkingConfig::None;
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ThinkingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn unsigned_tagged_not_equal_to_none() {
        let a = ThinkingConfig::UnsignedTagged {
            open_tag: "<think>".into(),
            close_tag: "</think>".into(),
        };
        let b = ThinkingConfig::None;
        assert_ne!(a, b);
    }

    // -----------------------------------------------------------------------
    // OWM-04 / SC2 — cross-turn non-resend tests (B6 fix).
    // -----------------------------------------------------------------------

    #[test]
    fn unsigned_not_resent_cross_turn() {
        let turn_n = AssistantTurn {
            text: "I'll proceed.".into(),
            thinking: ThinkingConfig::UnsignedTagged {
                open_tag: "<think>".into(),
                close_tag: "</think>".into(),
            },
            reasoning_segment: "INTERNAL_REASONING_MARKER".into(),
        };
        let history = vec![turn_n];
        let next_turn_history = strip_unsigned_for_cross_turn(&history);
        assert_eq!(next_turn_history.len(), 1);
        assert!(
            next_turn_history[0].reasoning_segment.is_empty(),
            "OWM-04 / SC2: UnsignedTagged reasoning MUST be stripped before cross-turn re-send"
        );
        // Defense in depth: the marker must not survive any serialization of
        // the history (text or full-turn).
        let serialized_text = serde_json::to_string(&next_turn_history[0].text).unwrap();
        assert!(
            !serialized_text.contains("INTERNAL_REASONING_MARKER"),
            "OWM-04 / SC2: marker leaked through text serialization"
        );
        let serialized_full = serde_json::to_string(&next_turn_history[0]).unwrap();
        assert!(
            !serialized_full.contains("INTERNAL_REASONING_MARKER"),
            "OWM-04 / SC2: marker leaked through full-turn serialization"
        );
    }

    #[test]
    fn signed_preserved_cross_turn() {
        let turn_n = AssistantTurn {
            text: "Continuing.".into(),
            thinking: ThinkingConfig::Signed,
            reasoning_segment: "SIGNED_REASONING_BLOCK".into(),
        };
        let history = vec![turn_n.clone()];
        let next_turn_history = strip_unsigned_for_cross_turn(&history);
        assert_eq!(next_turn_history.len(), 1);
        assert_eq!(
            next_turn_history[0].reasoning_segment, "SIGNED_REASONING_BLOCK",
            "Signed reasoning must be preserved verbatim (Anthropic re-send requirement)"
        );
        assert_eq!(next_turn_history[0], turn_n);
    }

    #[test]
    fn none_passthrough_cross_turn() {
        let turn_n = AssistantTurn {
            text: "Hello.".into(),
            thinking: ThinkingConfig::None,
            reasoning_segment: String::new(),
        };
        let history = vec![turn_n.clone()];
        let next_turn_history = strip_unsigned_for_cross_turn(&history);
        assert_eq!(next_turn_history.len(), 1);
        assert_eq!(next_turn_history[0], turn_n);
    }
}
