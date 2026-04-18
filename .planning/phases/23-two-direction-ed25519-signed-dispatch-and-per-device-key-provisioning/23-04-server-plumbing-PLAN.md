---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 04
type: execute
wave: 2
autonomous: true
objective: >
  Wire shared server scaffolding so downstream route + verify-gate plans (23-05
  and 23-06) can run in parallel without file conflicts. Adds AppState fields
  (moka LRU cache for VerifyingKey + server signing-state handle), config enum
  `SignedDispatchEnforcement`, the `safety.signature_failure.*` subject builders
  in roz-nats::subjects, and a stub routes/device.rs with the module registered
  in routes/mod.rs so 23-05 can fill handlers.
depends_on:
  - "23-01"
  - "23-02"
  - "23-03"
files_modified:
  - Cargo.toml
  - crates/roz-server/Cargo.toml
  - crates/roz-server/src/state.rs
  - crates/roz-server/src/main.rs
  - crates/roz-server/src/config.rs
  - crates/roz-server/src/routes/mod.rs
  - crates/roz-server/src/routes/device.rs
  - crates/roz-nats/src/subjects.rs
requirements:
  - FS-04
task_count: 3

must_haves:
  truths:
    - "AppState carries a `verifying_key_cache: moka::future::Cache<(Uuid, Uuid, u32), VerifyingKey>` with 60s TTL + 10 000 entry cap."
    - "AppState carries a server signing-state fetcher (reads `roz_server_signing_state` + decrypts seed via StaticKeyProvider)."
    - "Server reads SIGNED_DISPATCH_ENFORCEMENT env var into a typed enum (Off | Audit | Strict) with default Strict in prod and Audit in dev per Planner's Discretion."
    - "Subjects::safety_signature_failure_worker(host_id) and ::safety_signature_failure_server(tenant_id) compile and validate tokens."
    - "routes/device.rs stub exists with the module registered in routes/mod.rs so 23-05 only adds handler bodies."
  artifacts:
    - path: crates/roz-server/src/state.rs
      provides: "AppState with verifying_key_cache + server_signing_state access"
      contains: "verifying_key_cache"
    - path: crates/roz-server/src/config.rs
      provides: "SignedDispatchEnforcement enum + env loading"
      contains: "SignedDispatchEnforcement"
    - path: crates/roz-nats/src/subjects.rs
      provides: "safety.signature_failure.{host_id} and safety.signature_failure.server.{tenant_id} builders"
      contains: "safety_signature_failure"
    - path: crates/roz-server/src/routes/device.rs
      provides: "Stub module with provision_key_stub + rotate_key_stub placeholder handlers"
      exports: ["device_routes"]
  key_links:
    - from: crates/roz-server/src/state.rs
      to: moka::future::Cache
      via: "Field typed as `Cache<(Uuid, Uuid, u32), VerifyingKey>`"
      pattern: "moka::future::Cache"
    - from: crates/roz-server/src/main.rs
      to: crates/roz-server/src/state.rs
      via: "AppState constructor initializes cache + loads server signing state"
      pattern: "AppState::new"
---

<objective>
De-risk Wave 3 parallelism by pre-wiring the server-side plumbing that 23-05 (device routes) and 23-06 (verify gate + dispatch signing) would otherwise both need to edit. After this plan: AppState has the cache + the signing-state fetcher, config has the enforcement enum, subjects has the two new failure subjects, and `routes/device.rs` exists as a stub with empty handlers. Wave 3 plans then only touch their own files.

Purpose: Eliminate the hidden file conflict on `state.rs`, `main.rs`, `config.rs`, and `routes/mod.rs` that would otherwise force 23-05 and 23-06 into sequential waves. This is the advisor's recommended shape.
Output: Compile-clean server build with new scaffolding. No new functional behavior yet — handlers return 501 Not Implemented.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@crates/roz-server/src/state.rs
@crates/roz-server/src/main.rs
@crates/roz-server/src/config.rs
@crates/roz-server/src/routes/mod.rs
@crates/roz-nats/src/subjects.rs
@crates/roz-core/src/key_provider.rs
@crates/roz-db/src/server_signing_state.rs
@crates/roz-db/src/device_keys.rs

<interfaces>
<!-- Existing AppState — new fields must append, not restructure. -->
<!-- From crates/roz-server/src/state.rs (read before modifying). -->

<!-- Existing config pattern — figment-based; env vars under `roz_server` prefix. -->
<!-- From crates/roz-server/src/config.rs. -->

