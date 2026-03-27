//! Anthropic BYOK provider — now handled by `roz-agent`'s `AnthropicProvider`.
//!
//! Direct Anthropic API calls use `roz_agent::model::create_model()` with
//! `direct_api_key` set. The agent loop handles SSE streaming, `tool_use`
//! accumulation, and multi-turn tool loops internally.
//!
//! This module is kept as a placeholder for any Anthropic-specific CLI
//! configuration in the future.
