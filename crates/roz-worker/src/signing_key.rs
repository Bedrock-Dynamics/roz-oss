//! Worker-side device keypair loader/saver (Phase 23 FS-04, plan 23-07).
//!
//! Persists a per-device Ed25519 signing key at rest, encrypted by the
//! process-level `ROZ_ENCRYPTION_KEY` through `StaticKeyProvider`. Also caches
//! the server's verifying key (plaintext — it's public) next to the encrypted
//! device key so the worker can verify inbound `server→worker` envelopes
//! without a separate bootstrap round-trip (D-15 piggyback).
//!
//! # File layout
//!
//! ```text
//! {data_dir}/device-key-v{version}.pem   AES-256-GCM-encrypted 32-byte seed
//! {data_dir}/server-verifying-key.pem    plaintext 32-byte server public key
//! ```
//!
//! where `{data_dir}` resolves as:
//!
//! 1. `$ROZ_DATA_DIR` if set (dev / sim / CI override).
//! 2. `/etc/roz` if it already exists as a directory (production install layout).
//! 3. OS-appropriate user config directory (`dirs::config_dir()/roz`) as a
//!    fallback for developer workstations.
//!
//! # On-disk encryption format
//!
//! Each `device-key-v{version}.pem` is JSON with four fields:
//!
//! ```json
//! { "ciphertext_b64": "...", "nonce_b64": "...", "key_version": 1,
//!   "created_at": "2026-04-17T12:00:00Z" }
//! ```
//!
//! The seed is wrapped in a `URL_SAFE_NO_PAD`-base64 `SecretString` before
//! encryption so the ciphertext format matches the server's
//! `roz_server_signing_state` format exactly — any change to the wrapping must
//! stay in lockstep with `crates/roz-server/src/routes/device.rs`.
//!
//! # Rotation (D-07)
//!
//! Old and new key files coexist on disk for the 24 h rotation overlap window.
//! `load()` picks the highest-numbered file as the currently-active key. The
//! server-side verifier retains both keys in `roz_device_keys` for the overlap
//! per `migrations/021_*.sql`.
//!
//! # Threat notes
//!
//! - **T-23-28** (other-user key disclosure): `set_mode_0600` before write on
//!   POSIX systems via `OpenOptions::new().mode(0o600)`. Non-POSIX is dev-only.
//! - **T-23-29** (tamper): AES-GCM auth tag; decrypt returns
//!   `KeyProviderError::AeadFailure` → caller hard-stops with exit 78.
//! - **T-23-30** (DoS re-enroll): Enrollment is one-shot at startup;
//!   failures exit immediately, no retry storm.
//! - **T-23-31** (old key replay): Both keys are retained but `load()` always
//!   selects the highest version; the signer only uses the active key.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64_STD;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{SigningKey, VerifyingKey};
use roz_core::auth::TenantId;
use roz_core::key_provider::{KeyProvider, StaticKeyProvider};
use roz_core::signing::{Direction, HEADER_NAME, SignedFields, payload_sha256_hex, sign_envelope};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 90-day rotation interval per D-07.
const ROTATION_INTERVAL: Duration = Duration::days(90);

/// On-disk shape of a device-key file.
#[derive(Debug, Serialize, Deserialize)]
struct EncryptedKeyFile {
    ciphertext_b64: String,
    nonce_b64: String,
    key_version: u32,
    created_at: DateTime<Utc>,
}

/// Material the worker needs for outbound signing and inbound verifying.
///
/// - `signing_key` is used to sign every outbound `worker→server` envelope
///   (plan 23-08 wires this into the publish sites).
/// - `server_verifying_key` is used to verify every inbound `server→worker`
///   envelope the worker receives.
#[derive(Clone)]
pub struct SigningKeyMaterial {
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    pub key_version: u32,
    pub signing_key: SigningKey,
    pub server_verifying_key: VerifyingKey,
    pub created_at: DateTime<Utc>,
}