<!-- Subjects builder pattern — validate_token() on every user-supplied segment. -->
<!-- From crates/roz-nats/src/subjects.rs:6-16. -->

<!-- KeyProvider API — async encrypt/decrypt; tenant_id arg ignored by StaticKeyProvider. -->
<!-- From crates/roz-core/src/key_provider.rs. -->
use roz_core::{KeyProvider, StaticKeyProvider};
</interfaces>
</context>

<planners_discretion>
- **Enforcement default (Q6 from RESEARCH.md):** `SIGNED_DISPATCH_ENFORCEMENT` unset: default to **`Audit` when `ROZ_ENVIRONMENT=development`**, **`Strict` otherwise**. Rationale: avoids breaking the dev loop while still catching missing-signatures in warning logs. Production fails closed.
- **LRU cache crate (RESEARCH.md F4):** `moka = "0.12"` with `feature = "future"`. Already indirect via restate-sdk; promote to direct workspace dep.
- **Server signing key bootstrap:** On first request from a (tenant, host) pair, server generates a fresh Ed25519 keypair, encrypts the 32-byte seed with `StaticKeyProvider`, inserts into `roz_server_signing_state`. Implementation of the lazy-create path happens in 23-05 — this plan just stubs the AppState handle (`Arc<ServerSigningKeyStore>` with the DB pool + key provider + cache fields).
</planners_discretion>

<tasks>

<task type="auto">
  <name>Task 1: Add moka workspace dep, SignedDispatchEnforcement enum, and env loading</name>
  <files>Cargo.toml, crates/roz-server/Cargo.toml, crates/roz-server/src/config.rs</files>
  <action>
1. Root `Cargo.toml` `[workspace.dependencies]`:
   ```toml
   moka = { version = "0.12", features = ["future"] }
   ```
2. `crates/roz-server/Cargo.toml` `[dependencies]`:
   ```toml
   moka = { workspace = true }
   ```
3. In `crates/roz-server/src/config.rs`, add:
   ```rust
   /// Rollout gate for Phase 23 two-direction signed dispatch (D-12).
   #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum SignedDispatchEnforcement {
       /// Warn on missing/invalid signatures but accept them. Used only during
       /// pre-v3.0 rollout; never the production default.
       Off,
       /// Accept all messages but log a warning on signature problems. Default
       /// for `ROZ_ENVIRONMENT=development`.
       Audit,
       /// Reject messages with missing or invalid signatures. Production
       /// default for fresh v3.0 deployments.
       Strict,
   }

   impl SignedDispatchEnforcement {
       /// Planner's Discretion (Q6): default by environment.
       pub fn default_for_env(environment: &str) -> Self {
           if environment.eq_ignore_ascii_case("development") {
               Self::Audit
           } else {
               Self::Strict
           }
       }

       pub fn from_env(environment: &str) -> Self {
           match std::env::var("SIGNED_DISPATCH_ENFORCEMENT").ok().as_deref() {
               Some("off") => Self::Off,
               Some("audit") => Self::Audit,
               Some("strict") => Self::Strict,
               Some(other) => {
                   tracing::warn!(
                       value = %other,
                       "SIGNED_DISPATCH_ENFORCEMENT unknown value; falling back to env-appropriate default"
                   );
                   Self::default_for_env(environment)
               }
               None => Self::default_for_env(environment),
           }
       }
   }
   ```
4. Wire `SignedDispatchEnforcement::from_env(&environment)` into the existing config struct if there is a `Config`/`ServerConfig` struct that owns env-sourced fields — add a `pub signed_dispatch_enforcement: SignedDispatchEnforcement` field. Populate from `from_env` in the constructor/loader.
5. Unit tests at the bottom of `config.rs`:
   ```rust
   #[cfg(test)]
   mod enforcement_tests {
       use super::SignedDispatchEnforcement;

       #[test]
       fn default_strict_in_prod_audit_in_dev() {
           assert_eq!(SignedDispatchEnforcement::default_for_env("production"), SignedDispatchEnforcement::Strict);
           assert_eq!(SignedDispatchEnforcement::default_for_env("staging"), SignedDispatchEnforcement::Strict);
           assert_eq!(SignedDispatchEnforcement::default_for_env("development"), SignedDispatchEnforcement::Audit);
       }

       #[test]
       fn from_env_parses_all_three_values() {
           std::env::set_var("SIGNED_DISPATCH_ENFORCEMENT", "off");
           assert_eq!(SignedDispatchEnforcement::from_env("production"), SignedDispatchEnforcement::Off);
           std::env::set_var("SIGNED_DISPATCH_ENFORCEMENT", "audit");
           assert_eq!(SignedDispatchEnforcement::from_env("production"), SignedDispatchEnforcement::Audit);
           std::env::set_var("SIGNED_DISPATCH_ENFORCEMENT", "strict");
           assert_eq!(SignedDispatchEnforcement::from_env("production"), SignedDispatchEnforcement::Strict);
           std::env::remove_var("SIGNED_DISPATCH_ENFORCEMENT");
       }
   }
   ```
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-server config:: 2>&1 | tail -20</automated>
  </verify>
  <done>Enum compiles, default-by-env tests pass, env-parse tests pass, `cargo clippy -p roz-server --no-deps -- -D warnings` clean.</done>
