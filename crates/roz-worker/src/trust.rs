use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use roz_core::device_trust::{DeviceTrust, DeviceTrustPosture, FirmwareManifest, FlashPartition};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Reports device trust attestation on worker startup.
pub struct TrustReporter {
    host_id: Uuid,
    tenant_id: String,
    is_sim: bool,
}

impl TrustReporter {
    pub const fn new(host_id: Uuid, tenant_id: String, is_sim: bool) -> Self {
        Self {
            host_id,
            tenant_id,
            is_sim,
        }
    }

    /// Compute attestation for the device without signature verification.
    ///
    /// Without a verifying key, firmware presence yields `Provisional` posture
    /// (not `Trusted`). Use [`attest_with_verification`] to supply an Ed25519
    /// key and signature for full trust.
    pub fn attest(&self, firmware_data: Option<&[u8]>, sbom_data: Option<&[u8]>) -> DeviceTrust {
        self.attest_with_verification(firmware_data, sbom_data, None, None)
    }

    /// Compute attestation with optional Ed25519 signature verification.
    ///
    /// Trust posture logic:
    /// - Sim workers: always `Provisional`
    /// - Verified signature: `Trusted`
    /// - Firmware present, no signature provided: `Provisional`
    /// - Firmware present, signature FAILS verification: `Untrusted`
    /// - No firmware: `Untrusted`
    pub fn attest_with_verification(
        &self,
        firmware_data: Option<&[u8]>,
        sbom_data: Option<&[u8]>,
        verifying_key: Option<&VerifyingKey>,
        signature: Option<&Signature>,
    ) -> DeviceTrust {
        let now = chrono::Utc::now();

        // Attempt signature verification if both key and signature are provided
        let sig_result = match (firmware_data, verifying_key, signature) {
            (Some(data), Some(key), Some(sig)) => Some(key.verify(data, sig).is_ok()),
            _ => None,
        };

        let firmware = firmware_data.map(|data| {
            let sha256 = {
                let mut hasher = Sha256::new();
                hasher.update(data);
                hex::encode(hasher.finalize())
            };
            let crc32 = crc32fast::hash(data);

            // Store the hex-encoded signature in the manifest when present
            let ed25519_signature = signature.map(|sig| hex::encode(sig.to_bytes()));

            FirmwareManifest {
                version: "unknown".to_string(),
                sha256,
                crc32,
                ed25519_signature,
                partition: FlashPartition::A,
            }
        });

        let sbom_hash = sbom_data.map(|data| {
            let mut hasher = Sha256::new();
            hasher.update(data);
            hex::encode(hasher.finalize())
        });

        let posture = if self.is_sim {
            DeviceTrustPosture::Provisional
        } else {
            match (firmware.is_some(), sig_result) {
                // Firmware present with verified signature
                (true, Some(true)) => DeviceTrustPosture::Trusted,
                // Firmware present but no signature provided (unknown state)
                (true, None) => DeviceTrustPosture::Provisional,
                // Failed verification (actively suspicious) or no firmware at all
                (true, Some(false)) | (false, _) => DeviceTrustPosture::Untrusted,
            }
        };

        DeviceTrust {
            host_id: self.host_id,
            tenant_id: self.tenant_id.clone(),
            posture,
            firmware,
            sbom_hash,
            last_attestation: Some(now),
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn sim_worker_gets_provisional() {
        let reporter = TrustReporter::new(Uuid::new_v4(), "tenant-1".into(), true);
        let trust = reporter.attest(Some(b"firmware"), Some(b"sbom"));
        assert_eq!(trust.posture, DeviceTrustPosture::Provisional);
    }

    #[test]
    fn physical_worker_with_firmware_but_no_signature_gets_provisional() {
        let reporter = TrustReporter::new(Uuid::new_v4(), "tenant-1".into(), false);
        let trust = reporter.attest(Some(b"real firmware"), None);
        assert_eq!(trust.posture, DeviceTrustPosture::Provisional);
        assert!(trust.firmware.is_some());
    }

    #[test]
    fn physical_worker_without_firmware_gets_untrusted() {
        let reporter = TrustReporter::new(Uuid::new_v4(), "tenant-1".into(), false);
        let trust = reporter.attest(None, None);
        assert_eq!(trust.posture, DeviceTrustPosture::Untrusted);
    }

    #[test]
    fn firmware_hash_computed() {
        let reporter = TrustReporter::new(Uuid::new_v4(), "tenant-1".into(), false);
        let trust = reporter.attest(Some(b"test data"), None);
        let fw = trust.firmware.unwrap();
        assert!(!fw.sha256.is_empty());
        assert!(fw.crc32 > 0);
    }

    #[test]
    fn physical_worker_with_verified_signature_gets_trusted() {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let firmware_data = b"real firmware binary";
        let signature = signing_key.sign(firmware_data);

        let reporter = TrustReporter::new(Uuid::new_v4(), "tenant-1".into(), false);
        let trust = reporter.attest_with_verification(
            Some(firmware_data.as_slice()),
            None,
            Some(&verifying_key),
            Some(&signature),
        );

        assert_eq!(trust.posture, DeviceTrustPosture::Trusted);
        assert!(trust.firmware.is_some());
        let fw = trust.firmware.unwrap();
        assert!(fw.ed25519_signature.is_some(), "signature must be stored in manifest");
    }

    #[test]
    fn physical_worker_with_bad_signature_gets_untrusted() {
        let signing_key = SigningKey::from_bytes(&[1u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let firmware_data = b"real firmware binary";
        // Sign different data to create a mismatched signature
        let signature = signing_key.sign(b"different data entirely");

        let reporter = TrustReporter::new(Uuid::new_v4(), "tenant-1".into(), false);
        let trust = reporter.attest_with_verification(
            Some(firmware_data.as_slice()),
            None,
            Some(&verifying_key),
            Some(&signature),
        );

        assert_eq!(
            trust.posture,
            DeviceTrustPosture::Untrusted,
            "failed signature verification must produce Untrusted (actively suspicious)"
        );
    }
}
