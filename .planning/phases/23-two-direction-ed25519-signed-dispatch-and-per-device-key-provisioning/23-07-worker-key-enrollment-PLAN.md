---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 07
type: execute
wave: 4
autonomous: true
objective: >
  Worker-side key bootstrap + storage + counter. Adds signing_key.rs (load/save
  AES-GCM-encrypted Ed25519 seed at /etc/roz/device-key.pem or
  $ROZ_DATA_DIR/device-key.pem, mode 0600), extends registration.rs to call
  POST /v1/device/provision-key after host registration (enrolling fresh
  workers), extends wal.rs with signing_sequence_counter table + next_seq()
  helper. Also does 24h startup rotation age-check + optional auto-rotate call
  to POST /v1/device/rotate-key (D-07).
depends_on:
  - "23-05"
files_modified:
  - crates/roz-worker/src/signing_key.rs
  - crates/roz-worker/src/registration.rs
  - crates/roz-worker/src/wal.rs
  - crates/roz-worker/src/lib.rs
  - crates/roz-worker/Cargo.toml
requirements:
  - FS-04
task_count: 3

must_haves:
  truths:
    - "Fresh worker with no device key + valid ROZ_API_KEY: startup enrolls via POST /v1/device/provision-key and persists the returned seed encrypted at rest."
    - "Worker on startup reads device-key.pem, decrypts via StaticKeyProvider (ROZ_ENCRYPTION_KEY), constructs SigningKey without leaking plaintext to logs."
    - "Worker also caches the server's verifying key (from provision/rotate response) at $ROZ_DATA_DIR/server-verifying-key.pem (plaintext, 32 bytes — it's a public key)."
    - "WAL has signing_sequence_counter table; next_seq(key_version) returns a monotonically increasing u64 via atomic ON CONFLICT DO UPDATE RETURNING."
    - "Missing/corrupt/undecryptable device key at runtime: worker hard-stops with exit code 78 (EX_CONFIG) and a clear log (D-09)."
    - "On startup, worker checks key.created_at; if > 90 days it calls rotate-key; both old and new keys kept on disk for the 24 h overlap (D-07)."
  artifacts:
    - path: crates/roz-worker/src/signing_key.rs
      provides: "load_or_enroll, load, save, rotate_if_due, SigningKeyMaterial struct"
      exports: ["SigningKeyMaterial", "load_or_enroll", "rotate_if_due"]
    - path: crates/roz-worker/src/wal.rs
      provides: "signing_sequence_counter table + next_seq helper"
      contains: "signing_sequence_counter"
  key_links:
    - from: crates/roz-worker/src/signing_key.rs
      to: crates/roz-core/src/key_provider.rs
      via: "StaticKeyProvider::encrypt / decrypt"
      pattern: "key_provider"
    - from: crates/roz-worker/src/registration.rs
      to: crates/roz-server/src/routes/device.rs
      via: "HTTP POST /v1/device/provision-key"
      pattern: "/v1/device/provision-key"
---

<objective>
Get the worker enrolled. Before Plan 23-08 can wire signing into every publish site, the worker needs (a) a persistent per-device keypair, (b) a way to get one from the server on first run, (c) a place to persist the monotonic sequence counter, and (d) the server's verifying key so it can verify inbound dispatch.

Purpose: Make every fresh worker start sign-ready. Key material is at rest encrypted, runtime failures are fail-closed with a distinct exit code for ops dashboards (Q5).
Output: Three new module surfaces — `signing_key.rs` (load/save/enroll/rotate), an extended `wal.rs` (counter table), an extended `registration.rs` (enrollment call). No signing/verify wiring at publish/subscribe sites yet — that's 23-08.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@.planning/research/DEEP-SIGN.md
@crates/roz-worker/src/registration.rs
@crates/roz-worker/src/wal.rs
@crates/roz-worker/src/lib.rs
@crates/roz-worker/src/config.rs
@crates/roz-core/src/key_provider.rs
@crates/roz-core/src/signing/mod.rs

<interfaces>
<!-- From 23-02: -->
use roz_core::signing::{SignedFields, Direction, SignatureEnvelope, SignatureError, sign_envelope, verify_envelope, HEADER_NAME};

