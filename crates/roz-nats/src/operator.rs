use nkeys::KeyPair;

/// Generate a new NATS account keypair.
///
/// The returned keypair has a public key starting with `A` and a seed starting with `SA`.
pub fn generate_account_keypair() -> KeyPair {
    KeyPair::new_account()
}

/// Generate a new NATS user keypair.
///
/// The returned keypair has a public key starting with `U` and a seed starting with `SU`.
pub fn generate_user_keypair() -> KeyPair {
    KeyPair::new_user()
}

/// Encode a seed (private key) as a string for storage.
///
/// # Panics
///
/// Panics if the keypair was created from a public key only (no seed available).
pub fn encode_seed(kp: &KeyPair) -> String {
    kp.seed().expect("keypair should have a seed")
}

/// Decode a seed string back into a keypair.
pub fn decode_seed(seed: &str) -> Result<KeyPair, nkeys::error::Error> {
    KeyPair::from_seed(seed)
}

/// Configuration for a tenant's NATS account JWT.
#[derive(Debug, Clone)]
pub struct AccountClaims {
    /// Friendly name for the account.
    pub name: String,
    /// Maximum number of active connections (-1 for unlimited).
    pub max_connections: i64,
    /// Maximum message payload size in bytes (-1 for unlimited).
    pub max_payload: i64,
    /// Whether `JetStream` is enabled for this account.
    pub jetstream_enabled: bool,
}

/// Create a signed account JWT using the operator's signing key.
///
/// The JWT contains resource limits for the NATS account, and is signed by the
/// operator key so the NATS server can verify its authenticity.
///
/// Note: Subject permissions (pub/sub allow lists) are deferred to the account
/// provisioning step (Task 11) where tenant-scoped subject patterns are available.
///
/// # Arguments
///
/// * `operator` - The operator keypair used to sign the JWT.
/// * `account` - The account keypair whose public key becomes the JWT subject.
/// * `claims` - Configuration specifying limits and capabilities for the account.
///
/// # Errors
///
/// Returns an error if the account builder conversion fails.
pub fn create_account_jwt(
    operator: &KeyPair,
    account: &KeyPair,
    claims: &AccountClaims,
) -> Result<String, nats_io_jwt::error::ConversionError> {
    let mut limits_builder = nats_io_jwt::OperatorLimits::builder();
    limits_builder = limits_builder.conn(claims.max_connections).payload(claims.max_payload);

    if claims.jetstream_enabled {
        // Enable JetStream with default unlimited limits
        limits_builder = limits_builder.streams(-1).consumer(-1).mem_storage(-1).disk_storage(-1);
    } else {
        // Disable JetStream by setting all limits to zero
        limits_builder = limits_builder.streams(0).consumer(0).mem_storage(0).disk_storage(0);
    }

    let operator_limits: nats_io_jwt::OperatorLimits = limits_builder.try_into()?;

    let nats_account: nats_io_jwt::Account = nats_io_jwt::Account::builder().limits(operator_limits).try_into()?;

    let jwt = nats_io_jwt::Token::new(account.public_key())
        .name(claims.name.clone())
        .claims(nats_io_jwt::Claims::Account(nats_account))
        .sign(operator);

    Ok(jwt)
}

