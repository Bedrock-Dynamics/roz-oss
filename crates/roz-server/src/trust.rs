//! Shared device-trust gate (ENF-01).
//!
//! Called from BOTH REST `routes::tasks::create` and gRPC `grpc::tasks::create_task`
//! BEFORE any Restate workflow creation or NATS publish. Fail-closed: missing
//! `device_trust` row, cross-tenant host, non-Trusted posture all reject.
//!
//! Error shape is opaque on the wire (see `error::AppError::trust_rejected` /
//! `tonic::Status::failed_precondition`). Evaluator detail is logged server-side
//! via `tracing::warn!` per CONTEXT D-04.

use roz_core::device_trust::DeviceTrustPosture;
use roz_core::device_trust::evaluator::{TrustPolicy, evaluate_trust};
use sqlx::PgPool;
use uuid::Uuid;

/// Opaque rejection. `reason` is for server-side logging only — NEVER returned
/// on the wire (see CONTEXT D-04 / Pitfall 8).
#[derive(Debug)]
#[allow(dead_code)]
pub struct TrustRejection {
    pub reason: String,
}

/// Evaluate trust for the given host within the given tenant.
///
/// Fail-closed semantics (CONTEXT D-06, D-07):
/// 1. Missing `roz_device_trust` row → reject.
/// 2. `device.tenant_id != tenant_id` → reject (defense-in-depth beyond RLS,
///    since gRPC uses a bare pool — Pitfall 3).
/// 3. `evaluate_trust(...) != Trusted` → reject (Provisional and Untrusted
///    both reject per D-06).
///
/// All rejection paths emit `tracing::warn!` with structured fields and return
/// an opaque `TrustRejection`. Callers map this to HTTP 409 (REST) or
/// `FailedPrecondition` (gRPC).
pub async fn check_host_trust(
    pool: &PgPool,
    tenant_id: Uuid,
    host_id: Uuid,
    policy: &TrustPolicy,
) -> Result<(), TrustRejection> {
    // Step 1: Load device_trust row.
    let device = match roz_db::device_trust::get_by_host_id(pool, host_id).await {
        Ok(Some(device)) => device,
        Ok(None) => {
            let reason = "no device_trust row for host".to_string();
            tracing::warn!(
                %tenant_id,
                %host_id,
                reason = %reason,
                "host trust posture check failed"
            );
            return Err(TrustRejection { reason });
        }
        Err(e) => {
            let reason = format!("device_trust load failed: {e}");
            tracing::warn!(
                %tenant_id,
                %host_id,
                reason = %reason,
                "host trust posture check failed"
            );
            return Err(TrustRejection { reason });
        }
    };

    // Step 2: Defense-in-depth tenant check. `device.tenant_id` is stored as
    // a UUID in Postgres but lifted into `String` by the core type (legacy
    // shape); compare via the stringified incoming tenant_id.
    if device.tenant_id != tenant_id.to_string() {
        let reason = format!("tenant mismatch (row tenant={}, caller={tenant_id})", device.tenant_id);
        tracing::warn!(
            %tenant_id,
            %host_id,
            reason = %reason,
            "host trust posture check failed"
        );
        return Err(TrustRejection { reason });
    }

    // Step 3: Policy evaluation (fail-closed: only Trusted passes).
    let posture = evaluate_trust(&device, policy, chrono::Utc::now());
    match posture {
        DeviceTrustPosture::Trusted => Ok(()),
        DeviceTrustPosture::Provisional | DeviceTrustPosture::Untrusted => {
            let reason = format!("posture = {posture:?}");
            tracing::warn!(
                %tenant_id,
                %host_id,
                ?posture,
                reason = %reason,
                "host trust posture check failed"
            );
            Err(TrustRejection { reason })
        }
    }
}