<!-- From 23-05 HTTP response shape: -->
// provision-key response: { private_key_seed_b64, key_version, server_verifying_key_b64 }
// rotate-key response:    { private_key_seed_b64, key_version, server_verifying_key_b64 }

<!-- Existing worker config: reads ROZ_API_KEY, ROZ_API_URL, ROZ_DATA_DIR. -->
<!-- See crates/roz-worker/src/config.rs. -->

<!-- Existing WAL schema in crates/roz-worker/src/wal.rs: -->
// CREATE TABLE IF NOT EXISTS wal_entries (...)
// CREATE TABLE IF NOT EXISTS worker_state (...)
// CREATE TABLE IF NOT EXISTS idempotency_cache (...)
</interfaces>
</context>

<planners_discretion>
- **Hard-stop exit code (Q5):** Exit 78 (`EX_CONFIG`, sysexits.h convention) on missing/corrupt device key. Distinct from general panic exit codes; surfaces clearly in systemd health dashboards.
- **Server verifying key storage:** Plaintext at `$ROZ_DATA_DIR/server-verifying-key.pem` — it's a public key, not secret. Storing with the device private key keeps them on the same filesystem.
- **Rotation key retention:** During the 24 h overlap, keep BOTH keys on disk — `device-key-v{N}.pem` naming. Worker reads the highest-numbered file as "active" for outbound signing. Inbound verification (worker→server direction) isn't done on this plan's path; the worker only verifies server→worker, and that uses the cached server verifying key, not its own key.
</planners_discretion>

<tasks>

<task type="auto">
  <name>Task 1: Create signing_key.rs (load / save / enroll / rotate_if_due, file mode 0600)</name>
  <files>crates/roz-worker/src/signing_key.rs, crates/roz-worker/src/lib.rs, crates/roz-worker/Cargo.toml</files>
  <action>
Create `crates/roz-worker/src/signing_key.rs`:

