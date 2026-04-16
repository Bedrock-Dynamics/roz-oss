# Roz

## What This Is

Roz is the open-source robotics platform for embodied AI agents. It provides gRPC/REST APIs for task dispatch, agent sessions, embodiment management, safety enforcement, media analysis, durable agent memory, per-tenant skills artifacts, open-weight model routing, programmatic tool calling, server-side MCP integration, scheduled task dispatch, and edge worker orchestration — backed by PostgreSQL, NATS JetStream, Restate durable workflows, and optional Zenoh edge transport.

## Core Value

A reliable, secure, and well-tested platform that operators trust for physical robot deployments.

## Current State

**Last shipped:** v2.2 Runtime Event Contracts and Completeness (2026-04-16)

Platform now has:

- Durable multi-level memory: tenant-scoped FTS over session turns, curated long-term memory, per-tenant user-model facts, and rolling compaction under context pressure
- Per-tenant skills artifacts with `agentskills.io`-compatible `SKILL.md`, progressive disclosure, object-store-backed assets, and permission-gated writes
- Generic OpenAI-compatible model endpoints with configurable reasoning formats, structured outputs, encrypted credentials, and CLI/TUI OAuth flows
- Programmatic tool calling through QuickJS/Rhai sandboxes with preserved caller identity, nested approval handling, and benchmarked N→1 inference behavior
- Server-side MCP registry, control plane, degraded-tool pruning, and approval-style OAuth-backed registrations
- Natural-language scheduled invocations backed by canonical cron parsing, durable Restate workflows, and shipped `roz schedule` CLI commands
- Typed `skill_loaded` / `skill_crystallized` gRPC payloads with cloud, worker-relay, and local cloud-TUI correlation coverage
- An explicit skill freshness contract: `skills_context` freezes at session start while `skills_list` and `skill_view` remain the live mid-session surfaces

## Current Focus

Define the next milestone after closing the v2.2 carryover runtime-event work. The immediate skill-event completeness gaps from the v2.1 ship review are now closed.

## Requirements

### Validated

**v1.0 Roz Embodiment Protos**
- ✓ PROTO-01..19: Complete proto mirror of Rust embodiment types
- ✓ GRPC-01..04: EmbodimentService with 4 RPCs
- ✓ CONV-01..06: Bidirectional type conversions with round-trip property tests
- ✓ SERV-01..04: Server implementation, registration, and reflection

**v1.1 Embodiment Streaming, CLI, and Extensions**
- ✓ WIRE-01..02: Worker upload_embodiment with digest-based conditional upload
- ✓ EXT-01..03: GetRetargetingMap, GetManifest, coverage metadata
- ✓ STRM-01..03: StreamFrameTree, WatchCalibration, digest-aware streaming
- ✓ CLI-01..03: `roz host embodiment/bindings/validate` commands

**v2.0 Platform Hardening**
- ✓ REL-01..04: DB pool timeouts, pagination validation, shared extractor, migration failure gating
- ✓ SEC-01..05: Per-tenant REST+gRPC rate limiting, structural gRPC auth, NATS credential warning, Ed25519 WASM verification
- ✓ DEBT-01..03: agent loop module split, streaming unification, session turn persistence
- ✓ ENF-01..02: Device trust at task dispatch, signed-only `from_precompiled`
- ✓ TEST-01..02: Nightly Docker integration CI, nextest isolation profile
- ✓ ZEN-01..06: roz-zenoh pub/sub, session relay, health, coordination, worker wiring
- ✓ ZEN-TEST-01..06: Six Zenoh coverage gaps closed with real-binary and chaos tests
- ✓ MED-01..04: AnalyzeMedia RPC, Gemini backend, SSRF-guarded fetcher, roz-proto crate

**v2.1 Agent Capability Growth**
- ✓ MEM-01..08: Durable memory, session search, user-model facts, memory tools, and rolling compaction
- ✓ SKILL-01..07: Skills persistence, progressive disclosure, permission-gated writes, and `roz skill` CLI
- ✓ OWM-01..08: Generic OpenAI-compatible provider, reasoning-format control, structured outputs, endpoint registry, and fixture-backed verification
- ✓ PTC-01..07: `execute_code` sandboxing, auth preservation, approval pause/resume, output limits, and benchmark coverage
- ✓ MCP-01..06: Server-side MCP registry, control plane, runtime tool exposure, OAuth approvals, and degraded-tool handling
- ✓ SCHED-01..07: Scheduled-task persistence, NL→cron parsing, TaskService schedule RPCs, durable recurrence, and `roz schedule` CLI

**v2.2 Runtime Event Contracts and Completeness**
- ✓ RTEC-01..03: Typed skill-event gRPC payloads, cloud/worker/local correlation coverage, and the explicit frozen-vs-live skill reload contract

### Active

- Planning next milestone. No committed active requirement set exists yet.

### Next-Milestone Candidates

- Public skills registry (`skills.sh` / `/.well-known/skills/index.json`) once the internal skills catalog proves durable
- Python via Pyodide-WASM for `execute_code` if QuickJS/Rhai demand expands
- Periodic background review / self-nudge actor if it proves valuable in real operations
- pgvector-backed semantic retrieval once hosted-Postgres availability is confirmed
- Wider CLI output formatting and reporting polish beyond the current command-specific surfaces

