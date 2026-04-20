//! Thin wrapper over upstream `mavlink[mav2-message-signing]`.
//!
//! Per Phase 25 D-01' (post-research override) and D-14' / D-20 (post-review):
//! this module does NOT reimplement HMAC-SHA256, truncation, or timestamp
//! arithmetic. The upstream `mavlink::SigningConfig::new(secret_key, link_id,
//! sign_outgoing, allow_unsigned)` and `mavlink::SigningData::from_config(config)`
//! own the crypto primitives. We provide:
//!
//! * [`SigningPosture`] - enum loaded from `roz.toml [mavlink.signing]` (D-03).
//! * [`MavlinkSigningConfig`] - holds the 32-byte seed + posture + link-ID.
//! * [`build_signing_data`] - turns our config into an upstream `SigningData`.
//! * [`build_setup_signing_message`] - emits the MAVLink `SETUP_SIGNING (256)`
//!   message on RF-link bring-up (D-14'). The message's on-wire payload carries
//!   `initial_timestamp` as a `u64` field, populated by [`fresh_initial_timestamp_us`].
//!
//! # D-14' SETUP_SIGNING liveness (not in this module)
//!
//! SETUP_SIGNING is a MAVLink MESSAGE (msg_id 256), not a MAV_CMD. FCUs do
//! NOT reply with COMMAND_ACK. The liveness signal is the first signed
//! HEARTBEAT received from the FCU after SETUP_SIGNING is sent. That timer +
//! degrade-on-timeout logic lives in the backend (plan 25-12), not here.
//!
//! # D-20 Timestamp persistence (DEFERRED to Phase 27)
//!
//! Upstream `mavlink-core 0.17.1` `SigningConfig::new` does NOT take an
//! `initial_timestamp` parameter (verified against the live crate source
//! at `~/.cargo/registry/src/.../mavlink-core-0.17.1/src/signing.rs:47`).
//! `SigningData::from_config` initializes internal state with `timestamp: 0`.
//! The field `SigningState::timestamp` is `pub(crate)` - there is NO external
//! API to seed it. Upstream's `sign_message` / `verify_signature` both rescue
//! state.timestamp to wall-clock before signing/verifying, so the first
//! outgoing frame effectively resets to ~now. Restart-safe replay defense
//! against sub-second worker restart pathology is deferred to Phase 27
//! (which would need either a fork of mavlink-core or a PR upstream).
//! Documented as known limitation in `docs/mavlink-coexistence.md` (plan 25-16).
//!
//! Note: the `SETUP_SIGNING_DATA.initial_timestamp` FIELD on the OUTGOING
//! MAVLink message (msg 256 payload) IS upstream-exposed and is populated
//! here. Don't confuse the message payload field (external) with the
//! SigningData internal state field (private).

use std::time::{SystemTime, UNIX_EPOCH};

use mavlink::common::{MavMessage, SETUP_SIGNING_DATA};
use mavlink::{SigningConfig, SigningData};

/// Per-link signing posture loaded from `roz.toml [mavlink.signing]` per D-03.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SigningPosture {
    /// Signing disabled. Default for serial (USB) per D-03.
    #[default]
    Off,
    /// Signing enabled. Default for UDP (RF) per D-03.
    On,
    /// Auto: `Off` on serial, `On` on UDP. Resolved at `build_signing_data` time.
    Auto,
}

impl SigningPosture {
    /// Resolve `Auto` to a concrete on/off based on transport kind.
    #[must_use]
    pub fn resolve(self, transport: TransportKind) -> bool {
        match self {
            Self::Off => false,
            Self::On => true,
            Self::Auto => matches!(transport, TransportKind::Udp),
        }
    }
}

/// Transport kind — used to resolve `SigningPosture::Auto` per D-03.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// `/dev/ttyUSB0 @ 921600` — USB serial. Default signing posture: `Off`.
    Serial,
    /// UDP `14540`/`14550` — RF-equivalent. Default signing posture: `On`.
    Udp,
}

