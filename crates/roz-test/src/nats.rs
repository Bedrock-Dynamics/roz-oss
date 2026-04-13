use std::env;

use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::nats::{Nats, NatsServerCmd};

/// Guard that holds a running NATS container. The container is stopped and
/// removed when this guard is dropped.
pub struct NatsGuard {
    _container: Option<testcontainers::ContainerAsync<Nats>>,
    url: String,
}

impl NatsGuard {
    /// Connection URL for the running NATS instance.
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Starts a fresh NATS testcontainer (with `JetStream` enabled) and returns a
/// guard that owns it. The container is removed when the guard is dropped.
///
/// If `NATS_URL` is set, connects to the external instance instead.
pub async fn nats_container() -> NatsGuard {
    if let Ok(url) = env::var("NATS_URL") {
        return NatsGuard { _container: None, url };
    }

    let cmd = NatsServerCmd::default().with_jetstream();
    let container = Nats::default()
        .with_cmd(&cmd)
        .start()
        .await
        .expect("failed to start NATS testcontainer");

    let host = container.get_host().await.expect("failed to get host");
    // Docker port-table publish intermittently races with testcontainers-rs
    // 0.27's `get_host_port_ipv4` check — retry a few times before giving up.
    // See also `toxiproxy.rs` where the same pattern applies to port 8666.
    let port = {
        let mut last_err: Option<testcontainers_modules::testcontainers::TestcontainersError> = None;
        let mut found: Option<u16> = None;
        for _ in 0..10 {
            match container.get_host_port_ipv4(4222).await {
                Ok(p) => {
                    found = Some(p);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }
        found.unwrap_or_else(|| panic!("failed to get host port after retries: {last_err:?}"))
    };

    let url = format!("nats://{host}:{port}");

    NatsGuard {
        _container: Some(container),
        url,
    }
}

/// Returns a NATS connection URL, starting a testcontainer if needed.
///
/// The container is held in a static `OnceCell` so all tests in the same
/// binary share a single NATS instance. The container is cleaned up when
/// the test process exits (the `OnceCell` is never re-initialized).
pub async fn nats_url() -> &'static str {
    use tokio::sync::OnceCell;

    static NATS: OnceCell<NatsGuard> = OnceCell::const_new();

    let guard = NATS.get_or_init(|| async { nats_container().await }).await;
    guard.url()
}
