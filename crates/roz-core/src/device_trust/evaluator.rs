use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{DeviceTrust, TrustPosture};

// ---------------------------------------------------------------------------
// TrustPolicy
// ---------------------------------------------------------------------------

/// Policy governing how a device's trust posture is evaluated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustPolicy {
    pub max_attestation_age_secs: u64,
    pub require_firmware_signature: bool,
    pub allowed_firmware_versions: Vec<String>,
}

// ---------------------------------------------------------------------------
// evaluate_trust
// ---------------------------------------------------------------------------

/// Evaluate a device's trust posture against the given policy at time `now`.
///
/// Decision logic (evaluated in order):
/// 1. No firmware manifest -> `Untrusted`
/// 2. No last attestation -> `Untrusted`
/// 3. Stale attestation (older than policy max) -> `Provisional`
/// 4. Signature required but missing -> `Provisional`
/// 5. Firmware version not in allowed list (when list non-empty) -> `Untrusted`
/// 6. Otherwise -> `Trusted`
#[must_use]
pub fn evaluate_trust(device: &DeviceTrust, policy: &TrustPolicy, now: DateTime<Utc>) -> TrustPosture {
    // 1. No firmware manifest
    let Some(firmware) = &device.firmware else {
        return TrustPosture::Untrusted;
    };

    // 2. No last attestation
    let Some(last_attestation) = device.last_attestation else {
        return TrustPosture::Untrusted;
    };

    // 3. Stale attestation
    let age = now.signed_duration_since(last_attestation);
    let max_secs = i64::try_from(policy.max_attestation_age_secs).unwrap_or(i64::MAX);
    let max_age = chrono::TimeDelta::seconds(max_secs);
    if age > max_age {
        return TrustPosture::Provisional;
    }

    // 4. Signature required but missing
    if policy.require_firmware_signature && firmware.ed25519_signature.is_none() {
        return TrustPosture::Provisional;
    }

    // 5. Firmware version not in allowed list
    if !policy.allowed_firmware_versions.is_empty() && !policy.allowed_firmware_versions.contains(&firmware.version) {
        return TrustPosture::Untrusted;
    }

    // 6. All checks passed
    TrustPosture::Trusted
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_trust::{FirmwareManifest, FlashPartition};
    use chrono::{TimeDelta, Utc};
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn base_policy() -> TrustPolicy {
        TrustPolicy {
            max_attestation_age_secs: 3600,
            require_firmware_signature: false,
            allowed_firmware_versions: vec![],
        }
    }

    fn trusted_device(now: DateTime<Utc>) -> DeviceTrust {
        DeviceTrust {
            host_id: Uuid::new_v4(),
            tenant_id: "tenant-001".to_string(),
            posture: TrustPosture::Untrusted, // initial value; evaluator overrides
            firmware: Some(FirmwareManifest {
                version: "1.0.0".to_string(),
                sha256: "abc123".to_string(),
                crc32: 42,
                ed25519_signature: Some("sig".to_string()),
                partition: FlashPartition::A,
            }),
            sbom_hash: Some("sbom_hash".to_string()),
            last_attestation: Some(now),
            created_at: now,
            updated_at: now,
        }
    }

    // -----------------------------------------------------------------------
    // All conditions pass -> Trusted
    // -----------------------------------------------------------------------

    #[test]
    fn all_conditions_pass_returns_trusted() {
        let now = Utc::now();
        let device = trusted_device(now);
        let policy = base_policy();
        assert_eq!(evaluate_trust(&device, &policy, now), TrustPosture::Trusted);
    }

    // -----------------------------------------------------------------------
    // No firmware -> Untrusted
    // -----------------------------------------------------------------------

    #[test]
    fn no_firmware_returns_untrusted() {
        let now = Utc::now();
        let mut device = trusted_device(now);
        device.firmware = None;
        let policy = base_policy();
        assert_eq!(evaluate_trust(&device, &policy, now), TrustPosture::Untrusted);
    }

    // -----------------------------------------------------------------------
    // No attestation -> Untrusted
    // -----------------------------------------------------------------------

    #[test]
    fn no_attestation_returns_untrusted() {
        let now = Utc::now();
        let mut device = trusted_device(now);
        device.last_attestation = None;
        let policy = base_policy();
        assert_eq!(evaluate_trust(&device, &policy, now), TrustPosture::Untrusted);
    }

    // -----------------------------------------------------------------------
    // Stale attestation -> Provisional
    // -----------------------------------------------------------------------

    #[test]
    fn stale_attestation_returns_provisional() {
        let now = Utc::now();
        let mut device = trusted_device(now);
        // Attestation 2 hours ago, policy allows 1 hour
        device.last_attestation = Some(now - TimeDelta::seconds(7200));
        let policy = base_policy(); // max_attestation_age_secs = 3600
        assert_eq!(evaluate_trust(&device, &policy, now), TrustPosture::Provisional);
    }

    // -----------------------------------------------------------------------
    // Missing signature when required -> Provisional
    // -----------------------------------------------------------------------

    #[test]
    fn missing_signature_when_required_returns_provisional() {
        let now = Utc::now();
        let mut device = trusted_device(now);
        device.firmware.as_mut().unwrap().ed25519_signature = None;
        let mut policy = base_policy();
        policy.require_firmware_signature = true;
        assert_eq!(evaluate_trust(&device, &policy, now), TrustPosture::Provisional);
    }

    // -----------------------------------------------------------------------
    // Unknown firmware version -> Untrusted
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_firmware_version_returns_untrusted() {
        let now = Utc::now();
        let device = trusted_device(now); // version "1.0.0"
        let mut policy = base_policy();
        policy.allowed_firmware_versions = vec!["2.0.0".to_string(), "3.0.0".to_string()];
        assert_eq!(evaluate_trust(&device, &policy, now), TrustPosture::Untrusted);
    }

    // -----------------------------------------------------------------------
    // Empty allowed versions accepts any version
    // -----------------------------------------------------------------------

    #[test]
    fn empty_allowed_versions_accepts_any() {
        let now = Utc::now();
        let device = trusted_device(now);
        let policy = base_policy(); // allowed_firmware_versions is empty by default
        assert_eq!(evaluate_trust(&device, &policy, now), TrustPosture::Trusted);
    }

    // -----------------------------------------------------------------------
    // Fresh attestation with valid firmware -> Trusted
    // -----------------------------------------------------------------------

    #[test]
    fn fresh_attestation_with_valid_firmware_returns_trusted() {
        let now = Utc::now();
        let mut device = trusted_device(now);
        // Very recent attestation
        device.last_attestation = Some(now - TimeDelta::seconds(10));
        let mut policy = base_policy();
        policy.require_firmware_signature = true;
        policy.allowed_firmware_versions = vec!["1.0.0".to_string()];
        assert_eq!(evaluate_trust(&device, &policy, now), TrustPosture::Trusted);
    }
}
