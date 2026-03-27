use serde::{Deserialize, Serialize};

use crate::env_config::{EnvironmentConfig, EnvironmentKind};

/// Lightweight host descriptor used for matching — avoids a dependency on roz-db.
/// Callers construct this from `HostRow` or any other source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub status: String,
    pub host_type: String,
    pub capabilities: Vec<String>,
    pub active_task_count: u32,
}

/// Find the best matching online host for an environment configuration.
///
/// Filtering rules:
/// 1. Host must be `"online"`
/// 2. Host type must match the environment kind:
///    - `Simulation` → `"cloud"`
///    - `Hardware`   → `"edge"`
///    - `Hybrid`     → any host type
/// 3. Host must have all capabilities listed in `hardware.required_capabilities`
///
/// Among qualifying hosts the one with the fewest active tasks is preferred.
pub fn find_matching_host<'a>(env: &EnvironmentConfig, hosts: &'a [HostInfo]) -> Option<&'a HostInfo> {
    hosts
        .iter()
        .filter(|h| h.status == "online")
        .filter(|h| match env.kind {
            EnvironmentKind::Simulation => h.host_type == "cloud",
            EnvironmentKind::Hardware => h.host_type == "edge",
            EnvironmentKind::Hybrid => true,
        })
        .filter(|h| {
            env.hardware
                .as_ref()
                .is_none_or(|hw| hw.required_capabilities.iter().all(|cap| h.capabilities.contains(cap)))
        })
        .min_by_key(|h| h.active_task_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim_env() -> EnvironmentConfig {
        EnvironmentConfig::from_yaml(
            r#"
name: sim
kind: simulation
simulation:
  engine: gazebo
"#,
        )
        .unwrap()
    }

    fn hw_env() -> EnvironmentConfig {
        EnvironmentConfig::from_yaml(
            r#"
name: factory
kind: hardware
hardware:
  required_capabilities:
    - gpu
    - ros2
"#,
        )
        .unwrap()
    }

    fn hybrid_env() -> EnvironmentConfig {
        EnvironmentConfig::from_yaml(
            r#"
name: mixed
kind: hybrid
"#,
        )
        .unwrap()
    }

    fn host(status: &str, host_type: &str, caps: &[&str], tasks: u32) -> HostInfo {
        HostInfo {
            status: status.to_string(),
            host_type: host_type.to_string(),
            capabilities: caps.iter().map(|s| (*s).to_string()).collect(),
            active_task_count: tasks,
        }
    }

    #[test]
    fn simulation_env_matches_cloud_host() {
        let hosts = vec![host("online", "cloud", &[], 0), host("online", "edge", &[], 0)];
        let matched = find_matching_host(&sim_env(), &hosts).unwrap();
        assert_eq!(matched.host_type, "cloud");
    }

    #[test]
    fn hardware_env_matches_edge_host_with_capabilities() {
        let hosts = vec![
            host("online", "cloud", &["gpu", "ros2"], 0),
            host("online", "edge", &["gpu", "ros2"], 0),
        ];
        let matched = find_matching_host(&hw_env(), &hosts).unwrap();
        assert_eq!(matched.host_type, "edge");
    }

    #[test]
    fn missing_capabilities_skips_host() {
        let hosts = vec![
            host("online", "edge", &["gpu"], 0), // missing ros2
        ];
        assert!(find_matching_host(&hw_env(), &hosts).is_none());
    }

    #[test]
    fn no_matching_host_returns_none() {
        let hosts = vec![
            host("offline", "cloud", &[], 0), // not online
            host("online", "edge", &[], 0),   // wrong type for sim
        ];
        assert!(find_matching_host(&sim_env(), &hosts).is_none());
    }

    #[test]
    fn least_loaded_host_preferred() {
        let hosts = vec![
            host("online", "cloud", &[], 5),
            host("online", "cloud", &[], 1),
            host("online", "cloud", &[], 3),
        ];
        let matched = find_matching_host(&sim_env(), &hosts).unwrap();
        assert_eq!(matched.active_task_count, 1);
    }

    #[test]
    fn hybrid_matches_any_host_type() {
        let hosts = vec![host("online", "cloud", &[], 2), host("online", "edge", &[], 0)];
        let matched = find_matching_host(&hybrid_env(), &hosts).unwrap();
        // edge has fewer tasks, so it wins
        assert_eq!(matched.host_type, "edge");
        assert_eq!(matched.active_task_count, 0);
    }

    #[test]
    fn empty_hosts_returns_none() {
        assert!(find_matching_host(&sim_env(), &[]).is_none());
    }
}
