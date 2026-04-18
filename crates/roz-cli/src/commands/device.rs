//! `roz device` subcommand family (Phase 23 plan 23-09).
//!
//! Operator-facing surface for per-device signing-key lifecycle. Currently
//! exposes `roz device rotate-key`, which unconditionally rotates the host's
//! Ed25519 signing key via `POST /v1/device/rotate-key`.
//!
//! # Environment
//!
//! The subcommand reads the same environment surface as `roz-worker` so that
//! running it on the host where the worker runs "just works":
//!
//! - `ROZ_API_URL` — API base URL (required)
//! - `ROZ_API_KEY` — per-host API key (required)
//! - `ROZ_WORKER_ID` — worker name used during registration (default: hostname)
//! - `ROZ_ENCRYPTION_KEY` — 32-byte base64 AES-256-GCM key for decrypting the
//!   on-disk signing seed (required)
//! - `ROZ_DATA_DIR` — data directory override (default: `/etc/roz` when present,
//!   else OS config dir)
//!
//! # Exit codes
//!
//! - `0` on success
//! - Non-zero with an actionable error message on any failure. No private
//!   key material is ever printed; only the numeric `key_version` and
//!   `created_at` timestamp.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use roz_core::StaticKeyProvider;
use roz_worker::registration;
use roz_worker::signing_key;

/// `roz device ...` subcommand group.
#[derive(Debug, Args)]
pub struct DeviceArgs {
    #[command(subcommand)]
    pub command: DeviceCommand,
}

/// Individual device-key management actions.
#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// Force-rotate this host's device signing key.
    ///
    /// Signs `POST /v1/device/rotate-key` with the current key. The old key
    /// remains server-side valid for a 24h overlap window; new publishes
    /// immediately use the new key. Manual override of the 90-day auto-rotate
    /// policy (D-07).
    RotateKey,
    /// Clear a worker's deadman failsafe latch (Phase 24 FS-01 D-02).
    ///
    /// After a deadman fire, motion stays latched on the worker per D-02 —
    /// next valid command does NOT auto-clear. This subcommand issues the
    /// operator-initiated re-arm: the server signs a `clear_failsafe`
    /// envelope and publishes it on `cmd.{worker_id}.clear_failsafe`; the
    /// worker verifies the signature and un-latches.
    ClearFailsafe {
        /// Target worker id (matches `ROZ_WORKER_ID` on the device, i.e.
        /// the registered host name scoped to this tenant).
        worker_id: String,
        /// Optional free-text reason recorded in the server audit log.
        #[arg(long)]
        reason: Option<String>,
    },
}

/// Dispatch a `roz device <cmd>` invocation.
pub async fn execute(cmd: &DeviceCommand) -> Result<()> {
    match cmd {
        DeviceCommand::RotateKey => rotate_key().await,
        DeviceCommand::ClearFailsafe { worker_id, reason } => clear_failsafe(worker_id, reason.clone()).await,
    }
}

/// Env-backed worker identity required to locate the device key on disk and
/// authenticate the lookup call to the server.
struct WorkerEnv {
    api_url: String,
    api_key: String,
    worker_id: String,
}

impl WorkerEnv {
    fn from_env() -> Result<Self> {
        let api_url = std::env::var("ROZ_API_URL")
            .context("ROZ_API_URL is required (points at the roz-server this host is registered against)")?;
        let api_key = std::env::var("ROZ_API_KEY")
            .context("ROZ_API_KEY is required (per-host API key issued during worker enrollment)")?;
        let worker_id = std::env::var("ROZ_WORKER_ID").ok().unwrap_or_else(|| {
            // Mirror `roz_worker::config::default_worker_id` without taking a
            // dep on the private helper: hostname env → HOST → "unknown".
            std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("HOST"))
                .unwrap_or_else(|_| "unknown".to_string())
        });
        Ok(Self {
            api_url,
            api_key,
            worker_id,
        })
    }
}

