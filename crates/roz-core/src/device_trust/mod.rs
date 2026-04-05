pub mod evaluator;
pub mod verify;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DeviceTrustPosture
// ---------------------------------------------------------------------------

/// The assessed trust level of a device in the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceTrustPosture {
    Trusted,
    Provisional,
    Untrusted,
}

// ---------------------------------------------------------------------------
// FlashPartition
// ---------------------------------------------------------------------------

/// The A/B firmware partition on a device, used for safe firmware updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlashPartition {
    A,
    B,
}

// ---------------------------------------------------------------------------
// FirmwareManifest
// ---------------------------------------------------------------------------

/// Describes the firmware installed on a device, including integrity hashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareManifest {
    pub version: String,
    pub sha256: String,
    pub crc32: u32,
    pub ed25519_signature: Option<String>,
    pub partition: FlashPartition,
}

// ---------------------------------------------------------------------------
// DeviceTrust
// ---------------------------------------------------------------------------

/// The full trust state of a device, capturing firmware, attestation, and
/// posture information for a tenant-scoped host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceTrust {
    pub host_id: Uuid,
    pub tenant_id: String,
    pub posture: DeviceTrustPosture,
    pub firmware: Option<FirmwareManifest>,
    pub sbom_hash: Option<String>,
    pub last_attestation: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// FlashRequest
// ---------------------------------------------------------------------------

/// A request to flash firmware to a device. The `requires_human_approval`
/// field is always `true` — firmware updates on safety-critical robots must
/// never be auto-approved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashRequest {
    pub host_id: Uuid,
    pub firmware_url: String,
    pub partition: FlashPartition,
    #[serde(skip_deserializing, default = "always_true")]
    requires_human_approval: bool,
}

const fn always_true() -> bool {
    true
}

impl FlashRequest {
    /// Create a new flash request. Human approval is always required.
    #[must_use]
    pub const fn new(host_id: Uuid, firmware_url: String, partition: FlashPartition) -> Self {
        Self {
            host_id,
            firmware_url,
            partition,
            requires_human_approval: true,
        }
    }

