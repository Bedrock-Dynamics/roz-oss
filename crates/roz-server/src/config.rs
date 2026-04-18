use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub nats_url: Option<String>,
    /// Phase 18 D-discretion: object-store root for skill bundled assets.
    /// Defaults to `{data_dir}/skills` (data_dir defaults to `/var/lib/roz`).
    /// Env: `ROZ_SKILL_STORE_ROOT`.
    #[serde(default)]
    pub skill_store_root: Option<std::path::PathBuf>,
    /// Local data directory; used as the default base for `skill_store_root` when
    /// the latter is unset. Defaults to `/var/lib/roz`. Env: `ROZ_DATA_DIR`.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

/// Phase 23 Plan 23-04 (D-12) rollout gate for two-direction signed dispatch.
///
/// Controlled by the `SIGNED_DISPATCH_ENFORCEMENT` env var. Unknown values
/// log a warning and fall back to the environment-appropriate default
/// (`Audit` when `ROZ_ENVIRONMENT=development`, `Strict` everywhere else).
///
/// Serde representation is lowercase (`off` / `audit` / `strict`) to match
/// the env-var values documented in the Phase 23 rollout playbook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignedDispatchEnforcement {
    /// Accept unsigned messages silently. Used only during the pre-v3.0
    /// rollout window; never the production default.
    Off,
    /// Accept all messages but log a warning on missing / invalid signatures.
    /// Default when `ROZ_ENVIRONMENT=development`.
    Audit,
    /// Reject messages with missing or invalid signatures. Default for fresh
    /// v3.0 deployments (staging, production, anything not development).
    Strict,
}

impl SignedDispatchEnforcement {
    /// Planner's Discretion (Phase 23 RESEARCH Q6): default by environment.
    #[must_use]
    pub fn default_for_env(environment: &str) -> Self {
        if environment.eq_ignore_ascii_case("development") || environment.eq_ignore_ascii_case("dev") {
            Self::Audit
        } else {
            Self::Strict
        }
    }

    /// Read `SIGNED_DISPATCH_ENFORCEMENT` and parse into an enum variant.
    ///
    /// Unknown / unset values fall back to [`Self::default_for_env`]. Unknown
    /// values additionally emit a `tracing::warn!` so operators can detect
    /// typos.
    #[must_use]
    pub fn from_env(environment: &str) -> Self {
        match std::env::var("SIGNED_DISPATCH_ENFORCEMENT").ok().as_deref() {
            Some("off") => Self::Off,
            Some("audit") => Self::Audit,
            Some("strict") => Self::Strict,
            Some(other) => {
                tracing::warn!(
                    value = %other,
                    "SIGNED_DISPATCH_ENFORCEMENT unknown value; falling back to env-appropriate default"
                );
                Self::default_for_env(environment)
            }
            None => Self::default_for_env(environment),
        }
    }
}

fn default_data_dir() -> String {
    "/var/lib/roz".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            database_url: String::new(),
            nats_url: None,
            skill_store_root: None,
            data_dir: default_data_dir(),
        }
    }
}

impl Config {
    #[allow(dead_code)]
    pub fn from_env() -> Result<Self, Box<figment::Error>> {
        use figment::{Figment, providers::Env};
        Ok(Figment::new().merge(Env::prefixed("ROZ_")).extract()?)
    }

    /// Resolve the effective `skill_store_root` path, defaulting to
    /// `{data_dir}/skills` when unset (Phase 18 D-discretion in CONTEXT).
    #[allow(dead_code)]
    #[must_use]
    pub fn resolved_skill_store_root(&self) -> std::path::PathBuf {
        self.skill_store_root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(&self.data_dir).join("skills"))
    }
}

#[cfg(test)]
#[allow(
    unsafe_code,
    reason = "Edition-2024 std::env::{set_var,remove_var} are unsafe; env-var tests are gated by serial_test."
)]
mod enforcement_tests {
    use super::SignedDispatchEnforcement;
    use serial_test::serial;

    #[test]
    fn default_strict_in_prod_audit_in_dev() {
        assert_eq!(
            SignedDispatchEnforcement::default_for_env("production"),
            SignedDispatchEnforcement::Strict
        );
        assert_eq!(
            SignedDispatchEnforcement::default_for_env("staging"),
            SignedDispatchEnforcement::Strict
        );
        assert_eq!(
            SignedDispatchEnforcement::default_for_env("development"),
            SignedDispatchEnforcement::Audit
        );
        // Common short alias used in roz-server main.rs env-var handling.
        assert_eq!(
            SignedDispatchEnforcement::default_for_env("dev"),
            SignedDispatchEnforcement::Audit
        );
    }

    #[test]
    #[serial]
    fn from_env_parses_all_three_values() {
        unsafe {
            std::env::set_var("SIGNED_DISPATCH_ENFORCEMENT", "off");
        }
        assert_eq!(
            SignedDispatchEnforcement::from_env("production"),
            SignedDispatchEnforcement::Off
        );

        unsafe {
            std::env::set_var("SIGNED_DISPATCH_ENFORCEMENT", "audit");
        }
        assert_eq!(
            SignedDispatchEnforcement::from_env("production"),
            SignedDispatchEnforcement::Audit
        );

        unsafe {
            std::env::set_var("SIGNED_DISPATCH_ENFORCEMENT", "strict");
        }
        assert_eq!(
            SignedDispatchEnforcement::from_env("production"),
            SignedDispatchEnforcement::Strict
        );

        unsafe {
            std::env::remove_var("SIGNED_DISPATCH_ENFORCEMENT");
        }
    }

    #[test]
    #[serial]
    fn from_env_unknown_falls_back_to_env_default() {
        unsafe {
            std::env::set_var("SIGNED_DISPATCH_ENFORCEMENT", "panic_now");
        }
        assert_eq!(
            SignedDispatchEnforcement::from_env("production"),
            SignedDispatchEnforcement::Strict
        );
        assert_eq!(
            SignedDispatchEnforcement::from_env("development"),
            SignedDispatchEnforcement::Audit
        );
        unsafe {
            std::env::remove_var("SIGNED_DISPATCH_ENFORCEMENT");
        }
    }

    #[test]
    #[serial]
    fn from_env_unset_uses_env_default() {
        unsafe {
            std::env::remove_var("SIGNED_DISPATCH_ENFORCEMENT");
        }
        assert_eq!(
            SignedDispatchEnforcement::from_env("production"),
            SignedDispatchEnforcement::Strict
        );
        assert_eq!(
            SignedDispatchEnforcement::from_env("development"),
            SignedDispatchEnforcement::Audit
        );
    }
}