async fn rotate_key() -> Result<()> {
    let env = WorkerEnv::from_env()?;

    let provider = Arc::new(
        StaticKeyProvider::from_env().context("ROZ_ENCRYPTION_KEY missing or invalid (32-byte base64 key required)")?,
    );
    let http = reqwest::Client::builder()
        .build()
        .context("build HTTP client for rotate-key")?;
    let dir = signing_key::data_dir();

    // Resolve (host_id, tenant_id) via a read-only host lookup. Do NOT call
    // `register_host` — that flips status to `online` as a side effect.
    let identity = registration::lookup_host_identity(&http, &env.api_url, &env.api_key, &env.worker_id)
        .await
        .context("look up host identity from server")?
        .ok_or_else(|| {
            anyhow!(
                "no host named `{}` found on {}; has this worker been enrolled? \
                 Set ROZ_WORKER_ID if the hostname differs from the registered name.",
                env.worker_id,
                env.api_url
            )
        })?;

    let current = signing_key::load(&dir, &provider, identity.tenant_id, identity.host_id)
        .await
        .context("load current device key from disk")?
        .ok_or_else(|| {
            anyhow!(
                "no device key on disk at {}; run the worker once to enroll via POST /v1/device/provision-key first",
                dir.display()
            )
        })?;

    // Status output goes to stderr so stdout stays clean for callers that
    // parse CLI output; exit code is the real machine signal here.
    eprintln!(
        "Rotating key for host {} (current version {} created {})",
        identity.host_id, current.key_version, current.created_at
    );

    let new_mat = signing_key::force_rotate(&current, &dir, &http, &env.api_url, &provider)
        .await
        .context("POST /v1/device/rotate-key")?;

    eprintln!(
        "Rotated: new key version {} (created {})",
        new_mat.key_version, new_mat.created_at
    );
    eprintln!(
        "Old key version {} remains valid for 24h overlap (D-07).",
        current.key_version
    );

    Ok(())
}

/// POST `/v1/device/clear-failsafe` for the given `worker_id` (Phase 24
/// FS-01 D-02).
///
/// Sends the operator-initiated re-arm over bearer-auth REST; the server
/// signs the server→worker envelope and publishes to
/// `cmd.{worker_id}.clear_failsafe`. The worker's subscriber (Plan 24-06
/// Task 2) verifies the signature and clears the motion latch.
async fn clear_failsafe(worker_id: &str, reason: Option<String>) -> Result<()> {
    // Skip the on-disk signing seed load — this request does not ride the
    // device signing key; bearer auth is sufficient for an operator action.
    let api_url = std::env::var("ROZ_API_URL")
        .context("ROZ_API_URL is required (points at the roz-server this host is registered against)")?;
    let api_key = std::env::var("ROZ_API_KEY")
        .context("ROZ_API_KEY is required (tenant-scoped API key issued during worker enrollment)")?;

    let http = reqwest::Client::builder()
        .build()
        .context("build HTTP client for clear-failsafe")?;

    #[derive(serde::Serialize)]
    struct Body<'a> {
        worker_id: &'a str,
        reason: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Response {
        cleared_at: chrono::DateTime<chrono::Utc>,
        correlation_id: uuid::Uuid,
    }

    let url = format!("{api_url}/v1/device/clear-failsafe");
    let resp: Response = http
        .post(&url)
        .bearer_auth(&api_key)
        .json(&Body { worker_id, reason })
        .send()
        .await
        .context("POST /v1/device/clear-failsafe")?
        .error_for_status()
        .context("clear-failsafe returned non-2xx")?
        .json()
        .await
        .context("parse clear-failsafe response")?;

    eprintln!(
        "Cleared failsafe on {} at {} (correlation_id={})",
        worker_id,
        resp.cleared_at.to_rfc3339(),
        resp.correlation_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_failsafe_variant_constructs() {
        let cmd = DeviceCommand::ClearFailsafe {
            worker_id: "host1".into(),
            reason: Some("manual".into()),
        };
        match cmd {
            DeviceCommand::ClearFailsafe { worker_id, reason } => {
                assert_eq!(worker_id, "host1");
                assert_eq!(reason.as_deref(), Some("manual"));
            }
            DeviceCommand::RotateKey => panic!("wrong variant"),
        }
    }
}
