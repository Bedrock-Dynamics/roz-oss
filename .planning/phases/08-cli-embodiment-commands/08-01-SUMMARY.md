---
phase: 08-cli-embodiment-commands
plan: 01
subsystem: roz-cli
tags: [cli, grpc, embodiment, host-commands]
dependency_graph:
  requires: [07-02]
  provides: [CLI-01, CLI-02, CLI-03]
  affects: [roz-cli]
tech_stack:
  added: []
  patterns: [tonic-interceptor, bearer-token-grpc, process-exit-convention]
key_files:
  created: []
  modified:
    - crates/roz-cli/build.rs
    - crates/roz-cli/src/commands/host.rs
decisions:
  - "Use impl tonic::client::GrpcService<BoxBody> return type for make_embodiment_client to avoid naming the intercepted service type"
  - "Calibration line omitted from embodiment summary per plan spec (structural data only)"
  - "std::process::exit(1) on validate FAIL is intentional CLI convention suppressed with #[allow(clippy::exit)]"
metrics:
  duration: "~10 min"
  completed: "2026-04-09"
  tasks: 2
  files_modified: 2
---

# Phase 08 Plan 01: CLI Embodiment Commands Summary

Three new `roz host` subcommands wired to the `EmbodimentService` gRPC via TLS + Bearer token interceptor.

## Tasks Completed

| Task | Name | Commit | Files |
|------|------|--------|-------|
| 1 | Update build.rs to compile embodiment.proto | 73219d7 | crates/roz-cli/build.rs |
| 2 | Add embodiment, bindings, validate commands to host.rs | 5668e2c | crates/roz-cli/src/commands/host.rs |

## What Was Built

- `crates/roz-cli/build.rs`: Added `embodiment.proto` alongside `agent.proto` in `compile_protos` call — generates `EmbodimentServiceClient` and associated request/response types for the CLI.
- `crates/roz-cli/src/commands/host.rs`: Three new subcommands:
  - `roz host embodiment <id>`: Calls `GetModel` RPC, prints `Host/Family/Joints/Links/Frames/Digest` in aligned key-value format. Frame depth computed by walking `parent_id` links.
  - `roz host bindings <id>`: Calls `ListBindings` RPC, outputs channel bindings as pretty-printed JSON via `render_json`.
  - `roz host validate <id>`: Calls `ValidateBindings` RPC, prints `PASS` or `FAIL`. On FAIL, lists unbound channels with binding type and reason, then calls `std::process::exit(1)`.
  - Shared `make_embodiment_client` helper: Builds TLS channel from `config.api_url`, attaches Bearer token from `config.access_token` via tonic interceptor.

## Verification

- `cargo build -p roz-cli`: exits 0
- `cargo clippy -p roz-cli -- -D warnings`: exits 0
- `cargo fmt --check -p roz-cli`: exits 0
- `cargo test -p roz-cli`: 8 passed, 0 failed

## Deviations from Plan

None — plan executed exactly as written. Used `impl tonic::client::GrpcService<tonic::body::BoxBody>` return type (the plan's secondary option) to avoid naming the concrete intercepted service type.

## Known Stubs

None. All three command handlers are fully wired to live RPC calls.

## Threat Flags

No new threat surface beyond what the plan's threat model covers (T-08-01 through T-08-04). Bearer token loaded from keyring/env, transmitted over TLS. No new network endpoints introduced.

## Self-Check: PASSED

- crates/roz-cli/build.rs: contains `embodiment.proto` — FOUND
- crates/roz-cli/src/commands/host.rs: contains `Embodiment`, `Bindings`, `Validate` variants — FOUND
- Commit 73219d7 exists — FOUND
- Commit 5668e2c exists — FOUND