```rust
//! Worker-side device keypair loader/saver (Phase 23 FS-04).
//!
//! File layout:
//!   {data_dir}/device-key-v{version}.pem   — AES-256-GCM-encrypted 32-byte seed
//!   {data_dir}/server-verifying-key.pem    — plaintext 32-byte server public key
//!
//! where {data_dir} is:
//!   - /etc/roz            (production; owner roz-worker, mode 0700)
//!   - ${ROZ_DATA_DIR}     (dev/sim override)
//!   - ~/.config/roz       (fallback)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{SigningKey, VerifyingKey};
use roz_core::{KeyProvider, StaticKeyProvider};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

const ROTATION_INTERVAL: Duration = Duration::days(90);

#[derive(Debug, Serialize, Deserialize)]
struct EncryptedKeyFile {
    ciphertext_b64: String,
    nonce_b64: String,
    key_version: u32,
    created_at: DateTime<Utc>,
}

/// Material the worker needs for outbound signing + inbound verifying.
#[derive(Clone)]
pub struct SigningKeyMaterial {
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    pub key_version: u32,
    pub signing_key: SigningKey,                 // worker private
    pub server_verifying_key: VerifyingKey,      // server public (for inbound verify)
    pub created_at: DateTime<Utc>,
}

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ROZ_DATA_DIR") { return PathBuf::from(d); }
    let prod = PathBuf::from("/etc/roz");
    if prod.is_dir() { return prod; }
    dirs::config_dir()
        .map(|d| d.join("roz"))
        .unwrap_or_else(|| PathBuf::from(".roz"))
}

fn device_key_path(dir: &Path, version: u32) -> PathBuf {
    dir.join(format!("device-key-v{version}.pem"))
}

fn server_verifying_key_path(dir: &Path) -> PathBuf {
    dir.join("server-verifying-key.pem")
}

/// Load the highest-version device key from disk. Returns `None` if no key
/// file exists — caller should enroll.
pub async fn load(
    dir: &Path,
    key_provider: &Arc<StaticKeyProvider>,
    tenant_id: Uuid,
    host_id: Uuid,
) -> Result<Option<SigningKeyMaterial>> {
    // Discover the highest version.
    let mut highest: Option<u32> = None;
    if !dir.is_dir() { return Ok(None); }
    for entry in std::fs::read_dir(dir).context("read data dir")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(rest) = name.strip_prefix("device-key-v").and_then(|s| s.strip_suffix(".pem")) {
            if let Ok(v) = rest.parse::<u32>() {
                highest = Some(highest.map_or(v, |h| h.max(v)));
            }
        }
    }
    let Some(version) = highest else { return Ok(None); };

    let path = device_key_path(dir, version);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let file: EncryptedKeyFile = serde_json::from_slice(&bytes)
        .context("parse device key file")?;

    let ct = B64.decode(file.ciphertext_b64.as_bytes()).context("decode ciphertext")?;
    let nonce = B64.decode(file.nonce_b64.as_bytes()).context("decode nonce")?;
    let nonce_12: [u8; 12] = nonce.as_slice().try_into().context("nonce must be 12 bytes")?;
    let seed = key_provider.decrypt(tenant_id, &ct, &nonce_12).await
        .context("decrypt device key")?;
    let seed: [u8; 32] = seed.as_slice().try_into().context("seed must be 32 bytes")?;

    // Load server verifying key (plaintext).
    let svk_bytes = std::fs::read(server_verifying_key_path(dir))
        .context("read server verifying key")?;
    let svk_arr: [u8; 32] = svk_bytes.as_slice().try_into()
        .context("server verifying key must be 32 bytes")?;
    let server_verifying_key = VerifyingKey::from_bytes(&svk_arr)
        .context("parse server verifying key")?;

    Ok(Some(SigningKeyMaterial {
        tenant_id,
        host_id,
        key_version: file.key_version,
        signing_key: SigningKey::from_bytes(&seed),
        server_verifying_key,
        created_at: file.created_at,
    }))
}

/// Persist a seed + version + server verifying key to disk. Writes the device
/// key file with mode 0600 (POSIX). On non-POSIX platforms the mode setter is
/// a no-op — those are dev/test only.
pub async fn save(
    dir: &Path,
    key_provider: &Arc<StaticKeyProvider>,
    tenant_id: Uuid,
    key_version: u32,
    seed: &[u8; 32],
    server_verifying_key: &[u8; 32],
) -> Result<()> {
    std::fs::create_dir_all(dir).context("mkdir data dir")?;
    let (ciphertext, nonce) = key_provider.encrypt(tenant_id, seed).await
        .context("encrypt seed")?;
    let file = EncryptedKeyFile {
        ciphertext_b64: B64.encode(&ciphertext),
        nonce_b64: B64.encode(&nonce),
        key_version,
        created_at: Utc::now(),
    };
    let bytes = serde_json::to_vec(&file)?;
    let path = device_key_path(dir, key_version);
    std::fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
    set_mode_0600(&path).ok();    // best effort on non-POSIX

    std::fs::write(server_verifying_key_path(dir), server_verifying_key)
        .context("write server verifying key")?;

    Ok(())
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    perm.set_mode(0o600);
    std::fs::set_permissions(path, perm)
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> std::io::Result<()> { Ok(()) }

/// Bootstrap path: if no key on disk, call POST /v1/device/provision-key.
pub async fn load_or_enroll(
    dir: &Path,
    http: &reqwest::Client,
    api_url: &str,
    api_key: &str,
    key_provider: &Arc<StaticKeyProvider>,
    tenant_id: Uuid,
    host_id: Uuid,
) -> Result<SigningKeyMaterial> {
    if let Some(material) = load(dir, key_provider, tenant_id, host_id).await? {
        return Ok(material);
    }

    // Enroll.
    tracing::info!(host_id = %host_id, "no device key on disk; enrolling via /v1/device/provision-key");

    #[derive(Deserialize)]
    struct ProvisionResp {
        private_key_seed_b64: String,
        key_version: u32,
        server_verifying_key_b64: String,
    }

    let resp: ProvisionResp = http
        .post(format!("{api_url}/v1/device/provision-key"))
        .bearer_auth(api_key)
        .send().await.context("POST /v1/device/provision-key")?
        .error_for_status().context("provision-key failed")?
        .json().await.context("parse provision response")?;

    let seed_vec = B64.decode(resp.private_key_seed_b64.as_bytes())?;
    let seed: [u8; 32] = seed_vec.as_slice().try_into().context("seed bytes != 32")?;
    let svk_vec = B64.decode(resp.server_verifying_key_b64.as_bytes())?;
    let svk: [u8; 32] = svk_vec.as_slice().try_into().context("svk bytes != 32")?;

    save(dir, key_provider, tenant_id, resp.key_version, &seed, &svk).await?;

    Ok(SigningKeyMaterial {
        tenant_id,
        host_id,
        key_version: resp.key_version,
        signing_key: SigningKey::from_bytes(&seed),
        server_verifying_key: VerifyingKey::from_bytes(&svk).context("bad svk")?,
        created_at: Utc::now(),
    })
}

/// Age-check + rotation. Called at startup after `load_or_enroll` and
/// periodically via the heartbeat loop.
pub async fn rotate_if_due(
    current: &SigningKeyMaterial,
    dir: &Path,
    http: &reqwest::Client,
    api_url: &str,
    key_provider: &Arc<StaticKeyProvider>,
) -> Result<Option<SigningKeyMaterial>> {
    if Utc::now() - current.created_at < ROTATION_INTERVAL {
        return Ok(None);
    }
    force_rotate(current, dir, http, api_url, key_provider).await.map(Some)
}

/// Unconditional rotation — used by `roz device rotate-key` (Plan 23-09) and
/// by `rotate_if_due` when age exceeds the interval.
pub async fn force_rotate(
    current: &SigningKeyMaterial,
    dir: &Path,
    http: &reqwest::Client,
    api_url: &str,
    key_provider: &Arc<StaticKeyProvider>,
) -> Result<SigningKeyMaterial> {
    use roz_core::signing::{
        payload_sha256_hex, sign_envelope, Direction, SignedFields, HEADER_NAME,
    };

    // Build signed-body rotate-key request.
    let body = serde_json::json!({
        "tenant_id": current.tenant_id,
        "host_id": current.host_id,
        "current_key_version": current.key_version,
    });
    let body_bytes = serde_json::to_vec(&body)?;
    let fields = SignedFields {
        direction: Direction::WorkerToServer,
        tenant_id: current.tenant_id,
        host_id: current.host_id,
        correlation_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        sequence_number: 0,        // rotate-key uses single-shot envelope; server does not advance worker-scoped seq here
        payload_hash: payload_sha256_hex(&body_bytes),
        key_version: current.key_version,
    };
    let envelope = sign_envelope(&fields, &current.signing_key)?;
    let header = envelope.encode_header()?;

    #[derive(Deserialize)]
    struct RotateResp {
        private_key_seed_b64: String,
        key_version: u32,
        server_verifying_key_b64: String,
    }

    let resp: RotateResp = http
        .post(format!("{api_url}/v1/device/rotate-key"))
        .header(HEADER_NAME, header)
        .header("content-type", "application/json")
        .body(body_bytes)
        .send().await.context("POST /v1/device/rotate-key")?
        .error_for_status().context("rotate-key failed")?
        .json().await?;

    let seed_vec = B64.decode(resp.private_key_seed_b64.as_bytes())?;
    let seed: [u8; 32] = seed_vec.as_slice().try_into().context("seed bytes != 32")?;
    let svk_vec = B64.decode(resp.server_verifying_key_b64.as_bytes())?;
    let svk: [u8; 32] = svk_vec.as_slice().try_into().context("svk bytes != 32")?;

    save(dir, key_provider, current.tenant_id, resp.key_version, &seed, &svk).await?;

    Ok(SigningKeyMaterial {
        tenant_id: current.tenant_id,
        host_id: current.host_id,
        key_version: resp.key_version,
        signing_key: SigningKey::from_bytes(&seed),
        server_verifying_key: VerifyingKey::from_bytes(&svk).context("bad svk")?,
        created_at: Utc::now(),
    })
}
```

