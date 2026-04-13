//! Multi-worker process orchestration for Phase 16 integration tests.
//!
//! Pattern D-02: real `roz-worker` binaries, not in-process mocks. Mirrors
//! the `cargo_bin` + `env_clear` + `kill_on_drop(true)` approach used by
//! [`crates/roz-cli/tests/e2e_headless.rs`] lines 141-155.

#![allow(dead_code)] // Used by per-binary integration tests; each binary pulls only what it needs.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// A running worker binary. Drop triggers SIGKILL via `kill_on_drop(true)`.
pub struct Worker {
    pub id: String,
    pub child: Child,
    /// Holds the tempdir containing the generated zenoh config JSON5 alive
    /// for the child's lifetime — dropping it would remove the file the
    /// worker's `ROZ_ZENOH_CONFIG` reader is mmap'ing.
    _zenoh_cfg_dir: tempfile::TempDir,
}

/// Resolve the path to a cargo-built binary.
///
/// Reuses the pattern from `crates/roz-cli/tests/e2e_headless.rs:141-155`:
/// prefer `CARGO_BIN_EXE_<name>` (cargo sets this for integration tests when a
/// `[[bin]]` target exists in the same crate) and fall back to
/// `target/debug/<name>` relative to the workspace root.
#[must_use]
pub fn cargo_bin(name: &str) -> PathBuf {
    if let Ok(p) = std::env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(p);
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p.push("target");
    p.push("debug");
    p.push(name);
    p
}

/// Spawn a `roz-worker` binary with the given zenoh + NATS endpoints.
///
/// `env_clear()` is called FIRST to prevent dev-laptop env leak; only the
/// listed vars are set. `PATH` is passed through so the binary can find
/// dynamic libs.
///
/// stdout/stderr are tee'd into tracing under `worker = <id>` fields so
/// test failure output surfaces the per-worker log interleave.
///
/// # Errors
/// Returns any `std::io::Error` from `tokio::process::Command::spawn`,
/// wrapped in `anyhow::Error`.
#[allow(
    clippy::unused_async,
    reason = "async is required so callers can .await in a tokio runtime context; \
              body may not directly .await but spawn's child-wait hooks must run on a runtime"
)]
pub async fn spawn_worker(
    id: &str,
    zenoh_endpoint: &str,
    nats_url: &str,
    signing_key_path: &str,
) -> anyhow::Result<Worker> {
    // Multicast doesn't traverse the Docker bridge, so the default peer config
    // (from `load_zenoh_config(None)`) never finds the testcontainer router.
    // Write a JSON5 config pointing at the mapped TCP endpoint with multicast
    // disabled, then set `ROZ_ZENOH_CONFIG` so the worker loads it. Keep the
    // tempdir alive for the child's lifetime via the Worker struct.
    let cfg_dir = tempfile::tempdir()?;
    let cfg_path = cfg_dir.path().join("zenoh.json5");
    let cfg_json5 = format!(
        r#"{{
  mode: "peer",
  scouting: {{ multicast: {{ enabled: false }} }},
  connect: {{ endpoints: ["{zenoh_endpoint}"] }},
  listen: {{ endpoints: [] }},
}}"#
    );
    std::fs::write(&cfg_path, cfg_json5)?;

    let mut cmd = Command::new(cargo_bin("roz-worker"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        // Worker calls `logfire::configure()` at main.rs:720 and .expect()s its
        // result. Without a token, it panics with `TokenRequired`. Tests don't
        // want telemetry egress — disable it via the logfire-sanctioned env.
        .env("LOGFIRE_SEND_TO_LOGFIRE", "no")
        .env("ROZ_WORKER_ID", id)
        .env("ROZ_API_URL", "http://localhost:0")
        .env("ROZ_NATS_URL", nats_url)
        .env("ROZ_RESTATE_URL", "http://localhost:0")
        .env("ROZ_API_KEY", "test-key")
        .env("ROZ_GATEWAY_API_KEY", "test-gateway-key")
        // Worker's config loader reads `ROZ_ZENOH_CONFIG` (no `_PATH` suffix,
        // per `WorkerConfig::load` figment alias). Pointed at a peer config
        // that hard-codes the testcontainer TCP endpoint + disables multicast.
        .env("ROZ_ZENOH_CONFIG", &cfg_path)
        // `load_signing_key` (roz_zenoh::envelope) accepts either `base64:<seed>`
        // or a raw path — no `file:` prefix. Pass the raw absolute path.
        .env("ROZ_DEVICE_SIGNING_KEY", signing_key_path)
        .env("RUST_LOG", "info,roz_worker=debug,roz_zenoh=debug")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;

    let id_out = id.to_owned();
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(worker = %id_out, "stdout: {line}");
            }
        });
    }
    let id_err = id.to_owned();
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(worker = %id_err, "stderr: {line}");
            }
        });
    }

    Ok(Worker {
        id: id.to_owned(),
        child,
        _zenoh_cfg_dir: cfg_dir,
    })
}

/// Attempt a graceful shutdown. Sends SIGTERM, waits up to 5s, then falls
/// through to `Child::kill()` (SIGKILL, harmless if already exited).
pub async fn shutdown_worker(mut worker: Worker) {
    #[cfg(unix)]
    if let Some(pid) = worker.child.id() {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "pid from tokio always fits in i32 on supported unix platforms"
        )]
        let raw_pid = pid as i32;
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw_pid), nix::sys::signal::Signal::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), worker.child.wait()).await;
    let _ = worker.child.kill().await;
}
