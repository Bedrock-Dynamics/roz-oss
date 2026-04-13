//! Zenoh session lifecycle: load config (caller-driven path), open session,
//! expose a cheap-to-clone [`zenoh::Session`] to subsystems.
//!
//! Design (D-01..D-04):
//! - Explicit `path` argument -> [`zenoh::Config::from_file`]
//! - `None` path             -> [`zenoh::Config::from_json5`] with an in-code default
//! - [`zenoh::Session`] is internally `Arc`-based; clone freely across tasks.
//!
//! **Config ownership (C-07):** This module does NOT read env vars. Callers
//! (e.g. `WorkerConfig::load`) own env resolution and pass the resolved path
//! through to [`load_zenoh_config`]. Keeps config in one place and makes tests
//! trivial.

use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use ed25519_dalek::{SigningKey, VerifyingKey};
use roz_core::session::event::EventEnvelope;
use roz_core::transport::SessionTransport;
use zenoh::Session;
use zenoh::liveliness::LivelinessToken;

use crate::envelope::{
    PeerAnnouncement, PeerKeyCache, SignedSessionEnvelope, device_id_of, sign_envelope, verify_envelope,
};

/// Default in-code peer config used when no path is supplied.
/// Peer mode with multicast scout enabled.
// Zenoh 1.8 `autoconnect` expects a list of whatami variants ("router",
// "peer", "client"), not a string alternation. An empty list means "do not
// autoconnect" for that side. Peers autoconnect to routers and other peers;
// routers do not autoconnect (pure peer deployment).
const DEFAULT_PEER_JSON5: &str = r#"{
  mode: "peer",
  scouting: {
    multicast: {
      enabled: true,
      address: "224.0.0.224:7446",
      interface: "auto",
      autoconnect: { router: [], peer: ["router", "peer"] },
    },
    gossip: { enabled: true },
  },
  listen: { endpoints: [] },
  connect: { endpoints: [] },
}"#;

/// Resolve a [`zenoh::Config`] from an explicit path or the in-code default.
///
/// # Errors
/// Returns the file-load or parse error from zenoh, wrapped with the source
/// path (or the default-config marker) for diagnostics.
pub fn load_zenoh_config(path: Option<&std::path::Path>) -> anyhow::Result<zenoh::Config> {
    path.map_or_else(
        || {
            zenoh::Config::from_json5(DEFAULT_PEER_JSON5)
                .map_err(|e| anyhow::anyhow!("default zenoh config invalid: {e}"))
        },
        |p| {
            zenoh::Config::from_file(p)
                .map_err(|e| anyhow::anyhow!("ROZ_ZENOH_CONFIG={} parse failed: {e}", p.display()))
        },
    )
}

/// Open a zenoh session using an optional config file path (`None` = in-code default).
///
/// # Errors
/// Returns config-load failure or [`zenoh::open`] failure.
pub async fn open_session(config_path: Option<&std::path::Path>) -> anyhow::Result<zenoh::Session> {
    let cfg = load_zenoh_config(config_path).context("load zenoh config")?;
    let session = zenoh::open(cfg)
        .await
        .map_err(|e| anyhow::anyhow!("zenoh::open failed: {e}"))?;
    Ok(session)
}

// ============================================================================
// ZenohSessionTransport (plan 15-05): signed SessionEvent publish + verified
// subscribe over Zenoh. Implements the C-01-narrowed `SessionTransport` trait
// from `roz_core::transport`.
// ============================================================================

/// Zenoh implementation of [`SessionTransport`] with Ed25519 signing (D-22).
///
/// Holds the `zenoh::Session`, signing key, peer pubkey cache, and keeps alive:
/// - the liveliness token on `roz/peers/<robot_id>` (presence advertisement)
/// - the identity `Queryable` task on `roz/peers/<robot_id>/identity` (C-02)
/// - the liveliness subscriber task on `roz/peers/*` (peer discovery)
///
/// Dropping this struct signals "left" on the liveliness channel.
pub struct ZenohSessionTransport {
    session: Session,
    signing_key: Arc<SigningKey>,
    #[expect(
        dead_code,
        reason = "Retained for diagnostics/future signed announcements; reachable via signing_key."
    )]
    verifying_key: VerifyingKey,
    #[expect(
        dead_code,
        reason = "Retained for tracing/diagnostics; may be consumed by future plans."
    )]
    robot_id: String,
    peer_keys: PeerKeyCache,
    _liveliness_token: LivelinessToken,
    _liveliness_sub_task: tokio::task::JoinHandle<()>,
    _identity_queryable_task: tokio::task::JoinHandle<()>,
    _bootstrap_task: tokio::task::JoinHandle<()>,
}