</task>

<task type="auto">
  <name>Task 2: Add AppState fields (moka cache + signing-state handle)</name>
  <files>crates/roz-server/src/state.rs, crates/roz-server/src/main.rs</files>
  <action>
1. In `crates/roz-server/src/state.rs`, add to the `AppState` struct (append, do not restructure existing fields):
   ```rust
   use std::sync::Arc;
   use std::time::Duration;
   use ed25519_dalek::VerifyingKey;
   use moka::future::Cache;
   use uuid::Uuid;
   use roz_core::StaticKeyProvider;
   use crate::config::SignedDispatchEnforcement;

   #[derive(Clone)]
   pub struct AppState {
       // ... existing fields kept verbatim ...
       /// LRU cache for verifying keys. Key: (tenant_id, host_id, key_version).
       /// Value: Ed25519 32-byte verifying key. TTL 60s per D-11.
       pub verifying_key_cache: Cache<(Uuid, Uuid, u32), VerifyingKey>,

       /// KeyProvider used to decrypt the server's Ed25519 signing seed
       /// stored in roz_server_signing_state. Reuses the existing
       /// StaticKeyProvider (ROZ_ENCRYPTION_KEY env).
       pub key_provider: Arc<StaticKeyProvider>,

       /// Rollout gate (D-12).
       pub signed_dispatch_enforcement: SignedDispatchEnforcement,
   }
   ```
2. In the `AppState::new` (or `build`) constructor, initialize the cache and enforcement:
   ```rust
   let verifying_key_cache = Cache::builder()
       .max_capacity(10_000)
       .time_to_live(Duration::from_secs(60))
       .build();

   // StaticKeyProvider reads ROZ_ENCRYPTION_KEY at construction.
   let key_provider = Arc::new(StaticKeyProvider::from_env()
       .map_err(|e| anyhow::anyhow!("ROZ_ENCRYPTION_KEY missing/invalid: {e}"))?);

   let signed_dispatch_enforcement = SignedDispatchEnforcement::from_env(&environment);
   ```
   Pass `environment: &str` through to AppState constructor (either from the existing `ROZ_ENVIRONMENT` env-loading code in `main.rs`, or as a constructor argument).
3. In `crates/roz-server/src/main.rs`, thread the `environment` string from the existing config load into the AppState builder. If the existing code already has a `config.environment` or similar, reuse it. Log on startup:
   ```rust
   tracing::info!(
       enforcement = ?state.signed_dispatch_enforcement,
       "signed dispatch enforcement initialized"
   );
   ```
4. Add a doc-comment above each new field explaining the D-# decision it satisfies.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo check -p roz-server 2>&1 | tail -20</automated>
  </verify>
  <done>`AppState` has three new fields, constructor initializes all three, main.rs logs the enforcement value at startup; server crate compiles clean.</done>
</task>

<task type="auto">
  <name>Task 3: Add subject builders + routes/device.rs stub + wire in routes/mod.rs</name>
  <files>crates/roz-nats/src/subjects.rs, crates/roz-server/src/routes/mod.rs, crates/roz-server/src/routes/device.rs</files>
  <action>
