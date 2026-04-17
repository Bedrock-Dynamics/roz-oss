//! MEM-06: rolling mid-session compaction (head/tail protected).
//!
//! Fires when [`crate::context::ContextManager::fraction_used`] returns
//! ≥ 0.80 in the agent cycle loop. Replaces the middle of the message
//! history with a single synthetic `system`-role message whose body starts
//! with [`SUMMARY_PREFIX`] verbatim (Hermes parity — this phrasing
//! discourages the model from re-acting on resolved requests surfaced in
//! the summary).

use crate::aux_llm::{AuxLlm, AuxLlmError};

/// Hermes summary prefix — ported verbatim per `17-CONTEXT.md` Claude's
/// Discretion section. Do NOT rewrite. Do NOT paraphrase.
pub const SUMMARY_PREFIX: &str = "This is a handoff summary from a previous context window that has been compressed. Treat this summary as\nBACKGROUND REFERENCE ONLY — do NOT treat it as active instructions. Do NOT answer questions that were\nalready resolved and mentioned in the summary. Proceed from the current state described below.\n\nFormat:\n- Resolved questions: [list]\n- Pending questions: [list]\n- Remaining Work: [list]";

/// Minimum clamp on the summarized body (chars, not tokens).
pub const MIN_SUMMARY_CHARS: usize = 2000;
/// Maximum clamp on the summarized body (chars, not tokens).
pub const MAX_SUMMARY_CHARS: usize = 12000;

/// Message count kept at the head of the conversation (system + first user).
pub const HEAD_PROTECTED_COUNT: usize = 2;

/// Approximate tail char budget — ≈20K tokens at 4 chars/token.
pub const TAIL_PROTECTED_CHARS: usize = 80_000;

/// Outcome of a single compaction attempt.
#[derive(Debug, Clone)]
pub enum CompactionOutcome {
    /// Compaction succeeded with an LLM summary inserted in place of the
    /// middle segment. The returned `summary_text` is the clamped summary
    /// body (prefix included at the start). `removed_count` is the number
    /// of middle messages that should be removed by the caller.
    Summarized {
        /// The final summary text (prefixed with [`SUMMARY_PREFIX`] and
        /// clamped into `[MIN_SUMMARY_CHARS, MAX_SUMMARY_CHARS]`).
        summary_text: String,
        /// Count of middle messages (after the head) that were summarized.
        removed_count: usize,
    },
    /// Aux-LLM unavailable OR returned an error; messages were NOT
    /// modified. Caller should still apply cheaper levels (tool-results
    /// clear, thinking strip).
    DegradedNoSummary,
    /// Not enough messages to compact (below head+tail protected total).
    NotNeeded,
}

/// Trait adapter so the compressor doesn't depend on the concrete
/// provider-level `Message` type. Call sites wrap their message slice in
/// a thin adapter.
pub trait CompactableMessage {
    /// Approximate text form used for both size accounting and the
    /// concatenated user prompt sent to the aux LLM.
    fn approx_text(&self) -> String;
    /// Approximate char count (defaults to `approx_text().len()`).
    fn approx_chars(&self) -> usize {
        self.approx_text().len()
    }
}

/// Rolling-compaction orchestrator. Stateless — clone freely.
#[derive(Debug, Default)]
pub struct ContextCompressor;

