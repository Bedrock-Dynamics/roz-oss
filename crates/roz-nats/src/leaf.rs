//! Leaf node configuration generation for NATS edge workers.
//!
//! A leaf node is a lightweight NATS server that connects to a central hub,
//! enabling edge workers to participate in the NATS messaging fabric.
//! This module generates the NATS server config and credential files
//! needed for a worker to establish a leaf node connection.

/// Configuration for a NATS leaf node that connects to the central hub.
#[derive(Debug, Clone)]
pub struct LeafNodeConfig {
    /// URL of the central NATS hub (e.g., `"nats-leaf://hub.example.com:7422"`).
    pub remote_url: String,
    /// NATS account public key (A-prefixed).
    pub account_public_key: String,
    /// JWT for the user connecting to the hub.
    pub user_jwt: String,
    /// `NKey` seed for the user (SU-prefixed).
    pub user_seed: String,
}

impl LeafNodeConfig {
    /// Generate a NATS server configuration file for this leaf node.
    ///
    /// The output is a valid NATS server config that configures a leaf node
    /// remote pointing at the hub URL, using a credentials file at `./worker.creds`.
    pub fn to_nats_conf(&self) -> String {
        format!(
            "\
leafnodes {{
    remotes [
        {{
            url: \"{}\"
            credentials: \"./worker.creds\"
        }}
    ]
}}
",
            self.remote_url
        )
    }

    /// Generate a NATS credentials file containing the user JWT and `NKey` seed.
    ///
    /// The output follows the official NATS credentials format with asymmetric
    /// dashes: 5 dashes on BEGIN lines, 6 dashes on END lines.
    pub fn to_creds_file(&self) -> String {
        format!(
            "\
-----BEGIN NATS USER JWT-----
{}
------END NATS USER JWT------

-----BEGIN USER NKEY SEED-----
{}
------END USER NKEY SEED------
",
            self.user_jwt, self.user_seed
        )
    }
}

/// Extract the user JWT from a NATS credentials file.
///
/// Returns `None` if the expected delimiters are not found.
pub fn parse_jwt_from_creds(creds: &str) -> Option<&str> {
    let start_marker = "-----BEGIN NATS USER JWT-----";
    let end_marker = "------END NATS USER JWT------";

    let start = creds.find(start_marker)?;
    let after_start = start + start_marker.len();
    let end = creds[after_start..].find(end_marker)?;
    let content = &creds[after_start..after_start + end];
    Some(content.trim())
}

