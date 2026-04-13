//! Toxiproxy testcontainer harness, mirroring `zenoh.rs` and `nats.rs`.
//!
//! Brings up `ghcr.io/shopify/toxiproxy:2.12.0` and returns a handle plus a
//! connected [`noxious_client::Client`] for programmatic toxic injection.
//!
//! Used by Wave 2 chaos tests (plan 16-06) to inject userland TCP faults
//! (latency, bandwidth, timeout) between workers and zenohd per locked
//! decision D-01.
//!
//! See 16-RESEARCH §1 for the full toxiproxy + noxious-client background.

use noxious_client::Client;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ContainerRequest, GenericImage};

/// Guard that holds a running toxiproxy container plus a client connected to
/// its admin API. The container is stopped and removed when this guard is
/// dropped.
pub struct ToxiproxyGuard {
    _container: ContainerAsync<GenericImage>,
    /// Connected admin-API client. Use [`Client::populate`] / [`Client::proxy`]
    /// to create and manipulate proxies at runtime.
    pub client: Client,
    admin_url: String,
    host: String,
    /// Mapped host port for the in-container 8666 proxy listener. Tests point
    /// workers at `tcp://{host}:{proxy_listener_host_port}` so traffic passes
    /// through toxiproxy before reaching the upstream (e.g. zenohd).
    pub proxy_listener_host_port: u16,
}

impl ToxiproxyGuard {
    /// Base URL of the toxiproxy admin API (`http://{host}:{admin_port}`).
    #[must_use]
    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }

    /// Host where the container is reachable (typically `localhost` /
    /// `127.0.0.1` — container runtime dependent).
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }
}

/// Start a fresh toxiproxy testcontainer exposing:
///   - `8474/tcp` — admin / control REST API
///   - `8666/tcp` — conventional proxy listener (tests populate a proxy
///     binding `0.0.0.0:8666` → upstream)
///
/// Image is pinned to `ghcr.io/shopify/toxiproxy:2.12.0` per 16-RESEARCH §1.
///
/// # Panics
/// Panics on Docker start or port-mapping failure (mirrors `nats_container`
/// and `zenoh_router` — integration-test helpers treat Docker unavailability
/// as a fatal environment problem, not a runtime error).
pub async fn toxiproxy_container() -> ToxiproxyGuard {
    // Readiness marker: the 2.12.0 image logs a startup banner containing
    // "Starting Toxiproxy" before the admin API binds. If this marker stops
    // appearing in a future image version, switch to an observed substring
    // analogous to the 15-03 A4 deviation for zenoh.
    let image = GenericImage::new("ghcr.io/shopify/toxiproxy", "2.12.0")
        .with_exposed_port(8474.tcp())
        .with_exposed_port(8666.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Starting Toxiproxy"));

    let container = ContainerRequest::from(image)
        .start()
        .await
        .expect("failed to start toxiproxy testcontainer");

    let host = container
        .get_host()
        .await
        .expect("failed to get toxiproxy container host")
        .to_string();
    let admin_port = container
        .get_host_port_ipv4(8474)
        .await
        .expect("failed to get mapped 8474");
    let proxy_listener_host_port = container
        .get_host_port_ipv4(8666)
        .await
        .expect("failed to get mapped 8666");

    let admin_url = format!("http://{host}:{admin_port}");
    let client = Client::new(&admin_url);

    ToxiproxyGuard {
        _container: container,
        client,
        admin_url,
        host,
        proxy_listener_host_port,
    }
}