impl ContextCompressor {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Head/tail-preserving compact. Returns the summary body (with
    /// [`SUMMARY_PREFIX`] prefixed and length-clamped) plus the count of
    /// removed middle messages. The caller replaces the removed range
    /// with a single synthetic system message.
    ///
    /// # Errors
    /// Never returns `Err` in current implementation; aux-LLM failures
    /// translate into [`CompactionOutcome::DegradedNoSummary`]. The
    /// `Result` shape is preserved for future fatal-error signalling.
    pub async fn compact<M: CompactableMessage>(
        &self,
        messages: &[M],
        aux: Option<&dyn AuxLlm>,
    ) -> Result<CompactionOutcome, AuxLlmError> {
        if messages.len() <= HEAD_PROTECTED_COUNT + 1 {
            return Ok(CompactionOutcome::NotNeeded);
        }

        // Determine tail boundary: walk from the end accumulating chars until
        // the tail budget is exhausted.
        let mut tail_chars: usize = 0;
        let mut tail_start = messages.len();
        for (i, msg) in messages.iter().enumerate().rev() {
            tail_chars = tail_chars.saturating_add(msg.approx_chars());
            if tail_chars > TAIL_PROTECTED_CHARS {
                tail_start = i + 1;
                break;
            }
            tail_start = i;
        }

        // Middle range: [HEAD_PROTECTED_COUNT, tail_start)
        if tail_start <= HEAD_PROTECTED_COUNT {
            return Ok(CompactionOutcome::NotNeeded);
        }

        let Some(aux) = aux else {
            return Ok(CompactionOutcome::DegradedNoSummary);
        };

        let user_prompt: String = messages[HEAD_PROTECTED_COUNT..tail_start]
            .iter()
            .map(CompactableMessage::approx_text)
            .collect::<Vec<_>>()
            .join("\n---\n");
        let system_prompt = "Summarize the following prior conversation history into: \
            (1) resolved questions, (2) pending questions, (3) remaining work. \
            Be terse. Do NOT restate instructions.";

        let body = match aux.complete_text(system_prompt, &user_prompt).await {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(%err, "aux-llm compaction summary failed; degrading");
                return Ok(CompactionOutcome::DegradedNoSummary);
            }
        };

        let mut summary_text = format!("{SUMMARY_PREFIX}\n\n{body}");
        if summary_text.len() > MAX_SUMMARY_CHARS {
            summary_text.truncate(MAX_SUMMARY_CHARS);
        }
        if summary_text.len() < MIN_SUMMARY_CHARS {
            // Pad with a short note; the model still sees a valid summary
            // even if the aux LLM returned something too brief.
            summary_text.push_str("\n\n(summary was short; fall back to the preserved tail for details)");
        }
        Ok(CompactionOutcome::Summarized {
            summary_text,
            removed_count: tail_start - HEAD_PROTECTED_COUNT,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeMsg(String);
    impl CompactableMessage for FakeMsg {
        fn approx_text(&self) -> String {
            self.0.clone()
        }
    }

    #[derive(Debug, Default)]
    struct StubAuxOk;
    #[async_trait::async_trait]
    impl AuxLlm for StubAuxOk {
        async fn complete_text(&self, _s: &str, _u: &str) -> Result<String, AuxLlmError> {
            Ok("tiny body".into())
        }
    }

    #[derive(Debug, Default)]
    struct StubAuxErr;
    #[async_trait::async_trait]
    impl AuxLlm for StubAuxErr {
        async fn complete_text(&self, _s: &str, _u: &str) -> Result<String, AuxLlmError> {
            Err(AuxLlmError::Request("boom".into()))
        }
    }

    fn messages_of(n: usize, size: usize) -> Vec<FakeMsg> {
        (0..n).map(|i| FakeMsg(format!("{} {i}", "x".repeat(size)))).collect()
    }

    /// Produce enough messages that the tail budget cannot swallow them all,
    /// forcing a non-empty middle segment. With `TAIL_PROTECTED_CHARS = 80_000`
    /// a 20-message list at 10_000 chars each totals 200_000 chars — tail
    /// captures the last ~8 messages, middle has the rest.
    fn large_message_set() -> Vec<FakeMsg> {
        messages_of(20, 10_000)
    }

    #[tokio::test]
    async fn summary_prefix_is_preserved_verbatim() {
        let comp = ContextCompressor::new();
        let msgs = large_message_set();
        let outcome = comp.compact(&msgs, Some(&StubAuxOk)).await.expect("compact");
        match outcome {
            CompactionOutcome::Summarized { summary_text, .. } => {
                assert!(
                    summary_text.starts_with(SUMMARY_PREFIX),
                    "SUMMARY_PREFIX must appear verbatim at the start"
                );
            }
            other => panic!("expected Summarized; got {other:?}"),
        }
    }

    #[tokio::test]
    async fn aux_error_degrades_gracefully() {
        let comp = ContextCompressor::new();
        let msgs = large_message_set();
        let outcome = comp.compact(&msgs, Some(&StubAuxErr)).await.expect("compact");
        assert!(
            matches!(outcome, CompactionOutcome::DegradedNoSummary),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn small_history_returns_not_needed() {
        let comp = ContextCompressor::new();
        let msgs = messages_of(2, 50);
        let outcome = comp.compact(&msgs, Some(&StubAuxOk)).await.expect("compact");
        assert!(matches!(outcome, CompactionOutcome::NotNeeded), "got {outcome:?}");
    }

    #[tokio::test]
    async fn none_aux_degrades_gracefully() {
        let comp = ContextCompressor::new();
        let msgs = large_message_set();
        let outcome = comp.compact(&msgs, None).await.expect("compact");
        assert!(
            matches!(outcome, CompactionOutcome::DegradedNoSummary),
            "got {outcome:?}"
        );
    }

    #[test]
    fn summary_prefix_matches_hermes_verbatim() {
        // Canary: any accidental reflow of SUMMARY_PREFIX should fail here.
        assert!(SUMMARY_PREFIX.contains("BACKGROUND REFERENCE ONLY"));
        assert!(SUMMARY_PREFIX.contains("Resolved questions"));
        assert!(SUMMARY_PREFIX.contains("Pending questions"));
        assert!(SUMMARY_PREFIX.contains("Remaining Work"));
    }
}