/// Resolve the on-disk data directory where device keys live.
///
/// See module docs for the resolution order.
#[must_use]
pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ROZ_DATA_DIR") {
        return PathBuf::from(d);
    }
    let prod = PathBuf::from("/etc/roz");
    if prod.is_dir() {
        return prod;
    }
    directories::BaseDirs::new()
        .map(|b| b.config_dir().join("roz"))
        .unwrap_or_else(|| PathBuf::from(".roz"))
}

fn device_key_path(dir: &Path, version: u32) -> PathBuf {
    dir.join(format!("device-key-v{version}.pem"))
}

fn server_verifying_key_path(dir: &Path) -> PathBuf {
    dir.join("server-verifying-key.pem")
}

/// Load the highest-version device key from disk. Returns `Ok(None)` if no
/// device-key file exists — the caller should then enroll via
/// `load_or_enroll`. Any I/O, parse, or decrypt error surfaces as `Err` so
/// the caller can hard-stop (exit 78, D-09).
pub async fn load(
    dir: &Path,
    key_provider: &Arc<StaticKeyProvider>,
    tenant_id: Uuid,
    host_id: Uuid,
) -> Result<Option<SigningKeyMaterial>> {
    if !dir.is_dir() {
        return Ok(None);
    }

    // Scan the directory for `device-key-v{N}.pem` files and pick the
    // highest-numbered one as the active key.
    let mut highest: Option<u32> = None;
    for entry in std::fs::read_dir(dir).context("read data dir")? {
        let entry = entry.context("read data dir entry")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("device-key-v").and_then(|s| s.strip_suffix(".pem")) else {
            continue;
        };
        if let Ok(v) = rest.parse::<u32>() {
            highest = Some(highest.map_or(v, |h| h.max(v)));
        }
    }
    let Some(version) = highest else { return Ok(None) };

    let path = device_key_path(dir, version);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let file: EncryptedKeyFile = serde_json::from_slice(&bytes).context("parse device key file")?;

    let ct = B64_STD
        .decode(file.ciphertext_b64.as_bytes())
        .context("decode ciphertext")?;
    let nonce = B64_STD.decode(file.nonce_b64.as_bytes()).context("decode nonce")?;

    // The server side wraps the seed as URL_SAFE_NO_PAD(seed) inside a
    // SecretString before encryption (see roz-server/src/routes/device.rs
    // around line 367). We mirror that wrapping on the worker so both ends
    // speak the same on-disk format.
    let tenant = TenantId::new(tenant_id);
    let plaintext = key_provider
        .decrypt(&ct, &nonce, &tenant)
        .await
        .context("decrypt device key")?;
    let seed_vec = URL_SAFE_NO_PAD
        .decode(plaintext.expose_secret().as_bytes())
        .context("decode seed plaintext")?;
    let seed: [u8; 32] = seed_vec.as_slice().try_into().context("seed must be 32 bytes")?;

    // Load the server verifying key (plaintext; it's public material).
    let svk_bytes = std::fs::read(server_verifying_key_path(dir)).context("read server verifying key")?;
    let svk_arr: [u8; 32] = svk_bytes
        .as_slice()
        .try_into()
        .context("server verifying key must be 32 bytes")?;
    let server_verifying_key = VerifyingKey::from_bytes(&svk_arr).context("parse server verifying key")?;

    Ok(Some(SigningKeyMaterial {
        tenant_id,
        host_id,
        key_version: file.key_version,
        signing_key: SigningKey::from_bytes(&seed),
        server_verifying_key,
        created_at: file.created_at,
    }))
}