/// Push an account JWT to the NATS server via `$SYS.REQ.CLAIMS.UPDATE`.
///
/// This publishes the signed JWT to the system subject that the NATS server
/// monitors for account claim updates. After publishing, the connection is
/// flushed to ensure delivery.
///
/// # Errors
///
/// Returns an error if the publish or flush operation fails.
pub async fn push_account_jwt(nats: &async_nats::Client, jwt: &str) -> Result<(), async_nats::Error> {
    nats.publish("$SYS.REQ.CLAIMS.UPDATE", bytes::Bytes::from(jwt.to_owned()))
        .await?;
    nats.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_account_keypair_has_correct_prefix() {
        let kp = generate_account_keypair();
        let public = kp.public_key();
        assert!(
            public.starts_with('A'),
            "account public key should start with 'A', got: {public}"
        );
    }

    #[test]
    fn generate_user_keypair_has_correct_prefix() {
        let kp = generate_user_keypair();
        let public = kp.public_key();
        assert!(
            public.starts_with('U'),
            "user public key should start with 'U', got: {public}"
        );
    }

    #[test]
    fn generate_account_keypair_is_unique() {
        let kp1 = generate_account_keypair();
        let kp2 = generate_account_keypair();
        assert_ne!(kp1.public_key(), kp2.public_key());
    }

    #[test]
    fn generate_user_keypair_is_unique() {
        let kp1 = generate_user_keypair();
        let kp2 = generate_user_keypair();
        assert_ne!(kp1.public_key(), kp2.public_key());
    }

    #[test]
    fn encode_seed_roundtrips_account_keypair() {
        let kp = generate_account_keypair();
        let seed = encode_seed(&kp);
        let recovered = decode_seed(&seed).expect("decode should succeed");
        assert_eq!(kp.public_key(), recovered.public_key());
    }

    #[test]
    fn encode_seed_roundtrips_user_keypair() {
        let kp = generate_user_keypair();
        let seed = encode_seed(&kp);
        let recovered = decode_seed(&seed).expect("decode should succeed");
        assert_eq!(kp.public_key(), recovered.public_key());
    }

    #[test]
    fn decode_seed_rejects_invalid_input() {
        let result = decode_seed("not-a-valid-seed");
        assert!(result.is_err(), "decode_seed should reject invalid input");
    }

    #[test]
    fn account_seed_starts_with_sa() {
        let kp = generate_account_keypair();
        let seed = encode_seed(&kp);
        assert!(
            seed.starts_with("SA"),
            "account seed should start with 'SA', got prefix: {}",
            &seed[..2]
        );
    }

    #[test]
    fn user_seed_starts_with_su() {
        let kp = generate_user_keypair();
        let seed = encode_seed(&kp);
        assert!(
            seed.starts_with("SU"),
            "user seed should start with 'SU', got prefix: {}",
            &seed[..2]
        );
    }

    #[test]
    fn create_account_jwt_is_valid() {
        let operator = KeyPair::new_operator();
        let account = generate_account_keypair();
        let claims = AccountClaims {
            name: "test-tenant".to_owned(),
            max_connections: 100,
            max_payload: 1_048_576,
            jetstream_enabled: false,
        };

        let jwt = create_account_jwt(&operator, &account, &claims).expect("JWT creation should succeed");

        // JWT is a base64url-encoded JSON structure: header.payload.signature
        assert!(
            jwt.starts_with("eyJ"),
            "JWT should start with base64-encoded JSON header"
        );
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT should have three dot-separated parts");
    }

    #[test]
    fn create_account_jwt_with_jetstream() {
        let operator = KeyPair::new_operator();
        let account = generate_account_keypair();
        let claims = AccountClaims {
            name: "js-tenant".to_owned(),
            max_connections: 50,
            max_payload: 512_000,
            jetstream_enabled: true,
        };

        let jwt = create_account_jwt(&operator, &account, &claims).expect("JWT creation should succeed");

        assert!(
            jwt.starts_with("eyJ"),
            "JWT should start with base64-encoded JSON header"
        );
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT should have three dot-separated parts");

        // Decode the payload (second part) to verify JetStream limits are set
        let payload = base64_url_decode(parts[1]);
        let payload_json: serde_json::Value = serde_json::from_slice(&payload).expect("payload should be valid JSON");

        // The nats field contains the account claims with limits
        let nats_claims = &payload_json["nats"];
        assert!(nats_claims.is_object(), "JWT should contain nats claims");

        let limits = &nats_claims["limits"];
        // JetStream enabled: streams and consumer should be -1 (unlimited)
        assert_eq!(
            limits["streams"], -1,
            "streams should be unlimited when JetStream is enabled"
        );
        assert_eq!(
            limits["consumer"], -1,
            "consumer should be unlimited when JetStream is enabled"
        );
    }

    #[test]
    fn create_account_jwt_without_jetstream_disables_streams() {
        let operator = KeyPair::new_operator();
        let account = generate_account_keypair();
        let claims = AccountClaims {
            name: "no-js-tenant".to_owned(),
            max_connections: 10,
            max_payload: 65_536,
            jetstream_enabled: false,
        };

        let jwt = create_account_jwt(&operator, &account, &claims).expect("JWT creation should succeed");
        let parts: Vec<&str> = jwt.split('.').collect();
        let payload = base64_url_decode(parts[1]);
        let payload_json: serde_json::Value = serde_json::from_slice(&payload).expect("payload should be valid JSON");

        let limits = &payload_json["nats"]["limits"];
        assert_eq!(limits["streams"], 0, "streams should be 0 when JetStream is disabled");
        assert_eq!(limits["consumer"], 0, "consumer should be 0 when JetStream is disabled");
    }

    #[test]
    fn create_account_jwt_contains_account_public_key_as_subject() {
        let operator = KeyPair::new_operator();
        let account = generate_account_keypair();
        let claims = AccountClaims {
            name: "subject-check".to_owned(),
            max_connections: -1,
            max_payload: -1,
            jetstream_enabled: false,
        };

        let jwt = create_account_jwt(&operator, &account, &claims).expect("JWT creation should succeed");
        let parts: Vec<&str> = jwt.split('.').collect();
        let payload = base64_url_decode(parts[1]);
        let payload_json: serde_json::Value = serde_json::from_slice(&payload).expect("payload should be valid JSON");

        assert_eq!(
            payload_json["sub"].as_str().expect("sub field should be a string"),
            account.public_key(),
            "JWT subject should be the account's public key"
        );
    }

    /// Decode a base64url-encoded string (no padding) to bytes.
    fn base64_url_decode(input: &str) -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(input)
            .expect("should be valid base64url")
    }
}
