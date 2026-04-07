//! Docker container management for PX4 SITL simulation.
//!
//! Uses the `docker` CLI via `std::process::Command` (no bollard crate),
//! matching the proven approach from Substrate's `docker_launcher`.
//!
//! Security constraints:
//! - `--network=bridge` (no host networking)
//! - No `--privileged` flag
//! - Project directory mounted read-only
//! - Container label: `roz.managed=true`

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

// ---- Error type ----

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("Docker is not available — is Docker Desktop running?")]
    NotAvailable,
    #[error("Docker image not found: {0}")]
    ImageNotFound(String),
    #[error("container launch failed: {0}")]
    LaunchFailed(String),
    #[error("container not found: {0}")]
    NotFound(String),
    #[error("port unavailable: {0}")]
    PortUnavailable(String),
    #[error("health check timed out after {0:?}")]
    HealthTimeout(Duration),
}

// ---- Configuration ----

/// Default Docker image for PX4 SITL simulation.
pub const DEFAULT_SIM_IMAGE: &str = "bedrockdynamics/substrate-sim:px4-gazebo-humble";

/// Container label key for roz-managed containers.
pub const MANAGED_LABEL: &str = "roz.managed=true";

/// Configuration for a Docker simulation container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimContainerConfig {
    /// Docker image (default: `bedrockdynamics/substrate-sim:px4-gazebo-humble`).
    #[serde(default = "default_image")]
    pub image: String,
    /// PX4 vehicle model (e.g. "x500", "`rc_cessna`").
    #[serde(default = "default_px4_model")]
    pub px4_model: String,
    /// Gazebo world name (e.g. "default", "baylands").
    #[serde(default = "default_px4_world")]
    pub px4_world: String,
    /// CPU limit (e.g. "4").
    pub cpu_limit: Option<String>,
    /// Memory limit (e.g. "4G").
    pub memory_limit: Option<String>,
}

fn default_image() -> String {
    DEFAULT_SIM_IMAGE.to_string()
}
fn default_px4_model() -> String {
    "x500".to_string()
}
fn default_px4_world() -> String {
    "default".to_string()
}

impl Default for SimContainerConfig {
    fn default() -> Self {
        Self {
            image: default_image(),
            px4_model: default_px4_model(),
            px4_world: default_px4_world(),
            cpu_limit: Some("4".to_string()),
            memory_limit: Some("4G".to_string()),
        }
    }
}

/// Runtime state for a launched container.
#[derive(Debug, Clone)]
pub struct ContainerInstance {
    pub id: String,
    pub container_id: String,
    pub container_name: String,
    pub mavlink_port: u16,
    pub bridge_port: u16,
    pub mcp_port: u16,
    pub config: SimContainerConfig,
    pub started_at: Instant,
}

impl ContainerInstance {
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

// ---- Docker command helpers ----

/// Create a `docker` command. On Windows, prevents console popups.
fn docker_command() -> Command {
    let cmd = Command::new("docker");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = cmd;
        cmd.creation_flags(0x0800_0000);
        return cmd;
    }
    #[cfg(not(target_os = "windows"))]
    cmd
}