    /// Returns `true` — firmware flash always requires human approval.
    #[must_use]
    pub const fn requires_human_approval(&self) -> bool {
        self.requires_human_approval
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn sample_firmware() -> FirmwareManifest {
        FirmwareManifest {
            version: "1.2.3".to_string(),
            sha256: "abcdef1234567890".to_string(),
            crc32: 0xDEAD_BEEF,
            ed25519_signature: Some("sig_base64_here".to_string()),
            partition: FlashPartition::A,
        }
    }

    fn sample_device_trust() -> DeviceTrust {
        let now = Utc::now();
        DeviceTrust {
            host_id: Uuid::new_v4(),
            tenant_id: "tenant-acme-001".to_string(),
            posture: DeviceTrustPosture::Trusted,
            firmware: Some(sample_firmware()),
            sbom_hash: Some("sha256:sbom_hash_value".to_string()),
            last_attestation: Some(now),
            created_at: now,
            updated_at: now,
        }
    }

    // -----------------------------------------------------------------------
    // DeviceTrustPosture serde
    // -----------------------------------------------------------------------

    #[test]
    fn trust_posture_serde_roundtrip() {
        for posture in [
            DeviceTrustPosture::Trusted,
            DeviceTrustPosture::Provisional,
            DeviceTrustPosture::Untrusted,
        ] {
            let json = serde_json::to_string(&posture).unwrap();
            let restored: DeviceTrustPosture = serde_json::from_str(&json).unwrap();
            assert_eq!(posture, restored);
        }
    }

    #[test]
    fn trust_posture_variants_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&DeviceTrustPosture::Trusted).unwrap(),
            "\"trusted\""
        );
        assert_eq!(
            serde_json::to_string(&DeviceTrustPosture::Provisional).unwrap(),
            "\"provisional\""
        );
        assert_eq!(
            serde_json::to_string(&DeviceTrustPosture::Untrusted).unwrap(),
            "\"untrusted\""
        );
    }

    // -----------------------------------------------------------------------
    // FlashPartition serde
    // -----------------------------------------------------------------------

    #[test]
    fn flash_partition_serde_roundtrip() {
        for partition in [FlashPartition::A, FlashPartition::B] {
            let json = serde_json::to_string(&partition).unwrap();
            let restored: FlashPartition = serde_json::from_str(&json).unwrap();
            assert_eq!(partition, restored);
        }
    }

    #[test]
    fn flash_partition_variants_serialize_as_snake_case() {
        assert_eq!(serde_json::to_string(&FlashPartition::A).unwrap(), "\"a\"");
        assert_eq!(serde_json::to_string(&FlashPartition::B).unwrap(), "\"b\"");
    }

    // -----------------------------------------------------------------------
    // FirmwareManifest serde
    // -----------------------------------------------------------------------

    #[test]
    fn firmware_manifest_serde_roundtrip_with_signature() {
        let original = sample_firmware();
        let json = serde_json::to_string(&original).unwrap();
        let restored: FirmwareManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(original.version, restored.version);
        assert_eq!(original.sha256, restored.sha256);
        assert_eq!(original.crc32, restored.crc32);
        assert_eq!(original.ed25519_signature, restored.ed25519_signature);
        assert_eq!(original.partition, restored.partition);
    }

    #[test]
    fn firmware_manifest_serde_roundtrip_without_signature() {
        let original = FirmwareManifest {
            version: "0.1.0".to_string(),
            sha256: "deadbeef".to_string(),
            crc32: 42,
            ed25519_signature: None,
            partition: FlashPartition::B,
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: FirmwareManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.ed25519_signature, None);
        assert_eq!(restored.partition, FlashPartition::B);
    }

    // -----------------------------------------------------------------------
    // DeviceTrust serde
    // -----------------------------------------------------------------------

    #[test]
    fn device_trust_serde_roundtrip() {
        let original = sample_device_trust();
        let json = serde_json::to_string(&original).unwrap();
        let restored: DeviceTrust = serde_json::from_str(&json).unwrap();

        assert_eq!(original.host_id, restored.host_id);
        assert_eq!(original.tenant_id, restored.tenant_id);
        assert_eq!(original.posture, restored.posture);
        assert_eq!(original.sbom_hash, restored.sbom_hash);
    }

    // -----------------------------------------------------------------------
    // FlashRequest
    // -----------------------------------------------------------------------

    #[test]
    fn flash_request_always_requires_human_approval() {
        let req = FlashRequest::new(
            Uuid::new_v4(),
            "https://fw.example.com/v1.2.3.bin".to_string(),
            FlashPartition::A,
        );
        assert!(
            req.requires_human_approval(),
            "firmware flash must always require human approval"
        );
    }

    #[test]
    fn flash_request_serde_roundtrip() {
        let original = FlashRequest::new(
            Uuid::new_v4(),
            "https://fw.example.com/v2.0.0.bin".to_string(),
            FlashPartition::B,
        );
        let json = serde_json::to_string(&original).unwrap();
        let restored: FlashRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(original.host_id, restored.host_id);
        assert_eq!(original.firmware_url, restored.firmware_url);
        assert_eq!(original.partition, restored.partition);
        assert!(restored.requires_human_approval());
    }

    #[test]
    fn flash_request_deserialization_cannot_bypass_approval() {
        let json = serde_json::json!({
            "host_id": Uuid::new_v4(),
            "firmware_url": "https://fw.example.com/evil.bin",
            "partition": "a",
            "requires_human_approval": false
        });
        let restored: FlashRequest = serde_json::from_value(json).unwrap();
        assert!(
            restored.requires_human_approval(),
            "deserialization must not bypass human approval requirement"
        );
    }
}
