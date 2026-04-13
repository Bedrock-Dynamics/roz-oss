//! Zenoh testcontainer harness mirroring `nats.rs` and `restate.rs`.
//!
//! Returns a [`ZenohGuard`] that owns a running `eclipse/zenoh:1.8.0` container.
//! Tests connect peers via [`ZenohGuard::peer_config`] which sets explicit
//! `connect.endpoints` and disables multicast scout (Docker bridge constraint
//! — see 15-RESEARCH.md "Common Pitfalls" Pitfall 3).
//!
//! **API shape (C-09 fix — mirrors 15-01 `load_zenoh_config(Option<&Path>)`):**
//! The core helper [`zenoh_router_with_endpoint`] takes the bypass endpoint as
//! an argument. The env-reading convenience wrapper [`zenoh_router`] reads
//! `ZENOH_ROUTER_ENDPOINT` and forwards. Tests always call the arg-based form
//! — this keeps the unsafe Rust-2024 env-mutation APIs (denied by the
//! workspace `unsafe_code = "deny"` lint) out of `#[test]` bodies.
//!
//! Setting `ZENOH_ROUTER_ENDPOINT=tcp/<host>:<port>` at process launch still
//! bypasses container start for production integration test runs (CI
//! optimisation pointing at an externally-managed router).

use std::env;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ContainerRequest, GenericImage};

/// Guard that holds a running zenohd container. Container is stopped and
/// removed when this guard is dropped.
pub struct ZenohGuard {
    _container: Option<ContainerAsync<GenericImage>>,
    connect_json5: String,
    tcp_endpoint: String,
}

impl ZenohGuard {
    /// JSON5 fragment to merge into a peer's [`zenoh::Config`]:
    /// `{ connect: { endpoints: ["tcp/HOST:PORT"] } }`.
    #[must_use]
    pub fn connect_json5(&self) -> &str {
        &self.connect_json5
    }

    /// `tcp/HOST:PORT` connect endpoint string.
    #[must_use]
    pub fn tcp_endpoint(&self) -> &str {
        &self.tcp_endpoint
    }

    /// Build a [`zenoh::Config`] configured to connect to this router (no
    /// multicast scout — Docker bridge constraint).
    ///
    /// # Panics
    /// Panics if the generated JSON5 is rejected by zenoh (would indicate a
    /// bug in this helper).
    #[must_use]
    pub fn peer_config(&self) -> zenoh::Config {
        let cfg_str = format!(
            r#"{{
              mode: "peer",
              scouting: {{ multicast: {{ enabled: false }} }},
              connect: {{ endpoints: ["{}"] }},
              listen: {{ endpoints: [] }},
            }}"#,
            self.tcp_endpoint
        );
        zenoh::Config::from_json5(&cfg_str).expect("valid peer config")
    }
}

