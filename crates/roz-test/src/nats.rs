use std::env;

use testcontainers::GenericImage;
use testcontainers::ImageExt;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;

const NATS_CLIENT_PORT: ContainerPort = ContainerPort::Tcp(4222);

fn reserve_host_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve host port")
        .local_addr()
        .expect("reserved port local addr")
        .port()
}

/// Guard that holds a running NATS container. The container is stopped and
/// removed when this guard is dropped.
pub struct NatsGuard {
    _container: Option<testcontainers::ContainerAsync<GenericImage>>,
    url: String,
    container_name: String,
}

impl NatsGuard {
    /// Connection URL for the running NATS instance.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Docker container id/name for network-chaos tests.
    pub fn container_name(&self) -> &str {
        &self.container_name
    }
}

/// Starts a fresh NATS testcontainer (with `JetStream` enabled) and returns a
/// guard that owns it. The container is removed when the guard is dropped.
///
/// If `NATS_URL` is set, connects to the external instance instead.
pub async fn nats_container() -> NatsGuard {
    if let Ok(url) = env::var("NATS_URL") {
        return NatsGuard {
            _container: None,
            url,
            container_name: env::var("NATS_CONTAINER_NAME").unwrap_or_else(|_| "external-nats".to_string()),
        };
    }

    let host_port = reserve_host_port();
    let container_name = format!("roz-test-nats-{}", uuid::Uuid::new_v4());
    let container = GenericImage::new("nats", "2.10.14")
        .with_wait_for(WaitFor::message_on_stderr(
            "Listening for client connections on 0.0.0.0:4222",
        ))
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["--jetstream"])
        .with_container_name(&container_name)
        .with_mapped_port(host_port, NATS_CLIENT_PORT)
        .start()
        .await
        .expect("failed to start NATS testcontainer");

    let host = container.get_host().await.expect("failed to get host");
    let port = host_port;

    let url = format!("nats://{host}:{port}");

    NatsGuard {
        _container: Some(container),
        url,
        container_name,
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