1. Add `pub mod signing_key;` to `crates/roz-worker/src/lib.rs`.
2. In `crates/roz-worker/Cargo.toml` `[dependencies]`, confirm/add:
   - `reqwest` (already present)
   - `roz-core` (already present)
   - `ed25519-dalek` (already present via `roz-core` — add direct dep if not already there)
   - `dirs = "5"` (or whatever version matches workspace — confirm)
   - `base64` (already present)
   - `anyhow` (already present)

3. Add unit tests at the bottom of `signing_key.rs`:
   ```rust
   #[cfg(test)]
   mod tests {
       use super::*;
       use tempfile::TempDir;

       fn test_provider() -> Arc<StaticKeyProvider> {
           // Uses the same deterministic test key as roz-core StaticKeyProvider::for_tests().
           Arc::new(StaticKeyProvider::for_tests())
       }

       #[tokio::test]
       async fn save_then_load_round_trip() {
           let dir = TempDir::new().unwrap();
           let provider = test_provider();
           let tenant = Uuid::new_v4();
           let host = Uuid::new_v4();
           let seed = [7u8; 32];
           let svk = [11u8; 32];
           save(dir.path(), &provider, tenant, 1, &seed, &svk).await.unwrap();

           let mat = load(dir.path(), &provider, tenant, host).await.unwrap().unwrap();
           assert_eq!(mat.key_version, 1);
           assert_eq!(mat.signing_key.to_bytes(), seed);
           assert_eq!(mat.server_verifying_key.to_bytes(), svk);
       }

       #[tokio::test]
       async fn load_picks_highest_version() {
           let dir = TempDir::new().unwrap();
           let provider = test_provider();
           let tenant = Uuid::new_v4();
           let host = Uuid::new_v4();
           save(dir.path(), &provider, tenant, 1, &[1u8; 32], &[2u8; 32]).await.unwrap();
           save(dir.path(), &provider, tenant, 2, &[3u8; 32], &[4u8; 32]).await.unwrap();
           let mat = load(dir.path(), &provider, tenant, host).await.unwrap().unwrap();
           assert_eq!(mat.key_version, 2);
       }

       #[tokio::test]
       async fn load_missing_returns_none() {
           let dir = TempDir::new().unwrap();
           let provider = test_provider();
           assert!(load(dir.path(), &provider, Uuid::new_v4(), Uuid::new_v4()).await.unwrap().is_none());
       }

       #[tokio::test]
       async fn load_corrupt_ciphertext_errs() {
           let dir = TempDir::new().unwrap();
           let provider = test_provider();
           let tenant = Uuid::new_v4();
           save(dir.path(), &provider, tenant, 1, &[7u8; 32], &[8u8; 32]).await.unwrap();
           // Corrupt the file.
           let path = device_key_path(dir.path(), 1);
           std::fs::write(&path, b"not-json").unwrap();
           assert!(load(dir.path(), &provider, tenant, Uuid::new_v4()).await.is_err());
       }

       #[cfg(unix)]
       #[tokio::test]
       async fn saved_file_has_mode_0600() {
           use std::os::unix::fs::PermissionsExt;
           let dir = TempDir::new().unwrap();
           let provider = test_provider();
           save(dir.path(), &provider, Uuid::new_v4(), 1, &[7u8; 32], &[8u8; 32]).await.unwrap();
           let perms = std::fs::metadata(device_key_path(dir.path(), 1)).unwrap().permissions();
           assert_eq!(perms.mode() & 0o777, 0o600);
       }
   }
   ```