/// Remove stale roz-managed containers (created/exited state).
fn cleanup_stale_containers() {
    let output = docker_command()
        .args([
            "ps",
            "-a",
            "--filter",
            "label=roz.managed=true",
            "--filter",
            "status=created",
            "--filter",
            "status=exited",
            "--format",
            "{{.ID}}",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    if let Ok(output) = output {
        let ids = String::from_utf8_lossy(&output.stdout);
        for id in ids.lines().filter(|l| !l.is_empty()) {
            tracing::info!("Removing stale roz container: {id}");
            let _ = docker_command().args(["rm", "-f", id]).output();
        }
    }
}

/// Resolve the random host port Docker published for `container_port`.
fn published_host_port(container_ref: &str, container_port: &str) -> Result<u16, DockerError> {
    let output = docker_command()
        .args(["port", container_ref, container_port])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| DockerError::LaunchFailed(format!("docker port failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DockerError::LaunchFailed(format!(
            "docker port {container_port} failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| DockerError::LaunchFailed(format!("docker port {container_port} returned no mapping")))?;

    let port = line
        .trim()
        .rsplit(':')
        .next()
        .ok_or_else(|| DockerError::LaunchFailed(format!("unexpected docker port output: {line}")))?
        .parse::<u16>()
        .map_err(|e| DockerError::LaunchFailed(format!("failed to parse docker port output '{line}': {e}")))?;

    Ok(port)
}

// ---- DockerLauncher ----

pub struct DockerLauncher {
    instances: Mutex<HashMap<String, ContainerInstance>>,
    instance_counter: Mutex<u32>,
}

impl DockerLauncher {
    pub fn new() -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
            instance_counter: Mutex::new(0),
        }
    }

    /// Check whether the `docker` CLI is available and the daemon is running.
    pub fn is_available(&self) -> bool {
        docker_command()
            .args(["info"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Ensure the image exists locally, pulling if necessary.
    fn ensure_image(image: &str) -> Result<(), DockerError> {
        let check = docker_command()
            .args(["image", "inspect", image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if check.map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }

        tracing::info!("Pulling Docker image: {image}");
        let pull = docker_command()
            .args(["pull", image])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| DockerError::LaunchFailed(format!("docker pull failed: {e}")))?;

        if pull.success() {
            Ok(())
        } else {
            Err(DockerError::ImageNotFound(image.to_string()))
        }
    }

    /// Launch a simulation container.
    ///
    /// Security: `--network=bridge`, no `--privileged`, project dir mounted read-only.
    pub fn launch(&self, config: SimContainerConfig, project_dir: &Path) -> Result<ContainerInstance, DockerError> {
        if !self.is_available() {
            return Err(DockerError::NotAvailable);
        }

        cleanup_stale_containers();
        Self::ensure_image(&config.image)?;

        let counter_val = {
            let mut counter = self.instance_counter.lock();
            *counter += 1;
            *counter
        };
        let instance_id = format!("roz-sim-{counter_val}");

        let container_name = format!("roz-sim-{counter_val}-{}", uuid::Uuid::new_v4().simple());

        // Build docker run arguments
        let project_mount = format!("{}:/workspace:ro", project_dir.to_string_lossy());

        let mut args = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container_name.clone(),
            "--network".into(),
            "bridge".into(),
            "--label".into(),
            MANAGED_LABEL.into(),
            "--label".into(),
            format!("roz.instance_id={instance_id}"),
            "-p".into(),
            "127.0.0.1::14540/udp".into(),
            "-p".into(),
            "127.0.0.1::9090".into(),
            "-p".into(),
            "127.0.0.1::8090".into(),
            "-v".into(),
            project_mount,
            "-e".into(),
            format!("PX4_SIM_MODEL={}", config.px4_model),
            "-e".into(),
            format!("PX4_GZ_WORLD={}", config.px4_world),
        ];

        if let Some(ref cpu) = config.cpu_limit {
            args.extend(["--cpus".into(), cpu.clone()]);
        }
        if let Some(ref mem) = config.memory_limit {
            args.extend(["-m".into(), mem.clone()]);
        }

        args.push(config.image.clone());

        let output = docker_command()
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| DockerError::LaunchFailed(format!("failed to run docker: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DockerError::LaunchFailed(format!("docker run failed: {stderr}")));
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let mavlink_port = published_host_port(&container_name, "14540/udp")?;
        let bridge_port = published_host_port(&container_name, "9090/tcp")?;
        let mcp_port = published_host_port(&container_name, "8090/tcp")?;

        let instance = ContainerInstance {
            id: instance_id.clone(),
            container_id,
            container_name,
            mavlink_port,
            bridge_port,
            mcp_port,
            config,
            started_at: Instant::now(),
        };

        self.instances.lock().insert(instance_id, instance.clone());

        tracing::info!(
            "Launched container {} (MAVLink:{}, Bridge:{}, MCP:{})",
            instance.container_name,
            mavlink_port,
            bridge_port,
            mcp_port,
        );

        Ok(instance)
    }

    /// Stop and remove a running container.
    pub fn stop(&self, instance_id: &str) -> Result<(), DockerError> {
        let instance = self
            .instances
            .lock()
            .remove(instance_id)
            .ok_or_else(|| DockerError::NotFound(instance_id.to_string()))?;

        tracing::info!("Stopping container: {}", instance.container_name);

        let _ = docker_command()
            .args(["stop", "-t", "5", &instance.container_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let _ = docker_command()
            .args(["rm", "-f", &instance.container_id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        Ok(())
    }

    /// Stop all running containers.
    pub fn stop_all(&self) {
        let ids: Vec<String> = self.instances.lock().keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    /// List all active instances.
    pub fn list(&self) -> Vec<ContainerInstance> {
        self.instances.lock().values().cloned().collect()
    }

    /// Wait for a container's MCP port to become reachable (TCP connect).
    pub fn wait_healthy(&self, instance_id: &str, timeout: Duration) -> Result<(), DockerError> {
        let instance = {
            let instances = self.instances.lock();
            instances
                .get(instance_id)
                .cloned()
                .ok_or_else(|| DockerError::NotFound(instance_id.to_string()))?
        };

        let deadline = Instant::now() + timeout;
        let mut delay = Duration::from_millis(500);

        while Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", instance.mcp_port).parse().unwrap(),
                Duration::from_secs(2),
            )
            .is_ok()
            {
                return Ok(());
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }

        Err(DockerError::HealthTimeout(timeout))
    }
}

impl Default for DockerLauncher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_host_port_parses_ipv4_output() {
        let line = "127.0.0.1:32768";
        let port = line.trim().rsplit(':').next().unwrap().parse::<u16>().unwrap();
        assert_eq!(port, 32768);
    }

    #[test]
    fn published_host_port_parses_ipv6_output() {
        let line = "[::1]:32769";
        let port = line.trim().rsplit(':').next().unwrap().parse::<u16>().unwrap();
        assert_eq!(port, 32769);
    }

    #[test]
    fn default_config_has_expected_values() {
        let config = SimContainerConfig::default();
        assert_eq!(config.image, DEFAULT_SIM_IMAGE);
        assert_eq!(config.px4_model, "x500");
        assert_eq!(config.px4_world, "default");
    }

    #[test]
    fn container_instance_uptime() {
        let inst = ContainerInstance {
            id: "test".into(),
            container_id: "abc123".into(),
            container_name: "roz-sim-1".into(),
            mavlink_port: 14540,
            bridge_port: 9090,
            mcp_port: 8090,
            config: SimContainerConfig::default(),
            started_at: Instant::now(),
        };
        assert!(inst.uptime_secs() < 2);
    }

    #[test]
    fn launcher_is_available_returns_bool() {
        let launcher = DockerLauncher::new();
        let _ = launcher.is_available();
    }

    #[test]
    fn stop_unknown_instance_returns_not_found() {
        let launcher = DockerLauncher::new();
        let result = launcher.stop("nonexistent-123");
        assert!(matches!(result, Err(DockerError::NotFound(_))));
    }

    #[test]
    fn list_empty_by_default() {
        let launcher = DockerLauncher::new();
        assert!(launcher.list().is_empty());
    }
}