/// Handle for a session-event subscription. The returned receiver yields
/// already-verified `EventEnvelope`s. Dropping the handle stops the fanout task.
///
/// The `rx` field is public (C-11): integration tests in plan 15-08 and peer
/// relay code in roz-worker consume it directly. No `Box<dyn Trait>` erasure here
/// because the `SessionTransport` trait no longer carries subscribe (C-01 narrow).
pub struct ZenohSubscription {
    pub rx: tokio::sync::mpsc::Receiver<EventEnvelope>,
    _task: tokio::task::JoinHandle<()>,
}

/// C-02 helper: given the bytes of a `PeerAnnouncement` reply, run the
/// `PeerKeyVerifier` hook and insert into cache.
fn insert_peer_from_announcement_bytes(cache: &PeerKeyCache, bytes: &[u8]) {
    let announcement: PeerAnnouncement = match serde_json::from_slice(bytes) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "PeerAnnouncement JSON decode failed");
            return;
        }
    };
    if let Err(e) = cache.insert_from_announcement(&announcement) {
        tracing::warn!(
            robot_id = %announcement.robot_id,
            error = %e,
            "PeerKeyVerifier rejected announcement; dropping",
        );
    }
}

impl ZenohSessionTransport {
    /// Open the transport: declare the identity queryable + liveliness token,
    /// seed the peer cache from already-alive peers (late-joiner), and spawn
    /// the liveliness subscriber that keeps the cache fresh.
    ///
    /// # Errors
    /// Returns zenoh declare failures or liveliness errors.
    pub async fn open(session: Session, signing_key: Arc<SigningKey>, robot_id: String) -> anyhow::Result<Self> {
        let verifying_key = signing_key.verifying_key();
        let peer_keys = PeerKeyCache::new();

        // C-02: Zenoh liveliness tokens carry NO payload. Identity bootstrap
        // uses a SEPARATE Queryable on roz/peers/<robot_id>/identity.
        let announcement = PeerAnnouncement {
            robot_id: robot_id.clone(),
            device_id: device_id_of(&verifying_key),
            verifying_key_hex: hex::encode(verifying_key.to_bytes()),
            announced_at: chrono::Utc::now(),
        };
        let ann_bytes: Vec<u8> = serde_json::to_vec(&announcement)?;

        // Step 1: Declare identity queryable that answers with our PeerAnnouncement.
        let identity_key = format!("roz/peers/{robot_id}/identity");
        let identity_queryable = session
            .declare_queryable(&identity_key)
            .await
            .map_err(|e| anyhow::anyhow!("declare_queryable({identity_key}) failed: {e}"))?;
        let ann_bytes_for_q = ann_bytes.clone();
        let identity_queryable_task = tokio::spawn(async move {
            loop {
                match identity_queryable.recv_async().await {
                    Ok(query) => {
                        let reply_ke = query.key_expr().clone();
                        if let Err(e) = query.reply(reply_ke, ann_bytes_for_q.clone()).await {
                            tracing::warn!(error = %e, "identity queryable reply failed");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "identity queryable terminated");
                        break;
                    }
                }
            }
        });

        // Step 2: Seed cache from live peers (late-joiner pattern).
        // liveliness().get() returns a Receiver of Replies by key-expr.
        // For each alive peer, fire session.get() on their identity queryable.
        let alive_peers = session
            .liveliness()
            .get("roz/peers/*")
            .await
            .map_err(|e| anyhow::anyhow!("liveliness().get failed: {e}"))?;
        let session_for_bootstrap = session.clone();
        let peer_keys_bootstrap = peer_keys.clone();
        let bootstrap_task = tokio::spawn(async move {
            while let Ok(reply) = alive_peers.recv_async().await {
                let Ok(sample) = reply.result() else { continue };
                let peer_ke = sample.key_expr().as_str().to_string();
                // Extract robot_id from "roz/peers/<robot_id>".
                let Some(peer_robot_id) = peer_ke.strip_prefix("roz/peers/").map(str::to_string) else {
                    continue;
                };
                // Skip entries that already include the /identity suffix (shouldn't happen
                // on liveliness queries, but be defensive).
                if peer_robot_id.contains('/') {
                    continue;
                }
                // Fetch the identity of this peer.
                let identity_ke = format!("roz/peers/{peer_robot_id}/identity");
                match session_for_bootstrap.get(&identity_ke).await {
                    Ok(replies) => {
                        if let Ok(id_reply) = replies.recv_async().await
                            && let Ok(id_sample) = id_reply.result()
                        {
                            let payload = id_sample.payload().to_bytes();
                            insert_peer_from_announcement_bytes(&peer_keys_bootstrap, &payload);
                        }
                    }
                    Err(e) => tracing::warn!(peer = %peer_robot_id, error = %e, "identity query failed"),
                }
            }
        });

        // Step 3: Subscribe to future liveliness changes; on Put fetch identity; on Delete evict.
        let liveliness_sub = session
            .liveliness()
            .declare_subscriber("roz/peers/*")
            .with(flume::bounded::<zenoh::sample::Sample>(32))
            .await
            .map_err(|e| anyhow::anyhow!("liveliness declare_subscriber failed: {e}"))?;
        let peer_keys_for_sub = peer_keys.clone();
        let session_for_sub = session.clone();
        let sub_task = tokio::spawn(async move {
            loop {
                match liveliness_sub.recv_async().await {
                    Ok(sample) => {
                        let key_expr = sample.key_expr().as_str().to_string();
                        let Some(peer_robot_id) = key_expr.strip_prefix("roz/peers/").map(str::to_string) else {
                            continue;
                        };
                        if peer_robot_id.contains('/') {
                            continue;
                        }
                        match sample.kind() {
                            zenoh::sample::SampleKind::Put => {
                                // C-02: fetch identity via queryable.
                                let identity_ke = format!("roz/peers/{peer_robot_id}/identity");
                                match session_for_sub.get(&identity_ke).await {
                                    Ok(replies) => {
                                        if let Ok(id_reply) = replies.recv_async().await
                                            && let Ok(id_sample) = id_reply.result()
                                        {
                                            let payload = id_sample.payload().to_bytes();
                                            insert_peer_from_announcement_bytes(&peer_keys_for_sub, &payload);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(peer = %peer_robot_id, error = %e, "identity query failed");
                                    }
                                }
                            }
                            zenoh::sample::SampleKind::Delete => {
                                peer_keys_for_sub.evict_by_robot_id(&peer_robot_id);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "liveliness subscriber terminated");
                        break;
                    }
                }
            }
        });

        // Step 4: Declare the presence liveliness token (no payload — pure presence).
        let peer_key = format!("roz/peers/{robot_id}");
        let token = session
            .liveliness()
            .declare_token(&peer_key)
            .await
            .map_err(|e| anyhow::anyhow!("liveliness declare_token failed: {e}"))?;

        // Seed our own pubkey into cache so loopback publish/subscribe verifies.
        peer_keys.insert(hex::encode(verifying_key.to_bytes()), verifying_key);

        tracing::info!(
            robot_id = %robot_id,
            device_id = %device_id_of(&verifying_key),
            "zenoh session transport ready (signed)",
        );

        Ok(Self {
            session,
            signing_key,
            verifying_key,
            robot_id,
            peer_keys,
            _liveliness_token: token,
            _liveliness_sub_task: sub_task,
            _identity_queryable_task: identity_queryable_task,
            _bootstrap_task: bootstrap_task,
        })
    }

    fn session_key(team_id: &str, session_id: &str) -> String {
        format!("roz/sessions/{team_id}/{session_id}")
    }

    /// Extract `team_id` + `session_id` routing from an envelope.
    ///
    /// C-01 narrow-scope boundary: the trait accepts only `&EventEnvelope`, so
    /// routing must be derivable from it. Current convention: `correlation_id`
    /// carries `session_id`; `team_id` defaults to `"default"`. A future plan
    /// may extend `EventEnvelope` with explicit routing fields.
    fn envelope_routing(envelope: &EventEnvelope) -> (String, String) {
        let session_id = envelope.correlation_id.0.clone();
        ("default".to_string(), session_id)
    }

    /// Expose the underlying `zenoh::Session` for subsystems that share the
    /// transport's Session (plan 15-06 health monitors reuse it).
    #[must_use]
    pub fn session(&self) -> zenoh::Session {
        self.session.clone()
    }

    /// C-11: `subscribe_session_raw` is defined here in 15-05 (NOT retroactively in 15-08).
    ///
    /// Returns a concrete [`ZenohSubscription`] with `pub rx: Receiver<EventEnvelope>`.
    /// Signature verification happens BEFORE the inner `EventEnvelope` reaches
    /// the receiver; unknown pubkeys or bad signatures are dropped with `warn!`.
    ///
    /// # Errors
    /// Subscriber declare failure.
    pub async fn subscribe_session_raw(
        &self,
        _tenant_id: &str,
        team_id: &str,
        session_id: &str,
    ) -> anyhow::Result<ZenohSubscription> {
        let key = Self::session_key(team_id, session_id);
        let sub = self
            .session
            .declare_subscriber(&key)
            .with(flume::bounded::<zenoh::sample::Sample>(64))
            .await
            .map_err(|e| anyhow::anyhow!("zenoh session subscribe failed: {e}"))?;
        let (tx, rx) = tokio::sync::mpsc::channel::<EventEnvelope>(64);
        let peer_keys = self.peer_keys.clone();
        let task = tokio::spawn(async move {
            loop {
                match sub.recv_async().await {
                    Ok(sample) => {
                        let bytes = sample.payload().to_bytes();
                        let signed: SignedSessionEnvelope = match serde_json::from_slice(&bytes) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(error = %e, "SignedSessionEnvelope decode failed");
                                continue;
                            }
                        };
                        match verify_envelope(&peer_keys, &signed) {
                            Ok(inner) => {
                                if tx.send(inner).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => tracing::warn!(
                                error = %e,
                                signer = %signed.signer_pubkey_hex,
                                "session envelope verification failed; dropping",
                            ),
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "zenoh session subscriber terminated");
                        break;
                    }
                }
            }
        });
        Ok(ZenohSubscription { rx, _task: task })
    }
}

/// C-01 narrow impl: single method (`publish_event_envelope`). No subscribe on the trait.
#[async_trait]
impl SessionTransport for ZenohSessionTransport {
    async fn publish_event_envelope(&self, envelope: &EventEnvelope) -> anyhow::Result<()> {
        let (team_id, session_id) = Self::envelope_routing(envelope);
        let signed = sign_envelope(&self.signing_key, envelope)?;
        let key = Self::session_key(&team_id, &session_id);
        // D-10 key-expr literal: roz/sessions/{team_id}/{session_id}
        let payload = serde_json::to_vec(&signed)?;
        self.session
            .put(&key, payload)
            .await
            .map_err(|e| anyhow::anyhow!("zenoh session put failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // C-09 fix: no env::set_var/remove_var — Rust 2024 marks those unsafe and
    // the workspace lint `unsafe_code = "deny"` rejects that even in tests.
    // Tests pass explicit paths to `load_zenoh_config(Option<&Path>)`.

    #[test]
    fn default_config_valid_when_no_path() {
        let cfg = load_zenoh_config(None).expect("default config must be valid");
        drop(cfg);
    }

    #[test]
    fn missing_path_returns_error_mentioning_path() {
        let path = std::path::Path::new("/definitely/does/not/exist.json5");
        let err = load_zenoh_config(Some(path)).unwrap_err();
        assert!(err.to_string().contains("/definitely/does/not/exist.json5"));
    }

    #[test]
    fn valid_file_loads_from_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zenoh.json5");
        std::fs::write(&path, r#"{ mode: "peer" }"#).unwrap();
        let cfg = load_zenoh_config(Some(&path)).expect("valid file loads");
        drop(cfg);
    }

    #[test]
    fn invalid_file_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json5");
        std::fs::write(&path, "this is not json5 {{{").unwrap();
        let err = load_zenoh_config(Some(&path)).unwrap_err();
        // Accept any error string — zenoh's parse error message wording varies.
        let _ = err.to_string();
    }
}