/// Persist a seed + server verifying key to disk.
///
/// The device-key file is written via `OpenOptions` with mode `0o600` on
/// POSIX so the file never exists world-readable even transiently. The
/// plaintext bytes go to a temp file and are atomically renamed into place
/// to avoid leaving a torn file on disk if the process is killed mid-write.
pub async fn save(
    dir: &Path,
    key_provider: &Arc<StaticKeyProvider>,
    tenant_id: Uuid,
    key_version: u32,
    seed: &[u8; 32],
    server_verifying_key: &[u8; 32],
) -> Result<()> {
    std::fs::create_dir_all(dir).context("mkdir data dir")?;

    // Encrypt the seed. Wrap as URL_SAFE_NO_PAD(seed) inside a SecretString
    // to match the server's on-disk/on-DB format — see crate docs above.
    let tenant = TenantId::new(tenant_id);
    let plaintext = SecretString::from(URL_SAFE_NO_PAD.encode(seed));
    let (ciphertext, nonce) = key_provider
        .encrypt(&plaintext, &tenant)
        .await
        .context("encrypt seed")?;

    let file = EncryptedKeyFile {
        ciphertext_b64: B64_STD.encode(&ciphertext),
        nonce_b64: B64_STD.encode(&nonce),
        key_version,
        created_at: Utc::now(),
    };
    let bytes = serde_json::to_vec(&file).context("serialize device key file")?;

    let final_path = device_key_path(dir, key_version);
    atomic_write_mode_0600(&final_path, &bytes).context("write device key")?;

    // Server verifying key is not secret; plain write is fine.
    std::fs::write(server_verifying_key_path(dir), server_verifying_key).context("write server verifying key")?;

    Ok(())
}

/// Atomically write `bytes` to `final_path` with mode `0o600` on POSIX.
///
/// Writes to `{final_path}.tmp-{pid}-{uuid}` first, then renames into place.
/// On Unix the temp file is opened with `O_CREAT | O_WRONLY` and mode
/// `0o600`, so the file never exists world-readable.
fn atomic_write_mode_0600(final_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = final_path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir"))?;
    let tmp_name = format!(
        "{}.tmp-{}-{}",
        final_path.file_name().and_then(|n| n.to_str()).unwrap_or("device-key"),
        std::process::id(),
        Uuid::new_v4().simple()
    );
    let tmp_path = parent.join(tmp_name);

    write_tmp_0600(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, final_path)
}

#[cfg(unix)]
fn write_tmp_0600(tmp_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp_path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_tmp_0600(tmp_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(tmp_path, bytes)
}

/// Bootstrap path: if no key on disk, call `POST /v1/device/provision-key`
/// and persist the returned seed. Otherwise load the existing key from disk.
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

    tracing::info!(
        %host_id,
        "no device key on disk; enrolling via POST /v1/device/provision-key"
    );

    #[derive(Deserialize)]
    struct ProvisionResp {
        private_key_seed_b64: String,
        key_version: u32,
        server_verifying_key_b64: String,
    }

    let base = api_url.trim_end_matches('/');
    let resp: ProvisionResp = http
        .post(format!("{base}/v1/device/provision-key"))
        .bearer_auth(api_key)
        .json(&serde_json::json!({ "host_id": host_id }))
        .send()
        .await
        .context("POST /v1/device/provision-key")?
        .error_for_status()
        .context("provision-key returned error status")?
        .json()
        .await
        .context("parse provision response")?;

    let seed_vec = B64_STD
        .decode(resp.private_key_seed_b64.as_bytes())
        .context("decode private key seed")?;
    let seed: [u8; 32] = seed_vec.as_slice().try_into().context("seed must be 32 bytes")?;

    let svk_vec = B64_STD
        .decode(resp.server_verifying_key_b64.as_bytes())
        .context("decode server verifying key")?;
    let svk: [u8; 32] = svk_vec
        .as_slice()
        .try_into()
        .context("server verifying key must be 32 bytes")?;

    save(dir, key_provider, tenant_id, resp.key_version, &seed, &svk).await?;

    Ok(SigningKeyMaterial {
        tenant_id,
        host_id,
        key_version: resp.key_version,
        signing_key: SigningKey::from_bytes(&seed),
        server_verifying_key: VerifyingKey::from_bytes(&svk).context("parse server verifying key")?,
        created_at: Utc::now(),
    })
}

/// Age-check rotation per D-07.
///
/// If the active key is older than `ROTATION_INTERVAL` (90 days), rotate.
/// Otherwise return `Ok(None)` so the caller keeps using the current key.
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

