//! On-the-fly signing helper. No committed binary fixtures.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use roz_copper::wasm_signature::{SignatureEnvelope, SignedManifest, TrustedKeys};
use sha2::{Digest, Sha256};

pub struct SignedFixture {
    #[allow(dead_code)]
    pub signing_key: SigningKey,
    pub verifying_key: VerifyingKey,
    pub key_id: String,
    pub envelope_bytes: Vec<u8>,
    #[allow(dead_code)]
    pub manifest: SignedManifest,
}

pub fn sign_cwasm(cwasm: &[u8], module_id: &str, version: &str, key_id: &str) -> SignedFixture {
    let sk = SigningKey::generate(&mut OsRng);
    let vk = sk.verifying_key();
    let manifest = SignedManifest {
        module_id: module_id.into(),
        version: version.into(),
        sha256: format!("{:x}", Sha256::digest(cwasm)),
    };
    let mut canonical = Vec::new();
    ciborium::into_writer(&manifest, &mut canonical).unwrap();
    let sig = sk.sign(&canonical);
    let envelope = SignatureEnvelope {
        key_id: key_id.into(),
        manifest: manifest.clone(),
        signature: sig.to_bytes().to_vec(),
    };
    let mut bytes = Vec::new();
    ciborium::into_writer(&envelope, &mut bytes).unwrap();
    SignedFixture {
        signing_key: sk,
        verifying_key: vk,
        key_id: key_id.into(),
        envelope_bytes: bytes,
        manifest,
    }
}

pub fn keyset_with(key_id: &str, vk: VerifyingKey) -> TrustedKeys {
    let mut ks = TrustedKeys::new();
    ks.insert(key_id, vk);
    ks
}

/// Precompile the EXACT WAT from `crates/roz-copper/src/wasm.rs` minimal
/// template. Required-export `process(u64) -> ()` so `build_from_module`
/// accepts it on the positive-path test.
#[cfg(feature = "aot")]
pub fn precompile_minimal_cwasm() -> Vec<u8> {
    use wasmtime::{Config, Engine};
    let wat = r#"
        (module
            (func (export "process") (param i64))
        )
    "#;
    let wasm = wat::parse_str(wat).expect("wat parse");
    let mut config = Config::new();
    config.epoch_interruption(true);
    let engine = Engine::new(&config).expect("engine");
    engine.precompile_module(&wasm).expect("precompile")
}
