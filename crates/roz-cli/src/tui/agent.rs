//! Bridge between `roz-agent` streaming types and CLI `AgentEvent`s.

use roz_agent::model::types::StreamChunk;

use crate::tui::provider::AgentEvent;

/// Convert a `roz-agent` `StreamChunk` to a CLI `AgentEvent`.
pub fn stream_chunk_to_event(chunk: &StreamChunk) -> Option<AgentEvent> {
    match chunk {
        StreamChunk::TextDelta(text) => Some(AgentEvent::TextDelta(text.clone())),
        StreamChunk::ThinkingDelta(text) => Some(AgentEvent::ThinkingDelta(text.clone())),
        StreamChunk::ToolUseStart { id, name } => Some(AgentEvent::ToolRequest {
            id: id.clone(),
            name: name.clone(),
            params: String::new(),
        }),
        StreamChunk::Done(resp) => Some(AgentEvent::TurnComplete {
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            stop_reason: format!("{:?}", resp.stop_reason),
        }),
        StreamChunk::Usage(_) | StreamChunk::ToolUseInputDelta(_) => None,
    }
}