### Out of Scope

- Cloud-specific auth (Clerk, billing) — private repo
- fly.io deployment configuration — private repo
- substrate-ide client updates — separate repo
- Motion planning RPCs (IK, trajectory) — separate service concern
- Mutable model RPCs — model config remains boot-time, not runtime mutable
- Mesh binary data in proto — use URI references instead
- Messaging gateway (Telegram/Discord/Slack/WhatsApp/Signal/Email) — substrate-ide and gRPC remain the UX
- Browser automation, TTS, image generation, training-data pipeline (Atropos) — not robotics-core
- Pluggable exec backends (Daytona/Modal/Singularity hibernation) — `roz-local` is sufficient today
- tonic 0.14 upgrade — still requires workspace-wide tonic/prost migration

## Context

Shipped v2.1 spanning Phases 17-21: 5 phases, 49 plans, 72 tasks. Closed v2.2 as a narrow carryover milestone with Phase 21.1 on 2026-04-16.

Tech stack additions across v2.1: Postgres-backed memory, skills, model-endpoint, MCP, and scheduled-task tables; `crates/roz-openai`; `crates/roz-mcp`; QuickJS and Rhai execution runtimes; `chrono-tz` schedule preview/catch-up logic; encrypted credential handling for model endpoints and MCP registrations.

Verification posture: the v2.1 milestone audit passed `43/43` requirements. Phase 17 has an explicit closeout verification report, Phase 19 has a verifier pass, Phase 20 closed with gRPC session/MCP OAuth integration coverage plus the `execute_code_roundtrip` benchmark, Phase 21 closed with both fast gRPC coverage and ignored full-stack Restate restart durability coverage, and Phase 21.1 added focused server/worker/CLI/runtime regression coverage for the remaining skill-event contract gaps.

Security posture: tenant isolation remains enforced with RLS across the new persisted agent surfaces; structural gRPC auth continues to derive request identity centrally; model-endpoint and MCP credentials are stored encrypted; nested execute-code physical calls and MCP OAuth registrations both preserve the approval boundary through the session stream.

## Constraints

- **Multi-tenant isolation first:** new persisted agent features must preserve tenant boundaries and RLS semantics.
- **Approval-preserving safety stack:** physical actions and auth-bearing external integrations must continue to flow through the existing approval model.
- **Server-authoritative shared surfaces:** task dispatch, schedule parsing, and server-owned tool exposure should not fork into drifting client-side logic.
- **Vendor-neutral OSS model support:** open-weight endpoint support must stay generic in config, types, and env vars.
- **Brownfield workspace:** new capabilities must fit the existing tonic/prost, Restate, NATS, and multi-crate architecture.

## Key Decisions

| Decision | Rationale | Outcome |
|----------|-----------|---------|
| Structural Tower-layer gRPC auth | Remove fragile per-RPC auth calls and keep identity derivation centralized | ✓ Good — stayed compatible through v2.0 and v2.1 capability growth |
| Shared `Arc<KeyedRateLimiter>` across REST + gRPC | Prevent cross-protocol rate-limit bypass | ✓ Good — platform hardening baseline held through v2.1 |
| Postgres FTS primary; pgvector optional | Hosted-PG portability and deterministic rollout beat semantic-search ambition | ✓ Good — memory shipped cleanly without waiting on pgvector |
| Roz-native dialectic user model; Honcho later | Avoid external dependency pressure in the core memory path | ✓ Good — user-model facts shipped inside the same Postgres trust boundary |
| Skills metadata in Postgres + assets in object storage | Keep queryable skill metadata local while avoiding binary-blob bloat in Postgres | ✓ Good — import/export and progressive disclosure shipped cleanly |
| Concrete `EndpointRegistry`, not a trait | Defer abstraction until a second real caller exists | ✓ Good — kept the model-endpoint path simpler and easier to verify |
| Dual-wire OpenAI-compatible client (Chat + Responses) | One provider needed to span OSS backends and ChatGPT-backed OAuth flows | ✓ Good — Phase 19 shipped both wires without vendor forks |
| wasmtime + QuickJS/Rhai for `execute_code` | Preserve sandboxing, cross-platform support, and approval control without subprocess Python | ✓ Good — benchmarked and verified in Phase 20 |
| `PermissionDecision` as the single approval ingress | Reusing one approval surface reduces operator ambiguity and safety drift | ✓ Good — nested execute-code approvals and MCP OAuth both ride the same stream |
| Server-owned MCP surface separate from client-owned tools | Degraded-tool pruning should not mutate IDE inventories | ✓ Good — session-start import and degradation pruning remain precise |
| Reload auth-bearing MCP handles from DB after writes | Config-only upserts cannot reconstruct decrypted bearer/OAuth material | ✓ Good — static and OAuth registrations become immediately usable |
| Self-scheduling Restate workflow for NL cron | Restate has no native cron primitive; durable chaining is the correct fit | ✓ Good — restart durability proved out in Phase 21 |
| Server-authoritative NL→cron parsing | Persist one canonical schedule interpretation and reject client drift | ✓ Good — CLI stays thin and the server owns actual recurrence semantics |

## Evolution

This document evolves at phase transitions and milestone boundaries.

---
*Last updated: 2026-04-16 after completing carryover Phase 21.1*
