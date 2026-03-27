//! Provider implementations.
//!
//! BYOK (Anthropic) is handled by `roz-agent`'s `AgentLoop` in `tui/mod.rs`.
//! Cloud (Roz Cloud gRPC) is handled here.
//! Ollama is planned as a future `Model` impl in `roz-agent`.

mod anthropic;
pub mod cloud;
mod ollama;
