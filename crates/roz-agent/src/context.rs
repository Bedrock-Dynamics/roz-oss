use crate::error::AgentError;
use crate::model::types::{CompletionRequest, ContentPart, Message, MessageRole, Model};

/// What happened during a compaction pass (returned for observability).
#[derive(Debug, Clone)]
pub struct CompactionEvent {
    pub level: CompactionLevel,
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: u32,
    pub tokens_after: u32,
    pub summary: Option<String>,
}

/// The escalation level that was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionLevel {
    /// Level 1: replace old tool-result content with placeholders.
    ToolResults,
    /// Level 2: strip thinking blocks from older assistant messages.
    Thinking,
    /// Level 3: summarize old messages via an LLM call.
    Summary,
}

pub struct ContextManager {
    max_tokens: u32,
    tool_clear_threshold: f64,
    thinking_clear_threshold: f64,
    summary_threshold: f64,
    keep_tool_results: usize,
    keep_thinking_turns: usize,
}

impl ContextManager {
    pub const fn new(max_tokens: u32) -> Self {
        Self {
            max_tokens,
            tool_clear_threshold: 0.50,
            thinking_clear_threshold: 0.65,
            summary_threshold: 0.85,
            keep_tool_results: 5,
            keep_thinking_turns: 2,
        }
    }

    /// Estimate token count using simple char-based heuristic (4 chars ~ 1 token).
    #[allow(clippy::cast_possible_truncation)]
    pub const fn estimate_tokens(text: &str) -> u32 {
        (text.len() as u32) / 4
    }

    /// Total estimated tokens across all messages.
    pub fn message_tokens(messages: &[Message]) -> u32 {
        messages.iter().map(Message::estimated_tokens).sum()
    }

    /// Fraction of the token budget currently used by the given messages.
    ///
    /// Exposed for MEM-06 rolling-compaction trigger in
    /// `agent_loop::core::run_streaming_core`.
    #[must_use]
    pub fn fraction_used(&self, messages: &[Message]) -> f64 {
        f64::from(Self::message_tokens(messages)) / f64::from(self.max_tokens)
    }

    // ------------------------------------------------------------------
    // Level 1: Clear old tool results
    // ------------------------------------------------------------------

    /// Replace the content of old `ToolResult` parts with a placeholder.
    ///
    /// Walks all messages, counts `ToolResult` parts from the end, and clears
    /// (replaces content) all except the last `keep_tool_results`.
    /// The structural pairing with `ToolUse` is preserved -- only the content
    /// string changes.
    fn clear_tool_results(&self, messages: &mut [Message]) {
        // First, collect indices of all ToolResult parts (msg_idx, part_idx).
        let mut result_positions: Vec<(usize, usize)> = Vec::new();
        for (mi, msg) in messages.iter().enumerate() {
            for (pi, part) in msg.parts.iter().enumerate() {
                if matches!(part, ContentPart::ToolResult { .. }) {
                    result_positions.push((mi, pi));
                }
            }
        }

        let total = result_positions.len();
        if total <= self.keep_tool_results {
            return;
        }

        // Clear all except the last `keep_tool_results`.
        let clear_count = total - self.keep_tool_results;
        for &(mi, pi) in &result_positions[..clear_count] {
            if let ContentPart::ToolResult { content, .. } = &mut messages[mi].parts[pi] {
                let token_est = Self::estimate_tokens(content);
                *content = format!("[cleared: ~{token_est} tokens]");
            }
        }
    }

    // ------------------------------------------------------------------
    // Level 2: Clear old thinking blocks
    // ------------------------------------------------------------------

