//! `SignedManifest` — the CBOR-encoded payload bound to a `.cwasm` file.
//!
//! Binds module identity and content hash (D-05). `expect()` lets callers
//! assert the `module_id`/`version` they expected post-verify, giving
//! replay/downgrade resistance.

use serde::{Deserialize, Serialize};

use super::error::IdentityMismatch;

/// The signed payload. Binds module identity, version, and content hash (D-05).
///
/// Field order is fixed — do not reorder without bumping a version
/// discriminator because CBOR canonical encoding depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedManifest {
    /// Stable module identifier (e.g. "arm-controller").
    pub module_id: String,
    /// Semver string (e.g. "1.2.3").
    pub version: String,
    /// Lowercase hex SHA-256 of the .cwasm bytes.
    pub sha256: String,
}

impl SignedManifest {
    /// Assert that this manifest was signed for the exact (`module_id`, `version`)
    /// the caller expected. Callers use this after `verify_detached` returns
    /// `Ok` to gain replay/downgrade resistance (D-05).
    ///
    /// # Errors
    /// Returns `IdentityMismatch` if either field differs from expected.
    pub fn expect(&self, expected_module_id: &str, expected_version: &str) -> Result<(), IdentityMismatch> {
        if self.module_id != expected_module_id || self.version != expected_version {
            return Err(IdentityMismatch {
                expected_module_id: expected_module_id.to_string(),
                expected_version: expected_version.to_string(),
                actual_module_id: self.module_id.clone(),
                actual_version: self.version.clone(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SignedManifest {
        SignedManifest {
            module_id: "arm".into(),
            version: "1.2.0".into(),
            sha256: "deadbeef".into(),
        }
    }

    #[test]
    fn expect_ok_on_matching_identity() {
        assert!(sample().expect("arm", "1.2.0").is_ok());
    }

    #[test]
    fn expect_err_on_module_id_mismatch() {
        let e = sample().expect("leg", "1.2.0").unwrap_err();
        assert_eq!(e.expected_module_id, "leg");
        assert_eq!(e.actual_module_id, "arm");
    }

    #[test]
    fn expect_err_on_version_mismatch() {
        let e = sample().expect("arm", "0.9.0").unwrap_err();
        assert_eq!(e.expected_version, "0.9.0");
        assert_eq!(e.actual_version, "1.2.0");
    }

    #[test]
    fn json_roundtrip_preserves_fields() {
        let orig = sample();
        let bytes = serde_json::to_vec(&orig).unwrap();
        let back: SignedManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(orig, back);
    }
}