/// Unconditionally rotate the device key via `POST /v1/device/rotate-key`.
///
/// Builds a `roz-sig-v1` envelope signed with the CURRENT key and attaches
/// it to a signed-body request. On success, persists the new seed + server
/// verifying key to disk under the new version number. Old and new key
/// files coexist on disk for the 24 h overlap window.
pub async fn force_rotate(
    current: &SigningKeyMaterial,
    dir: &Path,
    http: &reqwest::Client,
    api_url: &str,
    key_provider: &Arc<StaticKeyProvider>,
) -> Result<SigningKeyMaterial> {
    // Build the signed-body rotate-key request. The body bytes are the exact
    // bytes the envelope covers — any re-serialization between here and the
    // HTTP send would desync the payload hash.
    let body = serde_json::json!({
        "tenant_id": current.tenant_id,
        "host_id": current.host_id,
        "current_key_version": current.key_version,
    });
    let body_bytes = serde_json::to_vec(&body).context("serialize rotate body")?;

    let fields = SignedFields {
        direction: Direction::WorkerToServer,
        tenant_id: current.tenant_id,
        host_id: current.host_id,
        correlation_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        // Rotate-key uses a one-shot envelope; server does not advance the
        // worker-scoped sequence counter on this path.
        sequence_number: 0,
        payload_hash: payload_sha256_hex(&body_bytes),
        key_version: current.key_version,
    };
    let envelope = sign_envelope(&fields, &current.signing_key).context("sign rotate envelope")?;
    let header = envelope.encode_header().context("encode rotate envelope header")?;

    #[derive(Deserialize)]
    struct RotateResp {
        private_key_seed_b64: String,
        key_version: u32,
        server_verifying_key_b64: String,
    }

    let base = api_url.trim_end_matches('/');
    let resp: RotateResp = http
        .post(format!("{base}/v1/device/rotate-key"))
        .header(HEADER_NAME, header)
        .header("content-type", "application/json")
        .body(body_bytes)
        .send()
        .await
        .context("POST /v1/device/rotate-key")?
        .error_for_status()
        .context("rotate-key returned error status")?
        .json()
        .await
        .context("parse rotate response")?;

    let seed_vec = B64_STD
        .decode(resp.private_key_seed_b64.as_bytes())
        .context("decode rotated seed")?;
    let seed: [u8; 32] = seed_vec.as_slice().try_into().context("seed must be 32 bytes")?;

    let svk_vec = B64_STD
        .decode(resp.server_verifying_key_b64.as_bytes())
        .context("decode server verifying key")?;
    let svk: [u8; 32] = svk_vec
        .as_slice()
        .try_into()
        .context("server verifying key must be 32 bytes")?;

    save(dir, key_provider, current.tenant_id, resp.key_version, &seed, &svk).await?;

    Ok(SigningKeyMaterial {
        tenant_id: current.tenant_id,
        host_id: current.host_id,
        key_version: resp.key_version,
        signing_key: SigningKey::from_bytes(&seed),
        server_verifying_key: VerifyingKey::from_bytes(&svk).context("parse server verifying key")?,
        created_at: Utc::now(),
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_provider() -> Arc<StaticKeyProvider> {
        Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]))
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
        assert_eq!(mat.tenant_id, tenant);
        assert_eq!(mat.host_id, host);
    }

    #[tokio::test]
    async fn load_picks_highest_version() {
        let dir = TempDir::new().unwrap();
        let provider = test_provider();
        let tenant = Uuid::new_v4();
        let host = Uuid::new_v4();
        // Use verifying keys derived from signing keys so the bytes are
        // valid Ed25519 points (arbitrary byte patterns may not be
        // decompressable Edwards points).
        let svk1 = SigningKey::from_bytes(&[1u8; 32]).verifying_key().to_bytes();
        let svk2 = SigningKey::from_bytes(&[3u8; 32]).verifying_key().to_bytes();
        save(dir.path(), &provider, tenant, 1, &[1u8; 32], &svk1).await.unwrap();
        save(dir.path(), &provider, tenant, 2, &[3u8; 32], &svk2).await.unwrap();
        let mat = load(dir.path(), &provider, tenant, host).await.unwrap().unwrap();
        assert_eq!(mat.key_version, 2);
        assert_eq!(mat.signing_key.to_bytes(), [3u8; 32]);
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let provider = test_provider();
        assert!(
            load(dir.path(), &provider, Uuid::new_v4(), Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn load_nonexistent_dir_returns_none() {
        let dir = TempDir::new().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let provider = test_provider();
        assert!(
            load(&nonexistent, &provider, Uuid::new_v4(), Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn load_corrupt_ciphertext_errs() {
        let dir = TempDir::new().unwrap();
        let provider = test_provider();
        let tenant = Uuid::new_v4();
        save(dir.path(), &provider, tenant, 1, &[7u8; 32], &[8u8; 32])
            .await
            .unwrap();
        // Corrupt the file — replace with garbage that is not JSON.
        let path = device_key_path(dir.path(), 1);
        std::fs::write(&path, b"not-json").unwrap();
        assert!(load(dir.path(), &provider, tenant, Uuid::new_v4()).await.is_err());
    }

    #[tokio::test]
    async fn load_tampered_ciphertext_errs() {
        let dir = TempDir::new().unwrap();
        let provider = test_provider();
        let tenant = Uuid::new_v4();
        save(dir.path(), &provider, tenant, 1, &[7u8; 32], &[8u8; 32])
            .await
            .unwrap();
        // Tamper the ciphertext_b64 field without breaking JSON parsing. The
        // AES-GCM auth tag must catch this (T-23-29).
        let path = device_key_path(dir.path(), 1);
        let raw = std::fs::read(&path).unwrap();
        let mut file: EncryptedKeyFile = serde_json::from_slice(&raw).unwrap();
        // Flip a byte in the ciphertext.
        let mut ct = B64_STD.decode(file.ciphertext_b64.as_bytes()).unwrap();
        ct[0] ^= 0xFF;
        file.ciphertext_b64 = B64_STD.encode(&ct);
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(load(dir.path(), &provider, tenant, Uuid::new_v4()).await.is_err());
    }

    #[tokio::test]
    async fn load_missing_server_verifying_key_errs() {
        let dir = TempDir::new().unwrap();
        let provider = test_provider();
        let tenant = Uuid::new_v4();
        save(dir.path(), &provider, tenant, 1, &[7u8; 32], &[8u8; 32])
            .await
            .unwrap();
        // Remove the plaintext server verifying key file.
        std::fs::remove_file(server_verifying_key_path(dir.path())).unwrap();
        assert!(load(dir.path(), &provider, tenant, Uuid::new_v4()).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let provider = test_provider();
        save(dir.path(), &provider, Uuid::new_v4(), 1, &[7u8; 32], &[8u8; 32])
            .await
            .unwrap();
        let perms = std::fs::metadata(device_key_path(dir.path(), 1)).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn signing_round_trip_survives_save_load() {
        // Prove the persisted key can still sign a valid Ed25519 signature
        // after a save→load cycle. Catches any encoding bug in the
        // SecretString wrapping.
        let dir = TempDir::new().unwrap();
        let provider = test_provider();
        let tenant = Uuid::new_v4();
        let host = Uuid::new_v4();
        let seed = [42u8; 32];
        save(dir.path(), &provider, tenant, 1, &seed, &[9u8; 32]).await.unwrap();
        let mat = load(dir.path(), &provider, tenant, host).await.unwrap().unwrap();

        let fields = SignedFields {
            direction: Direction::WorkerToServer,
            tenant_id: tenant,
            host_id: host,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(b"payload"),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &mat.signing_key).unwrap();
        // Verify against the original signing key's verifying key to confirm
        // the seed bytes round-tripped correctly.
        let original = SigningKey::from_bytes(&seed);
        roz_core::signing::verify_envelope(&fields, &env.signature, &original.verifying_key()).unwrap();
    }
}
