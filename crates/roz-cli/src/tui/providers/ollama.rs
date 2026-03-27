//! Ollama provider — future `OllamaModel` impl in `roz-agent`.
//!
//! When implemented, Ollama will be a `Model` trait implementation in
//! `roz-agent/src/model/ollama.rs` that uses the Ollama REST API
//! (`/api/chat` with NDJSON streaming). The same `AgentLoop` drives it.
//!
//! For now, Ollama is not supported. Use `--provider anthropic` with
//! an Anthropic API key, or `--provider cloud` for Roz Cloud.