    /// Strip `Thinking` parts from older assistant messages, keeping the last
    /// `keep_thinking_turns` assistant messages that contain thinking intact.
    fn clear_thinking(&self, messages: &mut [Message]) {
        // Find indices of assistant messages that have Thinking parts.
        let thinking_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, msg)| {
                msg.role == MessageRole::Assistant
                    && msg.parts.iter().any(|p| matches!(p, ContentPart::Thinking { .. }))
            })
            .map(|(i, _)| i)
            .collect();

        let total = thinking_indices.len();
        if total <= self.keep_thinking_turns {
            return;
        }

        let clear_count = total - self.keep_thinking_turns;
        for &idx in &thinking_indices[..clear_count] {
            messages[idx]
                .parts
                .retain(|p| !matches!(p, ContentPart::Thinking { .. }));
        }
    }

    // ------------------------------------------------------------------
    // Level 3: LLM summary
    // ------------------------------------------------------------------

    /// Summarize old messages using an LLM, preserving the system prompt
    /// and recent turns.
    ///
    /// Returns `(new_messages, summary_text)` on success.
    async fn summarize(&self, messages: &[Message], model: &dyn Model) -> Result<(Vec<Message>, String), AgentError> {
        if messages.is_empty() || messages[0].role != MessageRole::System {
            return Err(AgentError::Safety(
                "summarize requires a non-empty message list starting with a system message".into(),
            ));
        }
        let system = messages[0].clone();
        let rest = &messages[1..];

        let (old, recent) = Self::split_preserving_pairs(rest, self.max_tokens / 4);

        let prompt = Self::format_messages_for_summary(&old);
        let summary_req = CompletionRequest {
            messages: vec![
                Message::system(
                    "You are a conversation summarizer. Summarize the following conversation \
                     into a concise paragraph that captures all important context, decisions, \
                     and outcomes. Preserve tool names, parameter values, and error messages.",
                ),
                Message::user(prompt),
            ],
            tools: vec![],
            max_tokens: 1024,
            tool_choice: None,
            response_schema: None,
        };

        let response = model.complete(&summary_req).await.map_err(AgentError::Model)?;
        let summary_text = response.text().unwrap_or_else(|| "Summary unavailable.".to_string());

        let mut result = vec![system];
        result.push(Message::user(format!(
            "[Summary of earlier conversation]\n{summary_text}"
        )));
        result.extend(recent);

        Ok((result, summary_text))
    }

    /// Split messages into `(old, recent)` such that `recent` fits within
    /// `recent_budget` tokens, keeping `ToolUse`/`ToolResult` pairs atomic.
    ///
    /// Walks backward from the end. When a `User` message with `ToolResult`
    /// parts is encountered, the preceding `Assistant` message (if it has
    /// `ToolUse` parts) travels with it as a pair.
    ///
    /// **Assumption**: `ToolUse` (assistant) immediately precedes `ToolResult` (user)
    /// with no intervening messages. If this invariant is violated, the pair may
    /// be split across old/recent.
    fn split_preserving_pairs(messages: &[Message], recent_budget: u32) -> (Vec<Message>, Vec<Message>) {
        let mut recent: Vec<Message> = Vec::new();
        let mut tokens = 0u32;
        let mut i = messages.len();

        while i > 0 {
            let msg = &messages[i - 1];
            let msg_tokens = msg.estimated_tokens();

            // Check if this is a tool-result message that should be paired.
            let has_tool_results = msg.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. }));

            if has_tool_results && i >= 2 {
                // Check if the preceding message is an assistant with tool-use.
                let prev = &messages[i - 2];
                let prev_has_tool_use = prev.role == MessageRole::Assistant
                    && prev.parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. }));

                if prev_has_tool_use {
                    let pair_tokens = msg_tokens + prev.estimated_tokens();
                    if tokens + pair_tokens > recent_budget {
                        break;
                    }
                    // Push in reverse order: msg (tool_result) then prev (tool_use),
                    // because the vector is reversed at the end.
                    recent.push(msg.clone());
                    recent.push(prev.clone());
                    tokens += pair_tokens;
                    i -= 2;
                    continue;
                }
            }

            if tokens + msg_tokens > recent_budget {
                break;
            }
            recent.push(msg.clone());
            tokens += msg_tokens;
            i -= 1;
        }

        recent.reverse();
        let old = messages[..i].to_vec();
        (old, recent)
    }

    /// Format messages for the summarization prompt.
    fn format_messages_for_summary(messages: &[Message]) -> String {
        messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                };
                let text = msg.text().unwrap_or_default();
                format!("{role}: {text}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ------------------------------------------------------------------
    // Public API: escalating compaction
    // ------------------------------------------------------------------

    /// Run escalating context compaction.
    ///
    /// Checks thresholds in order (0.50, 0.65, 0.85) and applies increasingly
    /// aggressive compaction levels as needed. Returns a list of
    /// `CompactionEvent`s describing what was done at each level.
    pub async fn compact_escalating(
        &self,
        messages: &mut Vec<Message>,
        model: Option<&dyn Model>,
    ) -> Vec<CompactionEvent> {
        let mut events = Vec::new();

        // Level 1: clear old tool results
        if self.fraction_used(messages) > self.tool_clear_threshold {
            let before_count = messages.len();
            let before_tokens = Self::message_tokens(messages);
            self.clear_tool_results(messages);
            let after_tokens = Self::message_tokens(messages);
            events.push(CompactionEvent {
                level: CompactionLevel::ToolResults,
                messages_before: before_count,
                messages_after: messages.len(),
                tokens_before: before_tokens,
                tokens_after: after_tokens,
                summary: None,
            });
        }

        // Level 2: clear old thinking blocks
        if self.fraction_used(messages) > self.thinking_clear_threshold {
            let before_count = messages.len();
            let before_tokens = Self::message_tokens(messages);
            self.clear_thinking(messages);
            let after_tokens = Self::message_tokens(messages);
            events.push(CompactionEvent {
                level: CompactionLevel::Thinking,
                messages_before: before_count,
                messages_after: messages.len(),
                tokens_before: before_tokens,
                tokens_after: after_tokens,
                summary: None,
            });
        }

        // Level 3: LLM summary (only if model is available)
        if self.fraction_used(messages) > self.summary_threshold
            && let Some(model) = model
        {
            let before_count = messages.len();
            let before_tokens = Self::message_tokens(messages);
            match self.summarize(messages, model).await {
                Ok((new_messages, summary_text)) => {
                    *messages = new_messages;
                    let after_tokens = Self::message_tokens(messages);
                    events.push(CompactionEvent {
                        level: CompactionLevel::Summary,
                        messages_before: before_count,
                        messages_after: messages.len(),
                        tokens_before: before_tokens,
                        tokens_after: after_tokens,
                        summary: Some(summary_text),
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "level-3 summary compaction failed, skipping");
                }
            }
        }

        events
    }

    // ------------------------------------------------------------------
    // Legacy API (kept for backward compatibility with existing tests)
    // ------------------------------------------------------------------

    /// Returns true if message tokens exceed 50% of the max budget.
    ///
    /// This is the lowest escalation threshold -- the escalating compaction
    /// handles progressively higher thresholds internally.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn needs_compaction(&self, messages: &[Message]) -> bool {
        self.fraction_used(messages) > self.tool_clear_threshold
    }

    /// Legacy compaction: keeps the most recent messages that fit in 25% of
    /// budget and returns a placeholder summary of older messages.
    pub fn compact(&self, messages: &[Message]) -> (Vec<Message>, Option<String>) {
        if !self.needs_compaction(messages) {
            return (messages.to_vec(), None);
        }

        let recent_budget = self.max_tokens / 4; // 25%
        let mut recent = Vec::new();
        let mut recent_tokens = 0u32;

        for msg in messages.iter().rev() {
            let tokens = msg.estimated_tokens();
            if recent_tokens + tokens > recent_budget {
                break;
            }
            recent.push(msg.clone());
            recent_tokens += tokens;
        }
        recent.reverse();

        let old_count = messages.len() - recent.len();
        let summary = if old_count > 0 {
            Some(format!("[Compacted {old_count} earlier messages]"))
        } else {
            None
        };

        (recent, summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{CompletionResponse, ContentPart, StopReason, TokenUsage};
    use async_trait::async_trait;

    // ---- Mock model for summarization tests ----

    struct MockSummaryModel;

    #[async_trait]
    impl Model for MockSummaryModel {
        fn capabilities(&self) -> Vec<crate::model::types::ModelCapability> {
            vec![]
        }

        async fn complete(
            &self,
            _req: &CompletionRequest,
        ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
            Ok(CompletionResponse {
                parts: vec![ContentPart::Text {
                    text: "Summary of conversation.".to_string(),
                }],
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
                stop_reason: StopReason::EndTurn,
            })
        }
    }

    // ---- Helpers ----

    fn make_message(content: &str) -> Message {
        Message::user(content)
    }

    /// Build a message containing `ToolResult` parts.
    fn make_tool_result_message(results: Vec<(&str, &str, &str)>) -> Message {
        Message::tool_results(
            results
                .into_iter()
                .map(|(id, name, content)| (id.to_string(), name.to_string(), content.to_string(), false))
                .collect(),
        )
    }

    /// Build an assistant message with `ToolUse` parts.
    fn make_tool_use_message(calls: Vec<(&str, &str)>) -> Message {
        Message::assistant_parts(
            calls
                .into_iter()
                .map(|(id, name)| ContentPart::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: serde_json::json!({}),
                })
                .collect(),
        )
    }

    /// Build an assistant message with both Thinking and Text parts.
    fn make_thinking_assistant(thinking: &str, text: &str) -> Message {
        Message::assistant_parts(vec![
            ContentPart::Thinking {
                thinking: thinking.to_string(),
                signature: String::new(),
            },
            ContentPart::Text { text: text.to_string() },
        ])
    }

    // ---- Existing tests (updated for new threshold) ----

    #[test]
    fn estimate_tokens_four_chars_is_one_token() {
        assert_eq!(ContextManager::estimate_tokens("1234"), 1);
    }

    #[test]
    fn estimate_tokens_empty_string_is_zero() {
        assert_eq!(ContextManager::estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_short_string_rounds_down() {
        // 3 chars / 4 = 0 (integer division)
        assert_eq!(ContextManager::estimate_tokens("abc"), 0);
        // 7 chars / 4 = 1
        assert_eq!(ContextManager::estimate_tokens("abcdefg"), 1);
    }

    #[test]
    fn under_budget_needs_compaction_false() {
        let mgr = ContextManager::new(1000);
        // 8 chars = 2 tokens, well under 500 threshold (50%)
        let messages = vec![make_message("12345678")];
        assert!(!mgr.needs_compaction(&messages));
    }

    #[test]
    fn over_threshold_needs_compaction_true() {
        // max_tokens = 100, threshold = 50% = 50 tokens
        let mgr = ContextManager::new(100);
        // Need > 50 tokens = > 200 chars
        let big_content = "x".repeat(204); // 51 tokens
        let messages = vec![make_message(&big_content)];
        assert!(mgr.needs_compaction(&messages));
    }

    #[test]
    fn compact_under_budget_returns_same_messages() {
        let mgr = ContextManager::new(1000);
        let messages = vec![make_message("hello"), make_message("world")];

        let (preserved, summary) = mgr.compact(&messages);
        assert_eq!(preserved.len(), 2);
        assert_eq!(preserved[0].text().as_deref(), Some("hello"));
        assert_eq!(preserved[1].text().as_deref(), Some("world"));
        assert!(summary.is_none());
    }

    #[test]
    fn compact_over_budget_returns_recent_with_summary() {
        // max_tokens = 100, threshold = 50% = 50, recent_budget = 25 tokens = 100 chars
        let mgr = ContextManager::new(100);

        let messages = vec![
            make_message(&"a".repeat(200)), // 50 tokens -- old, should be compacted
            make_message(&"b".repeat(200)), // 50 tokens -- old, should be compacted
            make_message(&"c".repeat(40)),  // 10 tokens -- recent, fits in 25-token budget
            make_message(&"d".repeat(40)),  // 10 tokens -- recent, fits in 25-token budget
        ];

        // Total = 120 tokens > 50 threshold, so compaction triggers
        let (preserved, summary) = mgr.compact(&messages);

        // Recent messages should be the last ones that fit in 25-token budget
        assert!(preserved.len() < messages.len());
        assert!(summary.is_some());

        let summary_text = summary.unwrap();
        assert!(summary_text.contains("Compacted"));
        assert!(summary_text.contains("earlier messages"));
    }

    #[test]
    fn message_tokens_sums_all_messages() {
        let messages = vec![
            make_message("12345678"), // 2 tokens
            make_message("1234"),     // 1 token
            make_message("12345678"), // 2 tokens
        ];
        assert_eq!(ContextManager::message_tokens(&messages), 5);
    }

    // ---- New escalating compaction tests ----

    #[test]
    fn clear_tool_results_replaces_large_results_with_placeholder() {
        let mgr = ContextManager::new(10_000);
        let big_content = "x".repeat(400); // 100 tokens
        let mut messages = vec![
            Message::system("sys"),
            make_tool_use_message(vec![("t1", "read_file")]),
            make_tool_result_message(vec![("t1", "read_file", &big_content)]),
        ];

        mgr.clear_tool_results(&mut messages);

        // The single result should be cleared (only 1 total, keep=5, but we test
        // with a smaller keep). Use a custom manager.
        let mgr2 = ContextManager {
            keep_tool_results: 0,
            ..ContextManager::new(10_000)
        };
        let mut messages2 = vec![
            Message::system("sys"),
            make_tool_use_message(vec![("t1", "read_file")]),
            make_tool_result_message(vec![("t1", "read_file", &big_content)]),
        ];
        mgr2.clear_tool_results(&mut messages2);

        // The tool result content should now be a placeholder.
        let result_msg = &messages2[2];
        if let ContentPart::ToolResult { content, .. } = &result_msg.parts[0] {
            assert!(
                content.starts_with("[cleared: ~"),
                "expected placeholder, got: {content}"
            );
            assert!(content.ends_with("tokens]"), "expected placeholder, got: {content}");
        } else {
            panic!("expected ToolResult part");
        }

        // The ToolUse message should be untouched.
        assert!(matches!(messages2[1].parts[0], ContentPart::ToolUse { .. }));
    }

    #[test]
    fn clear_tool_results_preserves_last_k_results() {
        let mgr = ContextManager {
            keep_tool_results: 3,
            ..ContextManager::new(10_000)
        };

        // 5 tool-result messages, each with 1 result.
        let mut messages: Vec<Message> = (0..5)
            .map(|i| {
                let id = format!("t{i}");
                let content = format!("result-{i}-{}", "x".repeat(40));
                make_tool_result_message(vec![(&id, "tool", &content)])
            })
            .collect();

        mgr.clear_tool_results(&mut messages);

        // First 2 should be cleared, last 3 preserved.
        for (i, msg) in messages.iter().enumerate() {
            if let ContentPart::ToolResult { content, .. } = &msg.parts[0] {
                if i < 2 {
                    assert!(
                        content.starts_with("[cleared:"),
                        "message {i} should be cleared, got: {content}"
                    );
                } else {
                    assert!(
                        content.starts_with("result-"),
                        "message {i} should be preserved, got: {content}"
                    );
                }
            }
        }
    }

    #[test]
    fn clear_tool_results_never_splits_tool_use_result_pair() {
        let mgr = ContextManager {
            keep_tool_results: 0,
            ..ContextManager::new(10_000)
        };

        let mut messages = vec![
            make_tool_use_message(vec![("t1", "read_file")]),
            make_tool_result_message(vec![("t1", "read_file", "big content here with data")]),
        ];

        mgr.clear_tool_results(&mut messages);

        // ToolUse message must be entirely untouched.
        assert!(matches!(messages[0].parts[0], ContentPart::ToolUse { .. }));
        if let ContentPart::ToolUse { id, name, .. } = &messages[0].parts[0] {
            assert_eq!(id, "t1");
            assert_eq!(name, "read_file");
        }

        // ToolResult still exists (structure preserved), only content replaced.
        assert!(matches!(messages[1].parts[0], ContentPart::ToolResult { .. }));
        if let ContentPart::ToolResult { tool_use_id, name, .. } = &messages[1].parts[0] {
            assert_eq!(tool_use_id, "t1");
            assert_eq!(name, "read_file");
        }
    }

    #[test]
    fn clear_thinking_drops_old_thinking_keeps_recent() {
        let mgr = ContextManager {
            keep_thinking_turns: 2,
            ..ContextManager::new(10_000)
        };

        let mut messages: Vec<Message> = (0..5)
            .map(|i| make_thinking_assistant(&format!("thinking-{i}"), &format!("text-{i}")))
            .collect();

        mgr.clear_thinking(&mut messages);

        // First 3 should have thinking stripped, last 2 preserved.
        for (i, msg) in messages.iter().enumerate() {
            let has_thinking = msg.parts.iter().any(|p| matches!(p, ContentPart::Thinking { .. }));
            if i < 3 {
                assert!(!has_thinking, "message {i} should have thinking cleared");
                // But text should still be there.
                assert!(msg.text().is_some(), "message {i} should still have text");
            } else {
                assert!(has_thinking, "message {i} should retain thinking");
                assert!(msg.text().is_some(), "message {i} should still have text");
            }
        }
    }

    #[tokio::test]
    async fn summarize_preserves_system_prompt_and_recent_turns() {
        let mgr = ContextManager::new(1000);
        let model = MockSummaryModel;

        // System + several messages. Recent budget = 250 tokens = 1000 chars.
        let messages = vec![
            Message::system("You are a robot assistant."),
            make_message(&"old-msg-1-".repeat(40)), // ~100 chars = 25 tokens
            make_message(&"old-msg-2-".repeat(40)), // ~100 chars = 25 tokens
            make_message(&"old-msg-3-".repeat(40)), // ~100 chars = 25 tokens
            make_message(&"recent-1-".repeat(10)),  // ~90 chars = ~22 tokens
            make_message(&"recent-2-".repeat(10)),  // ~90 chars = ~22 tokens
        ];

        let (result, summary_text) = mgr.summarize(&messages, &model).await.unwrap();

        // System prompt preserved.
        assert_eq!(result[0].role, MessageRole::System);
        assert_eq!(result[0].text().as_deref(), Some("You are a robot assistant."));

        // Summary message present.
        assert_eq!(summary_text, "Summary of conversation.");
        assert!(result[1].text().unwrap().contains("Summary of conversation."));

        // Recent messages preserved at the end.
        let last = result.last().unwrap();
        assert!(last.text().unwrap().contains("recent-2-"));
    }

    #[tokio::test]
    async fn summarize_keeps_tool_pairs_atomic_when_splitting_recent() {
        let mgr = ContextManager::new(10_000);
        let model = MockSummaryModel;

        // System + old messages + tool_use + tool_result pair at the end.
        let messages = vec![
            Message::system("sys"),
            make_message(&"old-".repeat(100)), // 100 chars = 25 tokens
            make_tool_use_message(vec![("t1", "read_file")]),
            make_tool_result_message(vec![("t1", "read_file", "the file content")]),
            make_message("final user msg"),
        ];

        let (result, _) = mgr.summarize(&messages, &model).await.unwrap();

        // The tool_use and tool_result should both appear in recent, not split.
        let has_tool_use = result
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. })));
        let has_tool_result = result
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })));

        // Both or neither should be in recent.
        assert_eq!(has_tool_use, has_tool_result, "tool pairs must travel together");
    }

    #[tokio::test]
    async fn escalating_compaction_tries_levels_in_order() {
        // max_tokens = 200. At 55% usage (~110 tokens), only level 1 should fire.
        let mgr = ContextManager::new(200);

        // Build messages totaling ~110 tokens (440 chars) with tool results.
        let mut messages = vec![
            Message::system("sys"),
            make_tool_use_message(vec![("t1", "tool")]),
            // Large tool result: ~400 chars = 100 tokens
            make_tool_result_message(vec![("t1", "tool", &"x".repeat(400))]),
            make_message("short"), // ~1 token
        ];

        let events = mgr.compact_escalating(&mut messages, None).await;

        // Should have fired level 1 (tool results). fraction > 0.50 but clearing
        // tool results should bring it under 0.65 so level 2 should not fire.
        assert!(!events.is_empty(), "expected at least one compaction event");
        assert_eq!(events[0].level, CompactionLevel::ToolResults);

        // Verify no level 2 or level 3 fired.
        assert!(
            !events.iter().any(|e| e.level == CompactionLevel::Thinking),
            "level 2 should not fire at 55%"
        );
        assert!(
            !events.iter().any(|e| e.level == CompactionLevel::Summary),
            "level 3 should not fire at 55%"
        );
    }

    #[tokio::test]
    async fn compaction_returns_event_describing_what_happened() {
        let mgr = ContextManager {
            keep_tool_results: 0,
            ..ContextManager::new(200)
        };

        // ~110 tokens total > 50% of 200
        let mut messages = vec![
            Message::system("sys"),
            make_tool_use_message(vec![("t1", "tool")]),
            make_tool_result_message(vec![("t1", "tool", &"x".repeat(400))]),
            make_message("end"),
        ];

        let tokens_before = ContextManager::message_tokens(&messages);
        let events = mgr.compact_escalating(&mut messages, None).await;

        assert!(!events.is_empty());
        let event = &events[0];
        assert_eq!(event.level, CompactionLevel::ToolResults);
        assert_eq!(event.tokens_before, tokens_before);
        assert!(
            event.tokens_after < event.tokens_before,
            "tokens should decrease after clearing tool results"
        );
        assert!(event.summary.is_none(), "level 1 has no summary text");
    }

    #[tokio::test]
    async fn escalating_compaction_skips_summary_without_model() {
        let mgr = ContextManager::new(100);
        // Well past 85% threshold (100 tokens in a 100-token budget)
        let mut messages = vec![Message::system("sys"), make_message(&"x".repeat(400))];

        let events = mgr.compact_escalating(&mut messages, None).await;
        assert!(
            !events.iter().any(|e| e.level == CompactionLevel::Summary),
            "level 3 must not fire without a model"
        );
    }

    // ---- Multi-tool batching regression tests ----

    #[test]
    fn split_preserving_pairs_keeps_multi_tool_batch_atomic() {
        // Asst calls 3 tools → single User message with 3 results.
        // Both messages must stay together during split.
        let messages = vec![
            make_message(&"old-".repeat(200)), // 200 chars = 50 tokens
            make_tool_use_message(vec![("t1", "tool_a"), ("t2", "tool_b"), ("t3", "tool_c")]),
            make_tool_result_message(vec![
                ("t1", "tool_a", "result_a"),
                ("t2", "tool_b", "result_b"),
                ("t3", "tool_c", "result_c"),
            ]),
            make_message("final"),
        ];

        // Budget large enough for the pair + final, but not the old message.
        let (old, recent) = ContextManager::split_preserving_pairs(&messages, 200);

        let recent_has_tool_use = recent
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. })));
        let recent_has_tool_result = recent
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })));

        assert!(
            recent_has_tool_use && recent_has_tool_result,
            "multi-tool ToolUse/ToolResult pair must travel together and both be present in recent"
        );

        // Verify all 3 results are in the single batched message.
        if recent_has_tool_result {
            let result_msg = recent
                .iter()
                .find(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })))
                .unwrap();
            let result_count = result_msg
                .parts
                .iter()
                .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
                .count();
            assert_eq!(result_count, 3, "batched message must contain all 3 tool results");
        }

        assert!(!old.is_empty(), "old messages should exist");
    }

    #[tokio::test]
    async fn summarize_keeps_multi_tool_pair_atomic() {
        let mgr = ContextManager::new(10_000);
        let model = MockSummaryModel;

        // System + old messages + multi-tool pair + final message.
        let messages = vec![
            Message::system("sys"),
            make_message(&"old-".repeat(100)),
            make_tool_use_message(vec![("t1", "move_arm"), ("t2", "read_sensor")]),
            make_tool_result_message(vec![
                ("t1", "move_arm", "arm moved"),
                ("t2", "read_sensor", "distance: 3.5"),
            ]),
            make_message("final user msg"),
        ];

        let (result, _) = mgr.summarize(&messages, &model).await.unwrap();

        let has_tool_use = result
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. })));
        let has_tool_result = result
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })));

        assert!(
            has_tool_use && has_tool_result,
            "multi-tool pair must travel together and both be present after summarization"
        );

        // If tool results survived, verify both are in one message.
        if has_tool_result {
            let result_msgs: Vec<_> = result
                .iter()
                .filter(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })))
                .collect();
            assert_eq!(
                result_msgs.len(),
                1,
                "multi-tool results must remain in a single batched User message"
            );
            let part_count = result_msgs[0]
                .parts
                .iter()
                .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
                .count();
            assert_eq!(part_count, 2, "both tool results must be in the batched message");
        }
    }

    #[test]
    fn clear_tool_results_handles_multi_result_messages() {
        let mgr = ContextManager {
            keep_tool_results: 0,
            ..ContextManager::new(10_000)
        };

        // Single User message containing 2 tool results (batched).
        let mut messages = vec![
            Message::system("sys"),
            make_tool_use_message(vec![("t1", "tool_a"), ("t2", "tool_b")]),
            make_tool_result_message(vec![
                ("t1", "tool_a", &"x".repeat(400)),
                ("t2", "tool_b", &"y".repeat(400)),
            ]),
        ];

        mgr.clear_tool_results(&mut messages);

        // Both results in the batched message should be cleared.
        let result_msg = &messages[2];
        for part in &result_msg.parts {
            if let ContentPart::ToolResult { content, .. } = part {
                assert!(
                    content.starts_with("[cleared:"),
                    "batched tool result should be cleared, got: {content}"
                );
            }
        }

        // The ToolUse message must be untouched.
        let use_count = messages[1]
            .parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolUse { .. }))
            .count();
        assert_eq!(use_count, 2, "both ToolUse parts should remain intact");
    }

    /// Regression: previously each tool result was a separate User
    /// message. If unbatched messages leak into split_preserving_pairs,
    /// the i-2 check finds the wrong message. This test verifies that
    /// the unbatched layout would NOT be correctly paired — confirming
    /// batching is essential.
    #[test]
    fn split_preserving_pairs_fails_with_unbatched_layout() {
        // Simulate the OLD (broken) message layout:
        // Asst(TU_a, TU_b) → User(TR_a) → User(TR_b)
        let messages = vec![
            make_message(&"old-".repeat(200)),
            make_tool_use_message(vec![("t1", "tool_a"), ("t2", "tool_b")]),
            // BROKEN: separate User messages per result
            make_tool_result_message(vec![("t1", "tool_a", "result_a")]),
            make_tool_result_message(vec![("t2", "tool_b", "result_b")]),
            make_message("final"),
        ];

        let (_old, recent) = ContextManager::split_preserving_pairs(&messages, 200);

        // With unbatched layout, User(TR_b) at index 3 checks
        // messages[3-2] = messages[1] which IS the assistant.
        // But User(TR_a) at index 2 checks messages[2-2] = messages[0]
        // which is a plain User message, NOT the assistant.
        // So the pairing for TR_a is broken.
        let tool_result_msgs: Vec<_> = recent
            .iter()
            .filter(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })))
            .collect();
        let tool_use_msgs: Vec<_> = recent
            .iter()
            .filter(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. })))
            .collect();

        // The broken behavior: 2 separate ToolResult messages instead of 1 batched
        // message. This is what batching prevents — split_preserving_pairs relies on
        // a single User message containing all results for a given assistant turn.
        assert_eq!(
            tool_result_msgs.len(),
            2,
            "unbatched layout produces 2 separate ToolResult messages instead of 1 batched message"
        );
        assert_eq!(
            tool_use_msgs.len(),
            1,
            "ToolUse message remains in recent alongside the unbatched results"
        );
    }

    #[tokio::test]
    async fn l2_thinking_clear_preserves_multi_tool_pair_structure() {
        // Verify that L2 (thinking clearing) does not corrupt the
        // structure of multi-tool batched messages that follow a
        // thinking assistant turn.
        let mgr = ContextManager {
            keep_thinking_turns: 1,
            keep_tool_results: 10,
            ..ContextManager::new(10_000)
        };

        let mut messages = vec![
            Message::system("sys"),
            // Turn 1: thinking + text (old, will be cleared)
            make_thinking_assistant("planning move...", "I'll move the arm and read the sensor."),
            // Turn 1 follow-up: multi-tool call from same assistant
            make_tool_use_message(vec![("t1", "move_arm"), ("t2", "read_sensor")]),
            // Batched tool results
            make_tool_result_message(vec![
                ("t1", "move_arm", "arm moved to [1,0,0]"),
                ("t2", "read_sensor", "distance: 3.5m"),
            ]),
            // Turn 2: thinking + text (recent, will be kept)
            make_thinking_assistant("analyzing results...", "Both operations succeeded."),
            make_message("Great, what's next?"),
        ];

        mgr.clear_thinking(&mut messages);

        // Turn 1's thinking should be cleared (index 1)
        let turn1 = &messages[1];
        assert!(
            !turn1.parts.iter().any(|p| matches!(p, ContentPart::Thinking { .. })),
            "old thinking block should be cleared"
        );
        // But its text should survive
        assert!(turn1.text().is_some(), "text part should survive thinking clearing");

        // Turn 2's thinking should be preserved (within keep_thinking_turns)
        let turn2 = &messages[4];
        assert!(
            turn2.parts.iter().any(|p| matches!(p, ContentPart::Thinking { .. })),
            "recent thinking block should be preserved"
        );

        // Multi-tool pair must be completely intact
        let tool_use_msg = &messages[2];
        let tool_use_count = tool_use_msg
            .parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolUse { .. }))
            .count();
        assert_eq!(tool_use_count, 2, "both ToolUse parts must survive L2 clearing");

        let tool_result_msg = &messages[3];
        let tool_result_count = tool_result_msg
            .parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .count();
        assert_eq!(tool_result_count, 2, "both ToolResult parts must survive L2 clearing");

        // Verify tool result content is NOT cleared (L2 only touches thinking)
        for part in &tool_result_msg.parts {
            if let ContentPart::ToolResult { content, .. } = part {
                assert!(
                    !content.starts_with("[cleared:"),
                    "L2 must not clear tool result content: {content}"
                );
            }
        }
    }
}
