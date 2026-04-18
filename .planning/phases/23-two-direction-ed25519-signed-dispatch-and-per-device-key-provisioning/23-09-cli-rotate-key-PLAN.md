---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 09
type: execute
wave: 5
autonomous: true
objective: >
  Add `roz device rotate-key` CLI subcommand that invokes
  signing_key::force_rotate, prints the new key_version + created_at, and exits
  cleanly. Manual operator override per D-07.
depends_on:
  - "23-07"
files_modified:
  - crates/roz-cli/src/commands/device.rs
  - crates/roz-cli/src/commands/mod.rs
  - crates/roz-cli/src/main.rs
  - crates/roz-cli/Cargo.toml
requirements:
  - FS-04
task_count: 1

must_haves:
  truths:
    - "`roz device rotate-key` called on a provisioned worker host completes with exit 0 and prints the new key_version."
    - "`roz device rotate-key` on a host with no device key returns a clear error pointing the user to enrollment."
    - "CLI subcommand is discoverable via `roz device --help`."
  artifacts:
    - path: crates/roz-cli/src/commands/device.rs
      provides: "CLI subcommand `roz device rotate-key`"
      exports: ["DeviceCommand", "handle"]
  key_links:
    - from: crates/roz-cli/src/commands/device.rs
      to: crates/roz-worker/src/signing_key.rs
      via: "force_rotate call"
      pattern: "signing_key::force_rotate"
---

<objective>
Give operators a way to force-rotate a worker's signing key without waiting for the 90-day auto-rotation. Thin wrapper around `force_rotate` from Plan 23-07.

Purpose: D-07 says "roz device rotate-key CLI triggers immediate rotation." This plan implements that single operator-facing surface.
Output: One new CLI subcommand, registered in the existing `roz` binary.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@crates/roz-cli/src/main.rs
@crates/roz-cli/src/commands/mod.rs
@crates/roz-worker/src/signing_key.rs
@crates/roz-cli/src/config.rs

<interfaces>
<!-- From 23-07: -->
pub async fn force_rotate(
    current: &SigningKeyMaterial,
    dir: &Path,
    http: &reqwest::Client,
    api_url: &str,
    key_provider: &Arc<StaticKeyProvider>,
) -> Result<SigningKeyMaterial>;

pub async fn load(...) -> Result<Option<SigningKeyMaterial>>;

<!-- Existing CLI command pattern — clap-based subcommand enum. -->
<!-- See crates/roz-cli/src/main.rs + commands/mod.rs. -->
</interfaces>
</context>

<tasks>

<task type="auto">
  <name>Task 1: Add `roz device rotate-key` subcommand with integration test</name>
  <files>crates/roz-cli/src/commands/device.rs, crates/roz-cli/src/commands/mod.rs, crates/roz-cli/src/main.rs, crates/roz-cli/Cargo.toml</files>
  <action>
1. `crates/roz-cli/Cargo.toml` `[dependencies]`:
   ```toml
   roz-worker = { path = "../roz-worker" }    # confirm — add only if not present
   roz-core   = { workspace = true }          # likely already present
   reqwest    = { workspace = true }          # confirm
   ```

2. Create `crates/roz-cli/src/commands/device.rs`:
   ```rust
   //! `roz device` subcommand family.

   use std::sync::Arc;

   use anyhow::{bail, Context, Result};
   use clap::Subcommand;
   use roz_core::StaticKeyProvider;
   use roz_worker::signing_key;

   #[derive(Debug, Subcommand)]
   pub enum DeviceCommand {
       /// Force-rotate this host's device signing key. Calls
       /// POST /v1/device/rotate-key with the current key as the signer.
       /// Manual override of the 90-day auto-rotate policy (D-07).
       RotateKey,
   }

   pub async fn handle(cmd: DeviceCommand) -> Result<()> {
       match cmd {
           DeviceCommand::RotateKey => rotate_key().await,
       }
   }

   async fn rotate_key() -> Result<()> {
       // Load config from the same sources the worker uses (env + config file).
       let config = crate::config::load_worker_config()
           .context("load config (need ROZ_API_URL + tenant_id/host_id + ROZ_ENCRYPTION_KEY)")?;

       let provider = Arc::new(StaticKeyProvider::from_env()
           .context("ROZ_ENCRYPTION_KEY missing")?);
       let http = reqwest::Client::builder().build()?;
       let dir = signing_key::data_dir();

       let current = signing_key::load(&dir, &provider, config.tenant_id, config.host_id).await?
           .ok_or_else(|| anyhow::anyhow!(
               "no device key on disk at {}; run `roz` worker startup to enroll first",
               dir.display()
           ))?;

       println!(
           "Current key version: {} (created {})",
           current.key_version, current.created_at
       );

       let new_mat = signing_key::force_rotate(
           &current, &dir, &http, &config.api_url, &provider
       ).await.context("POST /v1/device/rotate-key")?;

       println!(
           "Rotated: new key version {} (created {})",
           new_mat.key_version, new_mat.created_at
       );
       println!("Old key remains valid for 24 h overlap (D-07).");

       Ok(())
   }
   ```