Note: `StaticKeyProvider::for_tests()` may not exist — if not, add a 4-line helper to `crates/roz-core/src/key_provider.rs`:
```rust
#[cfg(any(test, feature = "test-support"))]
pub fn for_tests() -> Self {
    StaticKeyProvider::from_bytes(&[7u8; 32])
}
```
Only add if missing. Use whatever existing test helper pattern already works in the repo (the key_provider.rs preamble references a `provider()` helper at `:214`).
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker signing_key:: 2>&1 | tail -30</automated>
  </verify>
  <done>All signing_key unit tests pass; `cargo clippy -p roz-worker --no-deps -- -D warnings` clean; no new workspace deps except `dirs` if not already present.</done>
</task>

<task type="auto">
  <name>Task 2: Extend wal.rs with signing_sequence_counter table + next_seq helper</name>
  <files>crates/roz-worker/src/wal.rs</files>
  <action>
In `crates/roz-worker/src/wal.rs`:

1. In the `CREATE TABLE IF NOT EXISTS` batch inside `WalStore::open` (around line 25-44 per RESEARCH.md), add:
   ```sql
   CREATE TABLE IF NOT EXISTS signing_sequence_counter (
       key_version INTEGER PRIMARY KEY,
       seq         INTEGER NOT NULL
   );
   ```