/// Load the global trust policy from `ROZ_TRUST_*` env vars at server startup.
///
/// Fail-closed defaults:
/// - `ROZ_TRUST_MAX_ATTESTATION_AGE_SECS` — 3600 (tight bound)
/// - `ROZ_TRUST_REQUIRE_FW_SIG` — true (strict `"true"` or `"1"`, else default true on parse failure)
/// - `ROZ_TRUST_ALLOWED_FW_VERSIONS` — empty (matches evaluator semantics:
///   empty list accepts any version)
#[must_use]
pub fn load_trust_policy_from_env() -> TrustPolicy {
    let max_attestation_age_secs = std::env::var("ROZ_TRUST_MAX_ATTESTATION_AGE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600);

    // Strict lowercase: `"true"` / `"1"` → true; `"false"` / `"0"` → false.
    // Any other value (including unset) → default true (fail-closed per D-10
    // / threat T-13-02-10).
    let require_firmware_signature = match std::env::var("ROZ_TRUST_REQUIRE_FW_SIG").as_deref() {
        Ok("false" | "0") => false,
        Ok("true" | "1") | Err(_) => true,
        Ok(other) => {
            tracing::warn!(
                value = %other,
                "ROZ_TRUST_REQUIRE_FW_SIG has ambiguous value; defaulting to true (fail-closed)"
            );
            true
        }
    };

    let allowed_firmware_versions: Vec<String> = std::env::var("ROZ_TRUST_ALLOWED_FW_VERSIONS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    tracing::info!(
        max_attestation_age_secs,
        require_firmware_signature,
        fw_versions_count = allowed_firmware_versions.len(),
        "trust policy loaded"
    );

    TrustPolicy {
        max_attestation_age_secs,
        require_firmware_signature,
        allowed_firmware_versions,
    }
}

/// Permissive trust policy for tests.
///
/// Accepts any device with a firmware manifest and attestation, regardless of
/// age or signature. NEVER use in production; callers must use
/// `load_trust_policy_from_env` at startup.
#[cfg(test)]
#[must_use]
pub const fn permissive_test_policy() -> TrustPolicy {
    TrustPolicy {
        // 10 years in seconds — large enough to make any reasonable attestation
        // fresh, but small enough that chrono::TimeDelta::seconds does not panic.
        max_attestation_age_secs: 315_360_000,
        require_firmware_signature: false,
        allowed_firmware_versions: vec![],
    }
}

// Runtime helper mirroring `permissive_test_policy` for use from integration
// tests (separate binary crate — cannot see `#[cfg(test)]` items). Only
// compiled into the library when the `test-fixtures` feature-ish gate is not
// required; since Rust integration tests link against the library crate as an
// external dep, we expose an explicitly-named public constructor.
#[doc(hidden)]
#[must_use]
pub const fn permissive_policy_for_integration_tests() -> TrustPolicy {
    TrustPolicy {
        // 10 years in seconds — large enough to make any reasonable attestation
        // fresh, but small enough that chrono::TimeDelta::seconds does not panic.
        max_attestation_age_secs: 315_360_000,
        require_firmware_signature: false,
        allowed_firmware_versions: vec![],
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env-var tests must run sequentially: `std::env::set_var` / `remove_var`
    /// mutate process-global state. Holding this mutex across the test body
    /// serialises trust-policy env reads without needing the `serial_test`
    /// crate. `#[tokio::test]` is not used (no async needed) so lock-hold is
    /// bounded to one thread.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    // `std::env::set_var` / `remove_var` became `unsafe` in Rust 2024 because
    // concurrent env mutation is UB in the presence of other threads reading
    // env. Test body holds `ENV_MUTEX`, and trust-policy tests are the only
    // env mutators in this crate, so the precondition is met for the scope
    // of this helper.
    #[expect(
        unsafe_code,
        reason = "test-only env-var mutation; serialised via ENV_MUTEX, no other threads mutate these keys"
    )]
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], body: F) {
        // Poisoning means a prior test panicked mid-mutation; env may be dirty
        // but the guard still serialises this run, so recover instead of cascading.
        let _guard = ENV_MUTEX.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(*k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(value) => unsafe { std::env::set_var(k, value) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        body();
        for (k, original) in saved {
            match original {
                Some(value) => unsafe { std::env::set_var(&k, value) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
    }

    #[test]
    fn load_trust_policy_defaults_when_unset() {
        with_env(
            &[
                ("ROZ_TRUST_MAX_ATTESTATION_AGE_SECS", None),
                ("ROZ_TRUST_REQUIRE_FW_SIG", None),
                ("ROZ_TRUST_ALLOWED_FW_VERSIONS", None),
            ],
            || {
                let policy = load_trust_policy_from_env();
                assert_eq!(policy.max_attestation_age_secs, 3600);
                assert!(policy.require_firmware_signature);
                assert!(policy.allowed_firmware_versions.is_empty());
            },
        );
    }

    #[test]
    fn load_trust_policy_parses_env_vars() {
        with_env(
            &[
                ("ROZ_TRUST_MAX_ATTESTATION_AGE_SECS", Some("60")),
                ("ROZ_TRUST_REQUIRE_FW_SIG", Some("false")),
                ("ROZ_TRUST_ALLOWED_FW_VERSIONS", Some("1.0.0,1.0.1")),
            ],
            || {
                let policy = load_trust_policy_from_env();
                assert_eq!(policy.max_attestation_age_secs, 60);
                assert!(!policy.require_firmware_signature);
                assert_eq!(
                    policy.allowed_firmware_versions,
                    vec!["1.0.0".to_string(), "1.0.1".to_string()]
                );
            },
        );
    }

    #[test]
    fn load_trust_policy_ambiguous_sig_value_defaults_true() {
        with_env(
            &[
                ("ROZ_TRUST_REQUIRE_FW_SIG", Some("YES")),
                ("ROZ_TRUST_MAX_ATTESTATION_AGE_SECS", None),
                ("ROZ_TRUST_ALLOWED_FW_VERSIONS", None),
            ],
            || {
                // "YES" is NOT the strict `"true"` / `"1"` — fail-closed to true.
                let policy = load_trust_policy_from_env();
                assert!(policy.require_firmware_signature);
            },
        );
    }

    #[test]
    fn load_trust_policy_empty_allowed_list_parses_as_empty() {
        with_env(
            &[
                ("ROZ_TRUST_ALLOWED_FW_VERSIONS", Some("")),
                ("ROZ_TRUST_MAX_ATTESTATION_AGE_SECS", None),
                ("ROZ_TRUST_REQUIRE_FW_SIG", None),
            ],
            || {
                let policy = load_trust_policy_from_env();
                assert!(policy.allowed_firmware_versions.is_empty());
            },
        );
    }

    #[test]
    fn permissive_test_policy_accepts_anything() {
        let p = permissive_test_policy();
        // Very large (10 years) — not u64::MAX because chrono::TimeDelta panics
        // on that.
        assert!(p.max_attestation_age_secs >= 315_360_000);
        assert!(!p.require_firmware_signature);
        assert!(p.allowed_firmware_versions.is_empty());
    }
}
