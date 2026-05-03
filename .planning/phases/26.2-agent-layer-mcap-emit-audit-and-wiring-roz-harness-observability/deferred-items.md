## Deferred: pre-existing workspace fmt issue

**Discovered during:** 26.2-02 verification
**File:** crates/roz-cli/src/commands/session.rs (line 7 area)
**Issue:** `cargo fmt --check` at workspace scope flags a line in roz-cli not touched by 26.2-02.
**Scope boundary:** Out of scope — no changes to roz-cli in this plan.
**Scoped verification:** `cargo fmt -p roz-core -p roz-agent --check` passes clean.

## Deferred: pre-existing `cargo build -p roz-copper --all-features` failure

**Discovered during:** 26.2-04 final verification
**File:** crates/roz-copper/src/lib.rs macro expansion
**Issue:** `cargo build --workspace --all-features` (and `-p roz-copper --all-features`)
fails with E0282/E0432 inside the `#[copper_runtime(...)]` attribute-macro expansion
(`cu29-derive` 0.14). Confirmed pre-existing by `git stash` + build on main
unchanged. Scope boundary: Plan 04 does not touch `roz-copper`.
**Scoped verification:** `cargo build --workspace` (default features) passes.
`cargo clippy --workspace -- -D warnings` (default features) passes.

## Deferred: edge-side AgentEventHook parity drift (cloud vs edge MCAP)

**Discovered during:** 26.2 code review (MR-01) + 26.2 verification (O1)
**Files:**
- crates/roz-worker/src/main.rs:1210 (worker-side AgentLoop::new)
- crates/roz-worker/src/session_relay.rs:441 (relayed edge sessions)
- crates/roz-local/src/runtime.rs:406,1230 (local CLI runtime paths)
**Issue:** Only `crates/roz-server/src/grpc/agent.rs:1639` installs a real `SessionRuntimeEventHook` via `with_agent_event_hook`. All other `AgentLoop::new` sites default to `NoopAgentEventHook`, so agent-loop-originated SessionEvents from edge sessions and local CLI runtime are dropped — creating MCAP parity drift between cloud and edge.
**Scope boundary:** Plan 04's D-14 Gap closure explicitly targeted the cloud session path. Edge/local wiring is a distinct enough surface (separate MCAP writers, separate session lifetimes) that folding it into 26.2 would have re-scoped the phase.
**Recommended follow-up:** Phase 26.2.1 or bundled into 26.3's trace-context propagation work, since both touch every `AgentLoop::new` call site.