/// Extract the user `NKey` seed from a NATS credentials file.
///
/// Returns `None` if the expected delimiters are not found.
pub fn parse_seed_from_creds(creds: &str) -> Option<&str> {
    let start_marker = "-----BEGIN USER NKEY SEED-----";
    let end_marker = "------END USER NKEY SEED------";

    let start = creds.find(start_marker)?;
    let after_start = start + start_marker.len();
    let end = creds[after_start..].find(end_marker)?;
    let content = &creds[after_start..after_start + end];
    Some(content.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> LeafNodeConfig {
        LeafNodeConfig {
            remote_url: "nats-leaf://hub.example.com:7422".to_owned(),
            account_public_key: "ABJHLOVMPA5OIPO7PJHKGBMFENNHGOAN2EV5E6MNXFMVRBAL553RVJA".to_owned(),
            user_jwt: "eyJ0eXAiOiJKV1QiLCJhbGciOiJlZDI1NTE5LW5rZXkifQ.payload.signature".to_owned(),
            user_seed: "SUAMLK2ZNL35WSMW37E7UD4VZ7ELPKW7DHC3BWBSD2GCZ7PULNEZ6LOBU".to_owned(),
        }
    }

    #[test]
    fn nats_conf_contains_remote_url() {
        let config = sample_config();
        let conf = config.to_nats_conf();
        assert!(
            conf.contains(&config.remote_url),
            "config should contain the remote URL, got:\n{conf}"
        );
    }

    #[test]
    fn nats_conf_contains_credentials_path() {
        let config = sample_config();
        let conf = config.to_nats_conf();
        assert!(
            conf.contains("credentials: \"./worker.creds\""),
            "config should reference the credentials file, got:\n{conf}"
        );
    }

    #[test]
    fn nats_conf_has_leafnodes_block() {
        let config = sample_config();
        let conf = config.to_nats_conf();
        assert!(
            conf.starts_with("leafnodes {"),
            "config should start with leafnodes block"
        );
        assert!(conf.contains("remotes ["), "config should contain remotes array");
    }

    #[test]
    fn creds_file_contains_jwt_with_correct_delimiters() {
        let config = sample_config();
        let creds = config.to_creds_file();

        assert!(
            creds.contains("-----BEGIN NATS USER JWT-----"),
            "creds should contain JWT begin marker (5 dashes)"
        );
        assert!(
            creds.contains("------END NATS USER JWT------"),
            "creds should contain JWT end marker (6 dashes)"
        );
        assert!(creds.contains(&config.user_jwt), "creds should contain the JWT");
    }

    #[test]
    fn creds_file_contains_seed_with_correct_delimiters() {
        let config = sample_config();
        let creds = config.to_creds_file();

        assert!(
            creds.contains("-----BEGIN USER NKEY SEED-----"),
            "creds should contain seed begin marker (5 dashes)"
        );
        assert!(
            creds.contains("------END USER NKEY SEED------"),
            "creds should contain seed end marker (6 dashes)"
        );
        assert!(creds.contains(&config.user_seed), "creds should contain the seed");
    }

    #[test]
    fn creds_file_asymmetric_dashes() {
        let config = sample_config();
        let creds = config.to_creds_file();

        // BEGIN lines have exactly 5 dashes, END lines have exactly 6.
        // Check by verifying the markers have the right dash counts.
        for line in creds.lines() {
            if line.contains("BEGIN") {
                assert!(
                    line.starts_with("-----") && !line.starts_with("------"),
                    "BEGIN lines should have exactly 5 leading dashes, got: {line}"
                );
            }
            if line.contains("END") {
                assert!(
                    line.starts_with("------") && !line.starts_with("-------"),
                    "END lines should have exactly 6 leading dashes, got: {line}"
                );
                assert!(
                    line.ends_with("------") && !line.ends_with("-------"),
                    "END lines should have exactly 6 trailing dashes, got: {line}"
                );
            }
        }
    }

    #[test]
    fn roundtrip_parse_jwt_from_creds() {
        let config = sample_config();
        let creds = config.to_creds_file();

        let extracted_jwt = parse_jwt_from_creds(&creds).expect("should parse JWT from creds");
        assert_eq!(extracted_jwt, config.user_jwt, "extracted JWT should match original");
    }

    #[test]
    fn roundtrip_parse_seed_from_creds() {
        let config = sample_config();
        let creds = config.to_creds_file();

        let extracted_seed = parse_seed_from_creds(&creds).expect("should parse seed from creds");
        assert_eq!(extracted_seed, config.user_seed, "extracted seed should match original");
    }

    #[test]
    fn roundtrip_with_real_nkeys() {
        // Generate real NATS user keys and verify round-trip through creds file.
        let user_kp = nkeys::KeyPair::new_user();
        let account_kp = nkeys::KeyPair::new_account();

        let config = LeafNodeConfig {
            remote_url: "nats-leaf://prod.roz.io:7422".to_owned(),
            account_public_key: account_kp.public_key(),
            user_jwt: "eyJhbGciOiJlZDI1NTE5In0.test-claims.test-sig".to_owned(),
            user_seed: user_kp.seed().expect("user keypair should have seed"),
        };

        let creds = config.to_creds_file();

        let extracted_jwt = parse_jwt_from_creds(&creds).expect("should parse JWT");
        let extracted_seed = parse_seed_from_creds(&creds).expect("should parse seed");

        assert_eq!(extracted_jwt, config.user_jwt);
        assert_eq!(extracted_seed, config.user_seed);

        // Verify the extracted seed is a valid NKey seed that recovers the original key.
        let recovered = nkeys::KeyPair::from_seed(extracted_seed).expect("should decode extracted seed");
        assert_eq!(recovered.public_key(), user_kp.public_key());
    }

    #[test]
    fn url_with_special_characters() {
        let config = LeafNodeConfig {
            remote_url: "nats-leaf://user:p%40ss@hub.example.com:7422/path?query=val&other=1".to_owned(),
            account_public_key: "ABJHLOVMPA5OIPO7PJHKGBMFENNHGOAN2EV5E6MNXFMVRBAL553RVJA".to_owned(),
            user_jwt: "eyJ0eXAiOiJKV1QifQ.payload.sig".to_owned(),
            user_seed: "SUAMLK2ZNL35WSMW37E7UD4VZ7ELPKW7DHC3BWBSD2GCZ7PULNEZ6LOBU".to_owned(),
        };

        let conf = config.to_nats_conf();
        assert!(
            conf.contains(&config.remote_url),
            "config should preserve special characters in URL"
        );
    }

    #[test]
    fn jwt_with_special_characters() {
        let config = LeafNodeConfig {
            remote_url: "nats-leaf://hub:7422".to_owned(),
            account_public_key: "ABJHLOVMPA5OIPO7PJHKGBMFENNHGOAN2EV5E6MNXFMVRBAL553RVJA".to_owned(),
            user_jwt: "eyJ0eXAi.pay+load/with=padding==.sig_nature-here".to_owned(),
            user_seed: "SUAMLK2ZNL35WSMW37E7UD4VZ7ELPKW7DHC3BWBSD2GCZ7PULNEZ6LOBU".to_owned(),
        };

        let creds = config.to_creds_file();

        let extracted_jwt = parse_jwt_from_creds(&creds).expect("should parse JWT with special chars");
        assert_eq!(extracted_jwt, config.user_jwt);
    }

    #[test]
    fn seed_with_special_characters() {
        let config = LeafNodeConfig {
            remote_url: "nats-leaf://hub:7422".to_owned(),
            account_public_key: "ABJHLOVMPA5OIPO7PJHKGBMFENNHGOAN2EV5E6MNXFMVRBAL553RVJA".to_owned(),
            user_jwt: "eyJ0eXAi.payload.sig".to_owned(),
            user_seed: "SUAMLK2ZNL35WSMW37E7UD4VZ7ELPKW7DHC3BWBSD2GCZ7PULNEZ6LOBU".to_owned(),
        };

        let creds = config.to_creds_file();

        let extracted_seed = parse_seed_from_creds(&creds).expect("should parse seed");
        assert_eq!(extracted_seed, config.user_seed);
    }

    #[test]
    fn parse_jwt_from_invalid_creds_returns_none() {
        assert!(parse_jwt_from_creds("no markers here").is_none());
        assert!(parse_jwt_from_creds("-----BEGIN NATS USER JWT-----\njwt-only-begin").is_none());
    }

    #[test]
    fn parse_seed_from_invalid_creds_returns_none() {
        assert!(parse_seed_from_creds("no markers here").is_none());
        assert!(parse_seed_from_creds("-----BEGIN USER NKEY SEED-----\nseed-only-begin").is_none());
    }

    #[test]
    fn creds_file_sections_are_separated_by_blank_line() {
        let config = sample_config();
        let creds = config.to_creds_file();

        // The JWT section and seed section should be separated by a blank line.
        let jwt_end_idx = creds
            .find("------END NATS USER JWT------")
            .expect("should find JWT end marker");
        let seed_begin_idx = creds
            .find("-----BEGIN USER NKEY SEED-----")
            .expect("should find seed begin marker");

        let between = &creds[jwt_end_idx + "------END NATS USER JWT------".len()..seed_begin_idx];
        assert_eq!(
            between, "\n\n",
            "JWT and seed sections should be separated by exactly one blank line"
        );
    }
}