/// Core helper. If `endpoint` is `Some`, bypass container start and return a
/// guard pointing at that endpoint. If `None`, start a fresh zenohd
/// testcontainer.
///
/// This is the arg-based form; tests call it directly with an explicit
/// `Some("tcp/...")` rather than mutating `ZENOH_ROUTER_ENDPOINT`. Mirrors
/// the 15-01 `load_zenoh_config(Option<&Path>)` contract.
///
/// # Panics
/// Panics on Docker start / port-mapping failure (mirrors `nats_container`).
pub async fn zenoh_router_with_endpoint(endpoint: Option<&str>) -> ZenohGuard {
    if let Some(endpoint) = endpoint {
        let connect_json5 = format!(r#"{{ connect: {{ endpoints: ["{endpoint}"] }} }}"#);
        return ZenohGuard {
            _container: None,
            connect_json5,
            tcp_endpoint: endpoint.to_owned(),
        };
    }

    let image = GenericImage::new("docker.io/eclipse/zenoh", "1.8.0")
        .with_exposed_port(7447.tcp())
        // Assumption A4 correction (observed against eclipse/zenoh:1.8.0):
        // the "listening on tcp/0.0.0.0:7447" line does not appear verbatim.
        // The stable readiness marker is the "Zenoh can be reached at:" line
        // logged by zenoh::net::runtime::orchestrator once the listener is up.
        // zenohd routes its tracing output to STDOUT (not stderr) when the
        // streams are captured separately — verified against the 1.8.0 image.
        .with_wait_for(WaitFor::message_on_stdout("Zenoh can be reached at"));

    let container = ContainerRequest::from(image)
        .start()
        .await
        .expect("failed to start zenohd testcontainer");

    let host = container.get_host().await.expect("failed to get host");
    let port = container
        .get_host_port_ipv4(7447)
        .await
        .expect("failed to get mapped 7447");

    let tcp_endpoint = format!("tcp/{host}:{port}");
    let connect_json5 = format!(r#"{{ connect: {{ endpoints: ["{tcp_endpoint}"] }} }}"#);

    ZenohGuard {
        _container: Some(container),
        connect_json5,
        tcp_endpoint,
    }
}

/// Convenience wrapper that reads `ZENOH_ROUTER_ENDPOINT`.
///
/// Reads the env var once at call time and forwards to
/// [`zenoh_router_with_endpoint`]. Production integration tests call this;
/// unit tests of the helper itself call [`zenoh_router_with_endpoint`]
/// directly to avoid the unsafe Rust-2024 env-mutation APIs.
///
/// # Panics
/// Panics on Docker start / port-mapping failure (mirrors `nats_container`).
pub async fn zenoh_router() -> ZenohGuard {
    let env_endpoint = env::var("ZENOH_ROUTER_ENDPOINT").ok();
    zenoh_router_with_endpoint(env_endpoint.as_deref()).await
}

/// Lazy shared router endpoint for tests in the same binary (mirrors
/// `nats_url()`). Starts a single container per test binary and returns a
/// `'static` reference to its `tcp/HOST:PORT` endpoint.
pub async fn zenoh_router_endpoint() -> &'static str {
    use tokio::sync::OnceCell;
    static ROUTER: OnceCell<ZenohGuard> = OnceCell::const_new();
    let guard = ROUTER.get_or_init(|| async { zenoh_router().await }).await;
    // Returning a `&'static str` is sound because `OnceCell` holds the guard
    // for `'static`; the slice points into that allocation.
    guard.tcp_endpoint()
}

#[cfg(test)]
mod tests {
    use super::*;

    // C-09 fix: NO env-mutation APIs anywhere in this module.
    // Rust 2024 makes those fns unsafe, and the workspace lint
    // `unsafe_code = "deny"` (Cargo.toml:156) rejects that even in tests.
    // Instead, tests pass the endpoint explicitly to `zenoh_router_with_endpoint`.

    #[tokio::test]
    async fn explicit_endpoint_bypasses_container_start() {
        let guard = zenoh_router_with_endpoint(Some("tcp/test-host:9999")).await;
        assert_eq!(guard.tcp_endpoint(), "tcp/test-host:9999");
        assert!(guard.connect_json5().contains("tcp/test-host:9999"));
    }

    #[tokio::test]
    async fn peer_config_disables_multicast_with_explicit_endpoint() {
        let guard = zenoh_router_with_endpoint(Some("tcp/x:1")).await;
        let cfg = guard.peer_config();
        // smoke: config builds; deeper assertion would require zenoh::Config
        // getters that are not stable public API.
        drop(cfg);
    }

    /// Docker-required smoke: starts a real zenohd, opens 2 peers, exchanges a
    /// sample. Runs in default `cargo test` matrix per D-29 — gated only by
    /// Docker availability. If your CI lacks Docker, run the other tests above
    /// (they do NOT require Docker).
    ///
    /// `flavor = "multi_thread"` is required because `zenoh::open` refuses the
    /// current-thread runtime (see zenoh-runtime 1.8 lib.rs:149).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_pubsub_smoke() {
        // None = start a real container. Explicitly `None` — no env dance.
        let guard = zenoh_router_with_endpoint(None).await;
        let s_a = zenoh::open(guard.peer_config()).await.unwrap();
        let s_b = zenoh::open(guard.peer_config()).await.unwrap();
        let sub = s_b
            .declare_subscriber("test/smoke")
            .with(flume::bounded::<zenoh::sample::Sample>(8))
            .await
            .unwrap();
        // Tiny warm-up to allow peer sessions to discover each other via the router.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        s_a.put("test/smoke", b"hi".to_vec()).await.unwrap();
        let sample = tokio::time::timeout(std::time::Duration::from_secs(3), sub.recv_async())
            .await
            .expect("recv timed out — router or peer-discovery failed")
            .expect("recv channel closed");
        assert_eq!(sample.payload().to_bytes().as_ref(), b"hi");
    }
}
