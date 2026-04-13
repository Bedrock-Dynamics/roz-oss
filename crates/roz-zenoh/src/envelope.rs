//! Ed25519-signed outer envelope for `SessionEvent`s over Zenoh (D-22).
//!
//! Edge state bus summaries, pose, barriers, and heartbeats are unsigned
//! (trusted-LAN posture). Only `SessionEvent` envelopes between colocated
//! agents are signed because they cross the agent-loop trust boundary.
//!
//! Key distribution is peer-to-peer via Zenoh liveliness-token presence plus
//! a parallel identity `Queryable` on `roz/peers/<robot_id>/identity` (see
//! `crate::session::ZenohSessionTransport` — C-02). Verification refuses
//! unknown pubkeys — no trust-on-first-use.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use parking_lot::RwLock;
use roz_core::session::event::EventEnvelope;
use serde::{Deserialize, Serialize};

/// Outer wrapper for `SessionEventEnvelope` published over Zenoh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedSessionEnvelope {
    /// 32-byte Ed25519 verifying-key, hex-encoded. Lookup key into peer cache.
    pub signer_pubkey_hex: String,
    /// Canonical JSON-serialized inner `EventEnvelope` (bytes are what's signed).
    pub envelope_bytes: Vec<u8>,
    /// 64-byte Ed25519 signature, hex-encoded.
    pub signature_hex: String,
}

/// Payload replied by the per-peer identity queryable for pubkey bootstrap (C-02).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerAnnouncement {
    pub robot_id: String,
    /// Short stable identifier: `hex(verifying_key.to_bytes()[..8])`.
    pub device_id: String,
    /// Full 32-byte pubkey, hex-encoded (64 chars).
    pub verifying_key_hex: String,
    pub announced_at: DateTime<Utc>,
}

/// Forward-compat hook for peer pubkey authentication (C-06).
///
/// A future phase can swap this for a server-signed verifier. Default
/// [`AcceptAnyVerifier`] implements the trusted-LAN posture (CONTEXT D-22,
/// residual risk T-06).
pub trait PeerKeyVerifier: Send + Sync + 'static {
    /// Return `Ok(())` to accept the announcement and insert into cache,
    /// `Err(reason)` to reject and log a warning.
    ///
    /// # Errors
    /// Implementation-defined.
    fn verify(&self, announcement: &PeerAnnouncement) -> anyhow::Result<()>;
}

/// Default verifier: accepts all announcements (trusted-LAN posture, T-06 residual risk).
#[derive(Debug, Default, Clone, Copy)]
pub struct AcceptAnyVerifier;

impl PeerKeyVerifier for AcceptAnyVerifier {
    fn verify(&self, _announcement: &PeerAnnouncement) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Peer verifying-key cache keyed by `verifying_key_hex`, with a parallel
/// `robot_id -> pubkey_hex` map for liveliness-Delete eviction (C-02).
#[derive(Clone)]
pub struct PeerKeyCache {
    inner: Arc<RwLock<HashMap<String, VerifyingKey>>>,
    robot_to_hex: Arc<RwLock<HashMap<String, String>>>,
    verifier: Arc<dyn PeerKeyVerifier>,
}

impl std::fmt::Debug for PeerKeyCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerKeyCache")
            .field("len", &self.inner.read().len())
            .finish_non_exhaustive()
    }
}

impl Default for PeerKeyCache {
    fn default() -> Self {
        Self::new_with_verifier(Arc::new(AcceptAnyVerifier))
    }
}

