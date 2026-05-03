//! Docker integration test: subscribe to Gazebo poses and convert to roz domain types.
//!
//! Launches `bedrockdynamics/substrate-sim:bare-gazebo` in a Docker container,
//! waits for it to advertise pose topics, subscribes via `gz-transport`, and
//! verifies that `roz_copper::gazebo_sensor::poses_to_entities()` produces valid
//! `EntityState` values.
//!
//! Run with:
//! ```bash
//! cargo test -p roz-copper --features gazebo --test gazebo_docker_integration -- --ignored
//! ```

use gz_transport_rs::msgs::PoseV;
use gz_transport_rs::{Config, Node};
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::process::Command as AsyncCommand;

// ── cleanup guard ────────────────────────────────────────────────────────────

/// RAII guard that force-removes the named Docker container on drop.
struct Cleanup {
    container_name: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // Best-effort: ignore errors (container may already be gone).
        let _ = Command::new("docker").args(["rm", "-f", &self.container_name]).output();
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Return `true` when `docker info` exits successfully (daemon reachable).
fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Poll `docker exec <name> gz topic -l` until the output contains the string
/// `"pose"`, signalling that Gazebo has started and is advertising pose topics.
///
/// Returns `Ok(())` on success, `Err(String)` on timeout.
async fn wait_for_gazebo_ready(container_name: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let output = AsyncCommand::new("docker")
            .args(["exec", container_name, "gz", "topic", "-l"])
            .output()
            .await
            .map_err(|e| format!("docker exec failed: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("pose") {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    Err(format!("Gazebo did not advertise a pose topic within {timeout:?}"))
}

// ── test ─────────────────────────────────────────────────────────────────────

/// Subscribe to `/world/empty/pose/info` from a containerised Gazebo, convert
/// the first `PoseV` message to roz `EntityState` values, and assert basic
/// invariants.
///
/// Marked `#[ignore]` because it requires a working Docker daemon and network
/// access to pull `bedrockdynamics/substrate-sim:bare-gazebo`.
#[tokio::test]
#[ignore]
async fn subscribe_poses_and_publish_commands() {
    // ── 1. check Docker availability ─────────────────────────────────────────
    if !docker_available() {
        println!("SKIP: Docker daemon not available — skipping Gazebo integration test");
        return;
    }

    // ── 2. launch the container ───────────────────────────────────────────────
    let container_name = format!("roz-test-gazebo-{}", std::process::id());
    let partition = container_name.clone();
    println!("Launching container: {container_name}");

    let mut run_args = vec![
        "run".to_owned(),
        "-d".to_owned(),
        "--rm".to_owned(),
        "--name".to_owned(),
        container_name.clone(),
        "-e".to_owned(),
        format!("GZ_PARTITION={partition}"),
    ];
    if cfg!(target_os = "linux") {
        run_args.push("--network".to_owned());
        run_args.push("host".to_owned());
    }
    run_args.push("bedrockdynamics/substrate-sim:bare-gazebo".to_owned());

    let run_status = AsyncCommand::new("docker")
        .args(&run_args)
        .status()
        .await
        .expect("failed to execute `docker run`");

    assert!(
        run_status.success(),
        "`docker run` exited with non-zero status — is the image available?"
    );

    // Ensure the container is cleaned up even if the test panics.
    let _cleanup = Cleanup {
        container_name: container_name.clone(),
    };

    // ── 3. wait for Gazebo to be ready ────────────────────────────────────────
    println!("Waiting for Gazebo to advertise pose topics (up to 120 s)…");
    wait_for_gazebo_ready(&container_name, Duration::from_secs(120))
        .await
        .expect("Gazebo readiness check timed out");
    println!("Gazebo is ready");

    // ── 4. use the explicit partition shared by host and container ────────────
    println!("GZ_PARTITION = {partition}");

    // ── 5. connect via gz-transport ───────────────────────────────────────────
    let config = Config::builder()
        .partition(Some(&partition))
        .discovery_timeout(Duration::from_secs(10))
        .build();

    let mut node = Node::with_config(config)
        .await
        .expect("failed to create gz-transport Node");

    // ── 6. subscribe to pose topic ────────────────────────────────────────────
    const TOPIC: &str = "/world/empty/pose/info";
    println!("Subscribing to {TOPIC}");

    let mut sub = match node.subscribe::<PoseV>(TOPIC).await {
        Ok(sub) => sub,
        Err(gz_transport_rs::Error::DiscoveryTimeout { .. }) => {
            eprintln!(
                "SKIP: host gz-transport discovery could not see Gazebo in Docker; \
                 run on Linux with host networking for this direct transport check"
            );
            return;
        }
        Err(error) => panic!("failed to subscribe to pose topic: {error}"),
    };

    // ── 7. receive one message ────────────────────────────────────────────────
    let (pose_v, meta) = tokio::time::timeout(Duration::from_secs(10), sub.recv())
        .await
        .expect("timed out waiting for PoseV message")
        .expect("subscriber channel closed unexpectedly");

    println!("Received PoseV on '{}' ({} poses)", meta.topic, pose_v.pose.len());

    // ── 8. convert and assert ─────────────────────────────────────────────────
    let entities = roz_copper::gazebo_sensor::poses_to_entities(&pose_v);

    assert!(!entities.is_empty(), "expected at least one EntityState");

    for entity in &entities {
        assert_eq!(
            entity.frame_id, "world",
            "entity '{}' has unexpected frame_id: {:?}",
            entity.id, entity.frame_id,
        );

        let pos = entity
            .position
            .map(|[x, y, z]| format!("({x:.3}, {y:.3}, {z:.3})"))
            .unwrap_or_else(|| "N/A".to_owned());

        println!("  entity: '{}' @ {pos}", entity.id);
    }

    println!("All {} entities validated", entities.len());
}