2. Add a method:
   ```rust
   impl WalStore {
       /// Atomically allocate the next sequence number for a given key_version.
       /// Creates the row on first call (seq = 1), increments otherwise.
       ///
       /// SQLite's `RETURNING` (3.35+) gives us atomicity under WAL mode.
       pub fn next_seq(&self, key_version: u32) -> rusqlite::Result<u64> {
           let conn = self.conn.lock().expect("wal mutex poisoned");
           let row: i64 = conn.query_row(
               "INSERT INTO signing_sequence_counter (key_version, seq) VALUES (?1, 1)
                ON CONFLICT(key_version) DO UPDATE SET seq = seq + 1
                RETURNING seq",
               rusqlite::params![key_version],
               |r| r.get(0),
           )?;
           Ok(row as u64)
       }
   }
   ```
   (If `self.conn` is not a `Mutex<Connection>` — check actual structure — adapt accordingly.)

3. Add unit tests at the bottom of `wal.rs` (there's an existing test module — append to it):
   ```rust
   #[test]
   fn next_seq_starts_at_one_and_monotonically_increases() {
       let wal = WalStore::open_in_memory().unwrap();
       assert_eq!(wal.next_seq(1).unwrap(), 1);
       assert_eq!(wal.next_seq(1).unwrap(), 2);
       assert_eq!(wal.next_seq(1).unwrap(), 3);
   }

   #[test]
   fn next_seq_separate_per_key_version() {
       let wal = WalStore::open_in_memory().unwrap();
       assert_eq!(wal.next_seq(1).unwrap(), 1);
       assert_eq!(wal.next_seq(2).unwrap(), 1);    // fresh counter for v2
       assert_eq!(wal.next_seq(1).unwrap(), 2);    // v1 unchanged
   }

   #[test]
   fn next_seq_survives_reopen() {
       let tmp = tempfile::tempdir().unwrap();
       let path = tmp.path().join("wal.db");
       {
           let wal = WalStore::open(&path).unwrap();
           assert_eq!(wal.next_seq(1).unwrap(), 1);
           assert_eq!(wal.next_seq(1).unwrap(), 2);
       }
       let wal = WalStore::open(&path).unwrap();
       assert_eq!(wal.next_seq(1).unwrap(), 3);
   }
   ```

   If `open_in_memory` doesn't exist, use `WalStore::open(&tmp.path().join("x.db"))`.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker wal:: 2>&1 | tail -20</automated>
  </verify>
  <done>Three new WAL tests pass; existing WAL tests still pass; clippy clean.</done>
</task>

<task type="auto">
  <name>Task 3: Wire registration.rs to call load_or_enroll + startup rotate-if-due; hard-stop on key failure</name>
  <files>crates/roz-worker/src/registration.rs, crates/roz-worker/src/lib.rs</files>
  <action>
1. In `crates/roz-worker/src/registration.rs`, after the existing `register_host()` returns the `host_id`, add a post-registration step:

   ```rust
   use crate::signing_key;

   pub async fn bootstrap_device_key(
       http: &reqwest::Client,
       api_url: &str,
       api_key: &str,
       key_provider: &Arc<StaticKeyProvider>,
       tenant_id: Uuid,
       host_id: Uuid,
   ) -> anyhow::Result<signing_key::SigningKeyMaterial> {
       let dir = signing_key::data_dir();
       let current = signing_key::load_or_enroll(
           &dir, http, api_url, api_key, key_provider, tenant_id, host_id,
       ).await?;

       // D-07: 90-day age check + rotate.
       match signing_key::rotate_if_due(&current, &dir, http, api_url, key_provider).await {
           Ok(Some(new_mat)) => {
               tracing::info!(
                   old_version = current.key_version,
                   new_version = new_mat.key_version,
                   "device key rotated (age > 90d)",
               );
               Ok(new_mat)
           }
           Ok(None) => Ok(current),
           Err(e) => {
               tracing::error!(err = %e, "rotate-if-due failed; keeping current key");
               Ok(current)
           }
       }
   }
   ```

2. In the worker's main startup flow (`crates/roz-worker/src/main.rs` — do NOT modify beyond the call site; the full sign-hook wiring is Plan 23-08), add a minimal call after registration:

   ```rust
   // In the startup function (around where register_host is called):
   let signing_material = match registration::bootstrap_device_key(
       &http, &config.api_url, &config.api_key, &key_provider, tenant_id, host_id
   ).await {
       Ok(m) => m,
       Err(e) => {
           tracing::error!(err = ?e, "device key bootstrap failed; hard-stop (exit 78 EX_CONFIG, D-09)");
           std::process::exit(78);
       }
   };

   // Stash in worker state for use by Plan 23-08 wiring.
   // (The full use of signing_material — at every publish site — lands in 23-08.)
   ```

   This plan does NOT touch the publish/subscribe sites — those are Plan 23-08's concern. Here we only prove that (a) enrollment completes, (b) material is in memory, (c) missing/corrupt key hard-stops.

3. Add an integration test to `crates/roz-worker/tests/` that spins up a testcontainers roz-server + Postgres + NATS, invokes `bootstrap_device_key` with a test API key, asserts:
   - Key file appears under `$ROZ_DATA_DIR`
   - A row appears in `roz_device_keys` in the DB
   - A row appears in `roz_server_signing_state`
   - Re-running `bootstrap_device_key` loads the existing key (no re-enrollment)

   Test name: `device_key_bootstrap_e2e`. Match the existing pattern in `crates/roz-worker/tests/dispatch_integration.rs`.

4. Add a hard-stop unit test that simulates corrupt key + confirms the function returns Err, which the caller converts to exit 78:
   ```rust
   #[tokio::test]
   async fn corrupt_key_file_surfaces_error() {
       let dir = TempDir::new().unwrap();
       std::env::set_var("ROZ_DATA_DIR", dir.path());
       // Write garbage.
       std::fs::write(dir.path().join("device-key-v1.pem"), b"not-json").unwrap();
       // load_or_enroll should fail at the load step (before attempting to enroll).
       let res = signing_key::load(
           dir.path(),
           &Arc::new(StaticKeyProvider::for_tests()),
           Uuid::new_v4(), Uuid::new_v4()
       ).await;
       assert!(res.is_err());
   }
   ```
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker registration:: signing_key:: -- --include-ignored 2>&1 | tail -30</automated>
  </verify>
  <done>Fresh worker startup enrolls and persists keys; re-startup loads without re-enrolling; corrupt file surfaces error → exit 78; DB rows match.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| filesystem → process memory | Device key file read must fail-closed on any parse/decrypt error. |
| HTTP round-trip → persisted key | Network MITM could mint a fake server verifying key during provision; mitigated by bearer auth + TLS (existing) + one-time provision per host. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-28 | Information Disclosure | device key readable by other users | mitigate | File mode 0600 set on write; creation mask respected; non-POSIX is dev only. |
| T-23-29 | Tampering | key file replaced by attacker offline | mitigate | AES-GCM auth tag detects any tamper; decrypt fails → hard-stop exit 78. |
| T-23-30 | Denial of Service | worker stuck retrying enrollment | accept | Fresh-worker enrollment is one-shot at startup; failure → exit 78 → ops pages. No retry storm. |
| T-23-31 | Replay | old private key re-used after rotation | mitigate | Old key file kept only for 24h rotation overlap; `load()` picks highest-version file. |
</threat_model>

<verification>
- `cargo test -p roz-worker signing_key:: wal:: registration::` clean
- `cargo clippy -p roz-worker --no-deps -- -D warnings` clean
- Integration test exercises real enrollment against testcontainers server
- Exit code 78 on hard-stop (verified by spawning worker as child process in integration test)
</verification>

<success_criteria>
- `signing_key::load_or_enroll` is the single bootstrap entry
- `signing_key::rotate_if_due` fires at 90 days
- Key files are AES-GCM encrypted at rest, mode 0600
- Server verifying key persisted (plaintext — it's public)
- WAL has `signing_sequence_counter` + `next_seq` helper
- Hard-stop exit 78 on key failure
- Commit: `feat(23-07): worker-side device-key enrollment + rotation + WAL sequence counter`
</success_criteria>

<output>
After completion, create `.planning/phases/23-.../23-07-SUMMARY.md` with: file paths, enrollment HTTP flow, rotation logic, WAL schema extension, and exit-code convention.
</output>