impl PeerKeyCache {
    /// Default: accept-any verifier (trusted-LAN posture per D-22, T-06 residual risk).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with a custom verifier.
    ///
    /// Future phases use this to swap in signed-bootstrap verification.
    #[must_use]
    pub fn new_with_verifier(verifier: Arc<dyn PeerKeyVerifier>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            robot_to_hex: Arc::new(RwLock::new(HashMap::new())),
            verifier,
        }
    }

    /// Insert via announcement (runs verifier hook first).
    ///
    /// Returns `Ok(())` on accept + insert, `Err(reason)` on reject.
    ///
    /// # Errors
    /// Returns verifier rejection or pubkey parse failure.
    pub fn insert_from_announcement(&self, announcement: &PeerAnnouncement) -> anyhow::Result<()> {
        self.verifier.verify(announcement)?;
        let vk_bytes =
            hex::decode(&announcement.verifying_key_hex).map_err(|e| anyhow::anyhow!("pubkey hex decode: {e}"))?;
        let vk_arr: [u8; 32] = vk_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("pubkey must be 32 bytes"))?;
        let vk = VerifyingKey::from_bytes(&vk_arr).map_err(|e| anyhow::anyhow!("pubkey parse: {e}"))?;
        self.inner.write().insert(announcement.verifying_key_hex.clone(), vk);
        self.robot_to_hex
            .write()
            .insert(announcement.robot_id.clone(), announcement.verifying_key_hex.clone());
        Ok(())
    }

    /// Direct insert (used by tests and seeding local pubkey).
    pub fn insert(&self, pubkey_hex: String, key: VerifyingKey) {
        self.inner.write().insert(pubkey_hex, key);
    }

    #[must_use]
    pub fn get(&self, pubkey_hex: &str) -> Option<VerifyingKey> {
        self.inner.read().get(pubkey_hex).copied()
    }

    pub fn evict(&self, pubkey_hex: &str) -> Option<VerifyingKey> {
        self.inner.write().remove(pubkey_hex)
    }

    /// Evict by `robot_id` (used on liveliness-Delete; C-02).
    pub fn evict_by_robot_id(&self, robot_id: &str) -> Option<VerifyingKey> {
        let hex = self.robot_to_hex.write().remove(robot_id)?;
        self.inner.write().remove(&hex)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

/// Load a signing key from either a `base64:<seed>` string or a file path
/// containing raw 32-byte or base64-encoded seed.
///
/// Accepts `ROZ_DEVICE_SIGNING_KEY` value in any of:
/// - `base64:AAAABBBB...` (44 chars of standard base64 after prefix)
/// - `/path/to/file` (file content: raw 32 bytes OR base64 text)
///
/// # Errors
/// Returns an error describing which form was attempted and why it failed.
pub fn load_signing_key(value: &str) -> anyhow::Result<SigningKey> {
    if let Some(rest) = value.strip_prefix("base64:") {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(rest.trim())
            .map_err(|e| anyhow::anyhow!("ROZ_DEVICE_SIGNING_KEY base64 decode failed: {e}"))?;
        let seed: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("ROZ_DEVICE_SIGNING_KEY must decode to 32 bytes, got {}", bytes.len()))?;
        return Ok(SigningKey::from_bytes(&seed));
    }

    // Treat as file path.
    let path = Path::new(value);
    let content = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("ROZ_DEVICE_SIGNING_KEY read {} failed: {e}", path.display()))?;

    // Accept raw 32 bytes OR base64 text (with or without trailing whitespace).
    if content.len() == 32 {
        let seed: [u8; 32] = content
            .as_slice()
            .try_into()
            .expect("length check above guarantees 32 bytes");
        return Ok(SigningKey::from_bytes(&seed));
    }
    let text = std::str::from_utf8(&content).map_err(|e| {
        anyhow::anyhow!(
            "ROZ_DEVICE_SIGNING_KEY file at {} is neither 32 raw bytes nor UTF-8 base64: {e}",
            path.display()
        )
    })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|e| anyhow::anyhow!("ROZ_DEVICE_SIGNING_KEY base64 decode of file content failed: {e}"))?;
    let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "ROZ_DEVICE_SIGNING_KEY file decoded to {} bytes, expected 32",
            bytes.len()
        )
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Compute the short stable `device_id` from a [`VerifyingKey`].
#[must_use]
pub fn device_id_of(key: &VerifyingKey) -> String {
    hex::encode(&key.to_bytes()[..8])
}

