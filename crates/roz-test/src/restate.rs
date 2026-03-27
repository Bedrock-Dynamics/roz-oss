use std::env;

use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ContainerRequest, GenericImage, ImageExt};

/// Guard that holds a running Restate container. The container is stopped and
/// removed when this guard is dropped.
pub struct RestateGuard {
    _container: Option<ContainerAsync<GenericImage>>,
    url: String,
    admin_url: String,
}

impl RestateGuard {
    /// Ingress URL for the running Restate instance (port 8080).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Admin URL for the running Restate instance (port 9070).
    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }
}

/// Starts a fresh Restate testcontainer and returns a guard that owns it.
/// The container is removed when the guard is dropped.
///
/// If `RESTATE_URL` and `RESTATE_ADMIN_URL` are set, connects to the external
/// instance instead.
pub async fn restate_container() -> RestateGuard {
    if let (Ok(url), Ok(admin_url)) = (env::var("RESTATE_URL"), env::var("RESTATE_ADMIN_URL")) {
        return RestateGuard {
            _container: None,
            url,
            admin_url,
        };
    }

    let image = GenericImage::new("docker.io/restatedev/restate", "1.3")
        .with_exposed_port(8080.tcp())
        .with_exposed_port(9070.tcp())
        .with_wait_for(WaitFor::Http(Box::new(
            HttpWaitStrategy::new("/restate/health")
                .with_port(8080.tcp())
                .with_response_matcher(|res| res.status().is_success()),
        )))
        .with_wait_for(WaitFor::Http(Box::new(
            HttpWaitStrategy::new("/health")
                .with_port(9070.tcp())
                .with_response_matcher(|res| res.status().is_success()),
        )));

    let container = ContainerRequest::from(image)
        .with_host("host.docker.internal", testcontainers::core::Host::HostGateway)
        .start()
        .await
        .expect("failed to start Restate testcontainer");

    let host = container.get_host().await.expect("failed to get host");
    let ports = container.ports().await.expect("failed to get ports");

    let ingress_port = ports
        .map_to_host_port_ipv4(8080.tcp())
        .expect("failed to get ingress port");
    let admin_port = ports
        .map_to_host_port_ipv4(9070.tcp())
        .expect("failed to get admin port");

    let url = format!("http://{host}:{ingress_port}");
    let admin_url = format!("http://{host}:{admin_port}");

    RestateGuard {
        _container: Some(container),
        url,
        admin_url,
    }
}