3. Register in `crates/roz-cli/src/commands/mod.rs`:
   ```rust
   pub mod device;
   ```

4. In `crates/roz-cli/src/main.rs`, add to the top-level `Commands` enum:
   ```rust
   #[derive(Subcommand)]
   enum Commands {
       // ... existing ...
       /// Device-key management (bootstrap, rotation).
       Device {
           #[command(subcommand)]
           cmd: commands::device::DeviceCommand,
       },
   }
   ```
   And in the main `match` dispatch:
   ```rust
   Commands::Device { cmd } => commands::device::handle(cmd).await?,
   ```

5. Add `crate::config::load_worker_config()` helper if it doesn't exist. Expected return shape:
   ```rust
   pub struct WorkerCliConfig {
       pub api_url: String,
       pub tenant_id: Uuid,
       pub host_id: Uuid,
   }
   ```
   Reuse whatever `crates/roz-cli/src/config.rs` already exposes — the CLI already reads host identity from config; this is a thin accessor on top. If the existing config code doesn't expose these three fields together, add a 10-line helper that reads them from the same sources (env + config file + keyring if applicable).

6. Add a simple CLI test `crates/roz-cli/tests/device_rotate.rs`:
   ```rust
   //! Basic smoke + error-path test for `roz device rotate-key`.

   use assert_cmd::Command;

   #[test]
   fn rotate_key_fails_cleanly_without_enrollment() {
       let tmp = tempfile::TempDir::new().unwrap();
       Command::cargo_bin("roz").unwrap()
           .env("ROZ_DATA_DIR", tmp.path())
           .env("ROZ_ENCRYPTION_KEY", base64::engine::general_purpose::STANDARD.encode([7u8; 32]))
           .env("ROZ_API_URL", "http://127.0.0.1:1")
           .args(["device", "rotate-key"])
           .assert()
           .failure()
           .stderr(predicates::str::contains("no device key on disk"));
   }

   #[test]
   fn device_help_lists_rotate_key() {
       Command::cargo_bin("roz").unwrap()
           .args(["device", "--help"])
           .assert()
           .success()
           .stdout(predicates::str::contains("rotate-key"));
   }
   ```
   (Requires `assert_cmd` and `predicates` in `[dev-dependencies]` — check; likely already present from other CLI tests.)

Full happy-path test (actually rotates against a live server) is deferred to the Plan 23-05 integration suite which already has the server-up testcontainers harness.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-cli --test device_rotate && cargo clippy -p roz-cli --no-deps -- -D warnings 2>&1 | tail -20</automated>
  </verify>
  <done>`roz device rotate-key` available; clean error on missing key; `roz device --help` lists the subcommand; compiles + clippy clean.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| operator terminal → CLI → server | Operator runs this with the same env as the worker; no new secret material exposed. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-37 | Elevation of Privilege | attacker with shell access forces rotations | accept | Requires read access to `/etc/roz/device-key-v*.pem` (which is root-owned, mode 0600) AND ROZ_ENCRYPTION_KEY. Attacker at that level already owns the host. |
| T-23-38 | Denial of Service | operator rotates in a loop | accept | Server rotate-key is not rate-limited (only provision-key is); operator-initiated rotation is a trusted path. |
</threat_model>

<verification>
- `cargo test -p roz-cli --test device_rotate` passes
- `cargo clippy -p roz-cli --no-deps -- -D warnings` clean
- Manual smoke: with a real worker + server up, `roz device rotate-key` prints old + new versions; DB shows both rows
</verification>

<success_criteria>
- `roz device rotate-key` subcommand works end-to-end
- Clear error when no device key is on disk
- Subcommand surfaces in `roz device --help`
- Commit: `feat(23-09): add roz device rotate-key CLI command`
</success_criteria>

<output>
After completion, create `.planning/phases/23-.../23-09-SUMMARY.md` with: command invocation, config sources, output format, error paths.
</output>