/// Sign an [`EventEnvelope`] into a [`SignedSessionEnvelope`].
///
/// # Errors
/// Returns JSON serialization failure.
pub fn sign_envelope(signing_key: &SigningKey, envelope: &EventEnvelope) -> anyhow::Result<SignedSessionEnvelope> {
    let bytes = serde_json::to_vec(envelope)?;
    let signature: Signature = signing_key.sign(&bytes);
    Ok(SignedSessionEnvelope {
        signer_pubkey_hex: hex::encode(signing_key.verifying_key().to_bytes()),
        envelope_bytes: bytes,
        signature_hex: hex::encode(signature.to_bytes()),
    })
}

/// Verify a [`SignedSessionEnvelope`] against a peer key cache.
///
/// # Errors
/// Returns:
/// - "unknown peer pubkey" if `signer_pubkey_hex` is not in the cache (no TOFU).
/// - "signature verification failed" if signature doesn't match `envelope_bytes`.
/// - "signature hex decode failed" or "envelope JSON decode failed" for format errors.
pub fn verify_envelope(cache: &PeerKeyCache, signed: &SignedSessionEnvelope) -> anyhow::Result<EventEnvelope> {
    let verifying_key = cache
        .get(&signed.signer_pubkey_hex)
        .ok_or_else(|| anyhow::anyhow!("unknown peer pubkey {}", signed.signer_pubkey_hex))?;
    let sig_bytes: [u8; 64] = hex::decode(&signed.signature_hex)
        .map_err(|e| anyhow::anyhow!("signature hex decode failed: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature hex must decode to 64 bytes"))?;
    let signature = Signature::from_bytes(&sig_bytes);
    verifying_key
        .verify(&signed.envelope_bytes, &signature)
        .map_err(|e| anyhow::anyhow!("signature verification failed: {e}"))?;
    serde_json::from_slice::<EventEnvelope>(&signed.envelope_bytes)
        .map_err(|e| anyhow::anyhow!("envelope JSON decode failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn fixture_envelope() -> EventEnvelope {
        // CANONICAL SHARED FIXTURE — keep byte-identical to plan 15-04 Task 1/3
        // and plan 15-08 Task 2. Any drift breaks the D-18 wire-format lock.
        use chrono::DateTime;
        use roz_core::session::event::{CorrelationId, EventId, SessionEvent};
        EventEnvelope {
            event_id: EventId("evt-15-fixture".into()),
            correlation_id: CorrelationId("corr-15-fixture".into()),
            parent_event_id: None,
            timestamp: DateTime::from_timestamp(1_767_225_600, 0).unwrap(), // 2026-01-01T00:00:00Z
            event: SessionEvent::TurnStarted { turn_index: 7 },
        }
    }

    #[test]
    fn load_base64_form() {
        let key = SigningKey::generate(&mut OsRng);
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.to_bytes());
        let loaded = load_signing_key(&format!("base64:{b64}")).unwrap();
        assert_eq!(loaded.verifying_key().to_bytes(), key.verifying_key().to_bytes());
    }

    #[test]
    fn load_raw_file_form() {
        let key = SigningKey::generate(&mut OsRng);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        std::fs::write(&path, key.to_bytes()).unwrap();
        let loaded = load_signing_key(path.to_str().unwrap()).unwrap();
        assert_eq!(loaded.verifying_key().to_bytes(), key.verifying_key().to_bytes());
    }

    #[test]
    fn load_rejects_wrong_length() {
        let err = load_signing_key("base64:AAAA").unwrap_err();
        assert!(err.to_string().contains("32 bytes"));
    }

    #[test]
    fn sign_verify_roundtrip() {
        let key = SigningKey::generate(&mut OsRng);
        let cache = PeerKeyCache::new();
        let pubkey_hex = hex::encode(key.verifying_key().to_bytes());
        cache.insert(pubkey_hex.clone(), key.verifying_key());

        let env = fixture_envelope();
        let signed = sign_envelope(&key, &env).unwrap();
        assert_eq!(signed.signer_pubkey_hex, pubkey_hex);
        let decoded = verify_envelope(&cache, &signed).unwrap();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::to_value(&env).unwrap()
        );
    }

    #[test]
    fn verify_rejects_unknown_pubkey() {
        let key = SigningKey::generate(&mut OsRng);
        let cache = PeerKeyCache::new(); // empty
        let signed = sign_envelope(&key, &fixture_envelope()).unwrap();
        let err = verify_envelope(&cache, &signed).unwrap_err();
        assert!(err.to_string().contains("unknown peer pubkey"));
    }

    #[test]
    fn verify_rejects_tampered_signature() {
        let key = SigningKey::generate(&mut OsRng);
        let cache = PeerKeyCache::new();
        cache.insert(hex::encode(key.verifying_key().to_bytes()), key.verifying_key());

        let mut signed = sign_envelope(&key, &fixture_envelope()).unwrap();
        // flip one byte of the signature hex
        signed.signature_hex.replace_range(0..2, "00");
        let err = verify_envelope(&cache, &signed).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("signature"));
    }

    #[test]
    fn peer_key_cache_insert_get_evict() {
        let key = SigningKey::generate(&mut OsRng);
        let hex = hex::encode(key.verifying_key().to_bytes());
        let cache = PeerKeyCache::new();
        cache.insert(hex.clone(), key.verifying_key());
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&hex).is_some());
        cache.evict(&hex);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn peer_key_cache_verifier_hook_rejects() {
        // C-06: PeerKeyCache uses injected PeerKeyVerifier; a rejecting verifier
        // blocks insert_from_announcement.
        struct RejectAll;
        impl PeerKeyVerifier for RejectAll {
            fn verify(&self, _: &PeerAnnouncement) -> anyhow::Result<()> {
                anyhow::bail!("rejected for test")
            }
        }
        let key = SigningKey::generate(&mut OsRng);
        let ann = PeerAnnouncement {
            robot_id: "r1".into(),
            device_id: device_id_of(&key.verifying_key()),
            verifying_key_hex: hex::encode(key.verifying_key().to_bytes()),
            announced_at: Utc::now(),
        };
        let cache = PeerKeyCache::new_with_verifier(Arc::new(RejectAll));
        let err = cache.insert_from_announcement(&ann).unwrap_err();
        assert!(err.to_string().contains("rejected"));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn peer_key_cache_evict_by_robot_id() {
        // C-02: liveliness Delete eviction path.
        let key = SigningKey::generate(&mut OsRng);
        let ann = PeerAnnouncement {
            robot_id: "r-evict".into(),
            device_id: device_id_of(&key.verifying_key()),
            verifying_key_hex: hex::encode(key.verifying_key().to_bytes()),
            announced_at: Utc::now(),
        };
        let cache = PeerKeyCache::new();
        cache.insert_from_announcement(&ann).unwrap();
        assert_eq!(cache.len(), 1);
        cache.evict_by_robot_id("r-evict");
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn peer_announcement_serde_roundtrip() {
        let pa = PeerAnnouncement {
            robot_id: "r1".into(),
            device_id: "deadbeef".into(),
            verifying_key_hex: "00".repeat(32),
            announced_at: Utc::now(),
        };
        let json = serde_json::to_string(&pa).unwrap();
        let back: PeerAnnouncement = serde_json::from_str(&json).unwrap();
        assert_eq!(back.robot_id, pa.robot_id);
        assert_eq!(back.device_id, pa.device_id);
        assert_eq!(back.verifying_key_hex, pa.verifying_key_hex);
    }
}
