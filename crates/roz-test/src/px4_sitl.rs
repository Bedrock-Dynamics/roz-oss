use std::env;
use std::time::Duration;

use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ContainerRequest, GenericImage, ImageExt, TestcontainersError};

const PX4_SITL_IMAGE: &str = "bedrockdynamics/substrate-sim";
const PX4_SITL_TAG: &str = "px4-gazebo-humble";
const BRIDGE_GRPC_PORT: u16 = 9090;
const MAVLINK_UDP_PORT: u16 = 14540;
const GCS_UDP_PORT: u16 = 14550;

/// Guard that holds a PX4 SITL + Gazebo + substrate bridge container.
///
/// If the `PX4_SITL_*` environment variables are provided, this guard points
/// at that external simulator instead and does not own a container.
///
/// The default container path is bridge-backed. Do not treat the returned
/// MAVLink ports as Roz's default native-FCU acceptance path; direct native
/// MAVLink diagnostics should require an explicit `PX4_SITL_MAVLINK_URL` or
/// `PX4_SITL_MAVLINK_PORT` so the test operator chooses the endpoint.
pub struct Px4SitlGuard {
    _container: Option<ContainerAsync<GenericImage>>,
    bridge_grpc_url: String,
    mavlink_udp_port: u16,
    gcs_udp_port: u16,
    container_name: String,
}

impl Px4SitlGuard {
    /// gRPC URL for the substrate simulator bridge inside the SITL stack.
    pub fn bridge_grpc_url(&self) -> &str {
        &self.bridge_grpc_url
    }

    /// Host UDP port that PX4 broadcasts MAVLink offboard traffic to.
    pub fn mavlink_udp_port(&self) -> u16 {
        self.mavlink_udp_port
    }

    /// Host UDP port that accepts QGroundControl/GCS-style MAVLink traffic.
    pub fn gcs_udp_port(&self) -> u16 {
        self.gcs_udp_port
    }

    /// Docker container name, or the caller-provided external simulator name.
    pub fn container_name(&self) -> &str {
        &self.container_name
    }
}

/// Starts a fresh PX4 SITL testcontainer and returns a guard that owns it.
/// The container is removed when the guard is dropped.
///
/// Set `PX4_SITL_BRIDGE_URL`, `PX4_SITL_MAVLINK_PORT`, and
/// `PX4_SITL_CONTAINER_NAME` to reuse an externally managed simulator.
pub async fn px4_sitl_container() -> Px4SitlGuard {
    if let (Ok(bridge_grpc_url), Ok(mavlink_udp_port), Ok(container_name)) = (
        env::var("PX4_SITL_BRIDGE_URL"),
        env::var("PX4_SITL_MAVLINK_PORT"),
        env::var("PX4_SITL_CONTAINER_NAME"),
    ) {
        let mavlink_udp_port = mavlink_udp_port.parse().expect("PX4_SITL_MAVLINK_PORT must be a u16");
        let gcs_udp_port = env::var("PX4_SITL_GCS_PORT")
            .ok()
            .map(|port| port.parse().expect("PX4_SITL_GCS_PORT must be a u16"))
            .unwrap_or(GCS_UDP_PORT);
        return Px4SitlGuard {
            _container: None,
            bridge_grpc_url,
            mavlink_udp_port,
            gcs_udp_port,
            container_name,
        };
    }

    let container_name = format!("roz-test-px4-sitl-{}", std::process::id());
    let mavlink_udp_port = reserve_udp_port();
    let image = GenericImage::new(PX4_SITL_IMAGE, PX4_SITL_TAG)
        .with_exposed_port(BRIDGE_GRPC_PORT.tcp())
        .with_exposed_port(MAVLINK_UDP_PORT.udp())
        .with_exposed_port(GCS_UDP_PORT.udp())
        .with_env_var("MAVLINK_OFFBOARD_PORT", mavlink_udp_port.to_string());

    let container = ContainerRequest::from(image)
        .with_container_name(&container_name)
        .start()
        .await
        .expect("failed to start PX4 SITL testcontainer");

    let host = container.get_host().await.expect("failed to get PX4 SITL host");
    let bridge_grpc_port = mapped_port_with_retry(&container, BRIDGE_GRPC_PORT.tcp()).await;
    let gcs_udp_port = mapped_port_with_retry(&container, GCS_UDP_PORT.udp()).await;
    let bridge_grpc_url = format!("http://{host}:{bridge_grpc_port}");

    Px4SitlGuard {
        _container: Some(container),
        bridge_grpc_url,
        mavlink_udp_port,
        gcs_udp_port,
        container_name,
    }
}

fn reserve_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve UDP port for PX4 MAVLink");
    let port = socket.local_addr().expect("reserved UDP socket local address").port();
    drop(socket);
    port
}

async fn mapped_port_with_retry(
    container: &ContainerAsync<GenericImage>,
    port: testcontainers::core::ContainerPort,
) -> u16 {
    let mut last_err: Option<TestcontainersError> = None;
    for _ in 0..10 {
        match container.get_host_port_ipv4(port).await {
            Ok(mapped) => return mapped,
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    panic!("failed to get PX4 SITL host port after retries: {last_err:?}");
}