/// MAVLink v2 signing config for a `MavlinkBackend`.
///
/// Constructed by the worker after decrypting the 32-byte seed from
/// `roz_hosts.mavlink_signing_key_*` columns (D-10 / D-11 / D-12).
/// `seed = None` means pre-migration host (NULL columns) → signing is
/// force-disabled regardless of `posture`, and the backend logs a warning
/// per D-12.
#[derive(Debug, Clone)]
pub struct MavlinkSigningConfig {
    /// 32-byte shared secret. `None` means pre-migration host (D-12).
    pub seed: Option<[u8; 32]>,
    /// Posture loaded from `[mavlink.signing]`. Default: `Auto`.
    pub posture: SigningPosture,
    /// Whether to accept unsigned inbound frames. Default: `false` (strict)
    /// when signing is on; ignored when signing is off.
    pub allow_unsigned: bool,
    /// Our local link_id — copper claims `1` per D-04. Override only for
    /// coexistence tests (plan 25-16 uses `3` for the QGC shim peer).
    pub local_link_id: u8,
}

impl Default for MavlinkSigningConfig {
    fn default() -> Self {
        Self {
            seed: None,
            posture: SigningPosture::Auto,
            allow_unsigned: false,
            local_link_id: 1, // D-04: copper = 1
        }
    }
}

/// Build an upstream `mavlink::SigningData` from our config + transport kind.
///
/// Returns `None` when:
/// * `config.seed` is `None` (pre-migration host per D-12), OR
/// * `config.posture.resolve(transport)` is `false` (signing disabled).
///
/// Returning `None` means the backend MUST NOT call `conn.setup_signing(..)`;
/// unsigned MAVLink traffic flows as-is.
///
/// D-20: `SigningData` is initialized with upstream defaults (internal state
/// `timestamp: 0`). Upstream's sign-path rescues state to wall-clock on the
/// first signed frame, so in practice restart-replay pathology is limited to
/// the ≤1-frame racy window after construction. Phase 27 scopes the WAL-seeded
/// variant.
#[must_use]
pub fn build_signing_data(config: &MavlinkSigningConfig, transport: TransportKind) -> Option<SigningData> {
    let Some(seed) = config.seed else {
        tracing::warn!(
            transport = ?transport,
            "mavlink signing force-disabled: no seed in config (pre-migration host? \
             see 25-CONTEXT.md D-12)"
        );
        return None;
    };
    if !config.posture.resolve(transport) {
        tracing::debug!(transport = ?transport, posture = ?config.posture, "mavlink signing off by config");
        return None;
    }
    let upstream = SigningConfig::new(
        seed,
        config.local_link_id,
        /* sign_outgoing */ true,
        config.allow_unsigned,
    );
    Some(SigningData::from_config(upstream))
}

/// Build an upstream `mavlink::SigningConfig` (NOT `SigningData`) from our
/// config + transport kind.
///
/// Mirror of [`build_signing_data`] but returns the pre-`SigningData` value
/// that `MavConnection::setup_signing` expects per upstream 0.17.1. The
/// transport layer (plan 25-06 `open_transport`) takes `Option<SigningConfig>`
/// because that is the signature upstream exposes; the backend (plan 25-12)
/// owns the conversion to `SigningData` when it needs to verify inbound
/// frames locally.
///
/// Returns `None` for the same two cases as [`build_signing_data`] —
/// pre-migration host (no seed) or posture-off.
#[must_use]
pub fn build_signing_config(config: &MavlinkSigningConfig, transport: TransportKind) -> Option<SigningConfig> {
    let Some(seed) = config.seed else {
        tracing::warn!(
            transport = ?transport,
            "mavlink signing force-disabled: no seed in config (pre-migration host? \
             see 25-CONTEXT.md D-12)"
        );
        return None;
    };
    if !config.posture.resolve(transport) {
        tracing::debug!(transport = ?transport, posture = ?config.posture, "mavlink signing off by config");
        return None;
    }
    Some(SigningConfig::new(
        seed,
        config.local_link_id,
        /* sign_outgoing */ true,
        config.allow_unsigned,
    ))
}

/// MAVLink epoch constant — 2015-01-01 00:00:00 UTC in Unix seconds.
/// [CITED: mavlink.io/en/guide/message_signing.html — timestamp format]
const MAVLINK_EPOCH_UNIX_SECS: u64 = 1_420_070_400;

