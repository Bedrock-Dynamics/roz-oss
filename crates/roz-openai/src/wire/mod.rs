//! Wire-protocol types for OpenAI-compatible endpoints.
//!
//! Three submodules cover the two wire families + the unified event enum:
//!
//! - [`chat`] — OpenAI Chat Completions v1 request + streaming-chunk types. Used by OSS servers
//!   (vLLM, Ollama, LMStudio, llama.cpp, LiteLLM) and the OpenAI platform Chat API.
//! - [`responses`] — OpenAI Responses v1 types ported from codex-rs `codex-api/src/common.rs`
//!   (pinned SHA `da86cedbd439d38fbd7e613e4e88f8f6f138debb`). Used by the OpenAI platform
//!   Responses API and the ChatGPT backend (`chatgpt.com/backend-api/codex/responses`).
//! - [`events`] — Unified [`events::ResponseEvent`] enum bridging both wires; consumed by the
//!   provider adapter in `roz-agent` (Plan 19-10).
//!
//! Types in this module are deliberately boundary-agnostic: no HTTP client, no SSE parsing, no
//! credential handling. Plan 19-07 ships the client; Plan 19-08 ships the ChatGPT-backend
//! transforms that operate on these types.

pub mod chat;
pub mod events;
pub mod responses;

pub use events::{ResponseEvent, TokenUsage};
pub use responses::ROZ_OUTPUT_SCHEMA_NAME;
