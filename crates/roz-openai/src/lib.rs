//! `roz-openai` — OpenAI-compatible provider (Chat + Responses wires, dual-auth).
//!
//! Phase 19 — see `.planning/phases/19-open-weight-model-path/19-RESEARCH.md`.
//!
//! # Upstream-divergence note
//!
//! Roz retains `WireApi::Chat` additively; codex-rs upstream deliberately dropped it
//! (codex-rs `model-provider-info/src/lib.rs:36,66`). Do NOT drop on rebase.
//!
//! # Module layout (populated by Phase 19 plans)
//!
//! - `auth` (this plan, 19-05) — `AuthProvider` trait + `ApiKey`/`OAuth`/`Null` impls.
//! - `provider_info` (this plan, 19-05) — built-in registry.
//! - Note: 19-04 defines `EndpointRegistry` in `roz-core` (not a `roz-openai`
//!   submodule; no trait).
//! - `wire` (19-06) — Chat + Responses wire types.
//! - `sse`, `client`, `error` (19-07) — streaming HTTP client.
//! - `transform` (19-08) — ChatGPT-backend request transforms.
//! - `prompts` (19-09) — Codex system-prompt snapshot.

pub mod auth;
pub mod client;
pub mod error;
pub mod prompts;
pub mod provider_info;
pub mod sse;
pub mod transform;
pub mod wire;