/// Produce a fresh `initial_timestamp` in MAVLink's 10-µs-since-2015 units
/// for the `SETUP_SIGNING_DATA.initial_timestamp` payload field.
///
/// This is the ON-WIRE payload value shipped in msg 256 to the FCU. It is
/// NOT used to seed upstream `SigningData`'s internal timestamp state —
/// upstream does not expose that (see module doc-comment / D-20).
#[must_use]
pub fn fresh_initial_timestamp_us() -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    // 10-µs units since 2015-01-01 UTC:
    let secs_since_mavlink_epoch = now.as_secs().saturating_sub(MAVLINK_EPOCH_UNIX_SECS);
    secs_since_mavlink_epoch
        .saturating_mul(100_000) // 10-µs ticks per second
        .saturating_add(u64::from(now.subsec_nanos()) / 10_000)
}

/// Build the MAVLink `SETUP_SIGNING (msg 256)` message the backend sends
/// to the FCU on RF-link bring-up per D-14'.
///
/// Caller is responsible for actually transmitting this message via the
/// already-open `MavConnection` and for starting the signed-HEARTBEAT-receipt
/// liveness timer (5 s, D-14' degrade-on-timeout lives in plan 25-12).
///
/// Returns `None` when `config.seed` is `None` (nothing to share).
#[must_use]
pub fn build_setup_signing_message(
    config: &MavlinkSigningConfig,
    target_system: u8,
    target_component: u8,
) -> Option<MavMessage> {
    let seed = config.seed?;
    Some(MavMessage::SETUP_SIGNING(SETUP_SIGNING_DATA {
        target_system,
        target_component,
        secret_key: seed,
        initial_timestamp: fresh_initial_timestamp_us(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posture_auto_resolves_by_transport() {
        assert!(!SigningPosture::Auto.resolve(TransportKind::Serial));
        assert!(SigningPosture::Auto.resolve(TransportKind::Udp));
    }

    #[test]
    fn posture_explicit_overrides_transport() {
        assert!(!SigningPosture::Off.resolve(TransportKind::Udp));
        assert!(SigningPosture::On.resolve(TransportKind::Serial));
    }

    #[test]
    fn build_signing_data_returns_none_for_no_seed() {
        let cfg = MavlinkSigningConfig::default();
        assert!(build_signing_data(&cfg, TransportKind::Udp).is_none());
    }

    #[test]
    fn build_signing_data_returns_none_for_off_posture() {
        let cfg = MavlinkSigningConfig {
            seed: Some([0u8; 32]),
            posture: SigningPosture::Off,
            ..Default::default()
        };
        assert!(build_signing_data(&cfg, TransportKind::Udp).is_none());
    }

    #[test]
    fn build_signing_data_returns_some_for_on_posture_with_seed() {
        let cfg = MavlinkSigningConfig {
            seed: Some([0xABu8; 32]),
            posture: SigningPosture::On,
            ..Default::default()
        };
        assert!(build_signing_data(&cfg, TransportKind::Serial).is_some());
    }

    #[test]
    fn fresh_initial_timestamp_monotonic_between_calls() {
        let t0 = fresh_initial_timestamp_us();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t1 = fresh_initial_timestamp_us();
        assert!(t1 > t0, "t0={t0} t1={t1}");
    }

    #[test]
    fn setup_signing_message_includes_seed_and_timestamp() {
        let cfg = MavlinkSigningConfig {
            seed: Some([0xCDu8; 32]),
            ..Default::default()
        };
        let msg = build_setup_signing_message(&cfg, /* target_system */ 1, /* target_component */ 1)
            .expect("seed present → Some");
        match msg {
            MavMessage::SETUP_SIGNING(data) => {
                assert_eq!(data.secret_key, [0xCDu8; 32]);
                assert_eq!(data.target_system, 1);
                assert_eq!(data.target_component, 1);
                assert!(data.initial_timestamp > 0);
            }
            _ => panic!("expected SETUP_SIGNING variant"),
        }
    }
}