1. In `crates/roz-nats/src/subjects.rs`, add two new builder methods to `impl Subjects` (follow the existing `event`/`telemetry` pattern, validating the token):
   ```rust
   /// Publish-only subject for worker-scoped signature-verification failures (D-09).
   /// Pattern: `safety.signature_failure.{host_id}`
   pub fn safety_signature_failure_worker(host_id: &str) -> Result<String, RozError> {
       validate_token("host_id", host_id)?;
       Ok(format!("safety.signature_failure.{host_id}"))
   }

   /// Publish-only subject for server-scoped (tenant-level) signature failures.
   /// REQUIREMENTS.md §FS-04 requires both worker and server scoped subjects.
   /// Pattern: `safety.signature_failure.server.{tenant_id}`
   pub fn safety_signature_failure_server(tenant_id: &str) -> Result<String, RozError> {
       validate_token("tenant_id", tenant_id)?;
       Ok(format!("safety.signature_failure.server.{tenant_id}"))
   }
   ```
   Add tests matching the pattern of `estop_subject`/`wasm_trust_failure_subject` (subjects.rs:273-306):
   ```rust
   #[test]
   fn safety_signature_failure_worker_subject() {
       assert_eq!(
           Subjects::safety_signature_failure_worker("abc").unwrap(),
           "safety.signature_failure.abc"
       );
       assert!(Subjects::safety_signature_failure_worker("bad.token").is_err());
       assert!(Subjects::safety_signature_failure_worker("").is_err());
   }

   #[test]
   fn safety_signature_failure_server_subject() {
       assert_eq!(
           Subjects::safety_signature_failure_server("tenant-7").unwrap(),
           "safety.signature_failure.server.tenant-7"
       );
       assert!(Subjects::safety_signature_failure_server("bad>token").is_err());
   }
   ```

2. Create stub `crates/roz-server/src/routes/device.rs`:
   ```rust
   //! Phase 23 device-key bootstrap + rotation routes (FS-04). Handlers are
   //! stubbed in Plan 23-04 and filled in Plan 23-05.

   use axum::{routing::post, Router};
   use http::StatusCode;

   use crate::state::AppState;

   pub fn device_routes() -> Router<AppState> {
       Router::new()
           .route("/v1/device/provision-key", post(provision_key_stub))
           .route("/v1/device/rotate-key", post(rotate_key_stub))
   }

   async fn provision_key_stub() -> StatusCode {
       // Filled by Plan 23-05.
       StatusCode::NOT_IMPLEMENTED
   }

   async fn rotate_key_stub() -> StatusCode {
       // Filled by Plan 23-05.
       StatusCode::NOT_IMPLEMENTED
   }
   ```

3. In `crates/roz-server/src/routes/mod.rs`, register the new module:
   ```rust
   pub mod device;
   ```
   And in the existing Router-building function (follow the pattern of existing `auth_keys`, `tasks` routes), merge `device::device_routes()` into the public router:
   ```rust
   .merge(device::device_routes())
   ```
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-nats subjects:: && cargo check -p roz-server 2>&1 | tail -20</automated>
  </verify>
  <done>Subject builder tests pass; device.rs stub compiles; routes/mod.rs merges the device router; `curl -X POST http://localhost:PORT/v1/device/provision-key` returns 501 (manual test not required; compile check suffices).</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| env var → config | SIGNED_DISPATCH_ENFORCEMENT is an operator-controlled kill switch; invalid values must fall back safely (`Strict` in prod). |
| LRU cache → verification | Cache entries have 60s TTL; revocation must invalidate synchronously (implemented in 23-05). |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-14 | Elevation of Privilege | operator sets SIGNED_DISPATCH_ENFORCEMENT=off in prod | accept | D-12 explicitly allows this for pre-v3.0 rollout; logged at startup; ops playbook documents production default. |
| T-23-15 | Denial of Service | cache memory exhaustion | mitigate | `max_capacity(10_000)` caps memory (<400KB at 32-byte keys). |
| T-23-16 | Spoofing | subject-builder accepts dangerous tokens | mitigate | `validate_token` rejects `.`, `*`, `>` in any user-supplied segment. |
</threat_model>

<verification>
- `cargo check -p roz-server && cargo check -p roz-nats` clean
- `cargo test -p roz-nats subjects::` passes (new + existing subjects)
- `cargo test -p roz-server config::` passes
- `cargo clippy -p roz-server --no-deps -- -D warnings` clean
- `cargo fmt --check` clean
</verification>

<success_criteria>
- `AppState` has `verifying_key_cache`, `key_provider`, `signed_dispatch_enforcement` fields
- `SignedDispatchEnforcement` enum loads from env with dev/prod default split
- `Subjects::safety_signature_failure_worker` and `safety_signature_failure_server` compile + have tests
- `routes/device.rs` stub exists with module registered in `routes/mod.rs`
- Commit: `feat(23-04): wire server plumbing for Phase 23 (cache, enforcement, subjects, stub routes)`
</success_criteria>

<output>
After completion, create `.planning/phases/23-.../23-04-SUMMARY.md` with: new AppState fields (type signatures), enforcement enum values, subject-builder signatures, stub route paths, and a confirmation that Wave 3 plans (23-05, 23-06) now have zero file overlap with each other.
</output>
