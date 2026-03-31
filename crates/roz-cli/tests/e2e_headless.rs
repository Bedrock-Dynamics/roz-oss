//! End-to-end headless CLI tests.
//!
//! These run `roz --non-interactive --task "..."` as a subprocess and verify
//! the full pipeline: cloud agent -> tool request -> local execution -> result
//! -> agent response.
//!
//! Requirements:
//!   - Authenticated roz CLI (`roz auth login`)
//!   - For daemon tool tests: Reachy Mini sim on localhost:8000
//!
//! Run:
//! ```bash
//! cargo test -p roz-cli --test e2e_headless -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Outcome from running `roz` in headless mode.
struct HeadlessResult {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run `roz --non-interactive --task <task>` with a timeout.
///
/// Uses a poll loop with `try_wait` since `Command::timeout()` is not
/// stabilized in our pinned Rust 1.92.0.
fn roz_headless(task: &str, timeout: Duration) -> HeadlessResult {
    roz_headless_in(task, timeout, None)
}

/// Run `roz --non-interactive --task <task>` in a specific directory.
fn roz_headless_in(task: &str, timeout: Duration, working_dir: Option<&std::path::Path>) -> HeadlessResult {
    let mut cmd = Command::new(cargo_bin("roz"));
    cmd.args(["--non-interactive", "--task", task]);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn roz binary");

    // Run wait_with_output on a dedicated thread so we can enforce a timeout.
    let join_handle = std::thread::spawn(move || child.wait_with_output());

    let start = Instant::now();
    loop {
        if join_handle.is_finished() {
            let output = join_handle
                .join()
                .expect("child thread panicked")
                .expect("failed to collect roz output");
            return HeadlessResult {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            };
        }
        if start.elapsed() >= timeout {
            // The thread holds the child -- we cannot kill it from here without
            // unsafe. Instead, report the timeout and let the thread be leaked
            // (it will be cleaned up when the test process exits).
            return HeadlessResult {
                success: false,
                stdout: String::new(),
                stderr: format!("process timed out after {}s", timeout.as_secs()),
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Resolve the path to a cargo-built binary.
///
/// Uses `CARGO_BIN_EXE_roz` (set by cargo for integration tests when a
/// `[[bin]]` target exists) or falls back to `target/debug/<name>`.
fn cargo_bin(name: &str) -> PathBuf {
    // cargo sets CARGO_BIN_EXE_<name> for integration tests
    if let Ok(path) = std::env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(path);
    }

    // Fallback: look in target/debug
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("target");
    path.push("debug");
    path.push(name);
    path
}

/// Parse the headless JSON output, returning the parsed value.
///
/// Panics with a descriptive message if stdout is not valid JSON.
fn parse_output(result: &HeadlessResult) -> serde_json::Value {
    serde_json::from_str(&result.stdout).unwrap_or_else(|e| {
        panic!(
            "non-interactive output should be valid JSON.\n\
             Parse error: {e}\n\
             stdout: {}\n\
             stderr: {}",
            result.stdout, result.stderr
        );
    })
}

/// Path to the reachy-mini example directory.
fn reachy_mini_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("examples");
    path.push("reachy-mini");
    path
}

// ---------------------------------------------------------------------------
// Basic cloud tests (no daemon needed)
// ---------------------------------------------------------------------------

/// Non-interactive mode should output valid JSON with the expected structure.
#[test]
#[ignore = "requires authenticated roz CLI"]
fn headless_json_output_structure() {
    let result = roz_headless("What is 2+2? Answer with just the number.", Duration::from_secs(30));
    assert!(
        result.success,
        "roz should exit successfully.\nstderr: {}",
        result.stderr
    );
    assert!(!result.stdout.is_empty(), "stdout should not be empty");

    let json = parse_output(&result);
    assert_eq!(json["status"], "success", "status should be 'success': {json}");
    assert!(json.get("response").is_some(), "should have 'response' field: {json}");
    assert!(json.get("usage").is_some(), "should have 'usage' field: {json}");
    assert!(json.get("cycles").is_some(), "should have 'cycles' field: {json}");
}

/// Basic prompt that requires no tools -- verifies the cloud round-trip works.
#[test]
#[ignore = "requires authenticated roz CLI"]
fn headless_basic_prompt() {
    let result = roz_headless("Say hello in exactly 3 words.", Duration::from_secs(30));
    assert!(
        result.success,
        "roz should exit successfully.\nstderr: {}",
        result.stderr
    );

    let json = parse_output(&result);
    let response = json["response"].as_str().expect("response should be a string");
    assert!(!response.is_empty(), "response should contain text: {json}");
}

// ---------------------------------------------------------------------------
// Tool discovery tests (no daemon needed, just robot.toml)
// ---------------------------------------------------------------------------

/// When run from a directory with robot.toml, the agent should discover and
/// list the daemon tools registered from the manifest.
#[test]
#[ignore = "requires authenticated roz CLI"]
fn headless_discovers_robot_tools() {
    let dir = reachy_mini_dir();
    assert!(
        dir.join("robot.toml").exists(),
        "reachy-mini/robot.toml should exist at {}",
        dir.display()
    );

    let result = roz_headless_in(
        "List all available tools you have. Just list their names, nothing else.",
        Duration::from_secs(45),
        Some(&dir),
    );
    assert!(
        result.success,
        "roz should exit successfully.\nstderr: {}",
        result.stderr
    );

    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();

    // The agent should mention at least some of the daemon tools from robot.toml.
    assert!(
        response.contains("get_robot_state")
            || response.contains("move_to")
            || response.contains("play_animation")
            || response.contains("set_motors"),
        "agent should list daemon tools from robot.toml.\n\
         Expected one of: get_robot_state, move_to, play_animation, set_motors\n\
         Got response: {}",
        json["response"]
    );
}

// ---------------------------------------------------------------------------
// Daemon tool execution tests (need running Reachy Mini sim)
// ---------------------------------------------------------------------------

/// Full pipeline test: cloud agent calls `get_robot_state`, tool executes locally
/// against the daemon, result flows back through cloud, agent reports the state.
///
/// This is the test that would have caught the "tools only displayed, never
/// executed" bug.
#[test]
#[ignore = "requires authenticated roz CLI + Reachy Mini sim on localhost:8000"]
fn headless_get_robot_state() {
    let dir = reachy_mini_dir();
    let result = roz_headless_in(
        "Use the get_robot_state tool to check the robot state. Report the head pose values.",
        Duration::from_secs(60),
        Some(&dir),
    );
    assert!(
        result.success,
        "roz should exit successfully.\nstderr: {}",
        result.stderr
    );

    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();

    // The agent should have actually called get_robot_state and reported real values.
    // If tools were only displayed but never executed, the response would be generic.
    assert!(
        response.contains("head")
            || response.contains("pose")
            || response.contains("pitch")
            || response.contains("roll")
            || response.contains("yaw")
            || response.contains("orientation"),
        "agent should mention head pose data from get_robot_state.\n\
         Got response: {}",
        json["response"]
    );

    // Verify at least one tool cycle executed.
    let cycles = json["cycles"].as_u64().unwrap_or(0);
    assert!(
        cycles >= 1,
        "should have at least 1 agent cycle (tool use + response), got {cycles}"
    );
}

/// Full pipeline: enable motors, then `move_to` to tilt the head.
#[test]
#[ignore = "requires authenticated roz CLI + Reachy Mini sim on localhost:8000"]
fn headless_move_to() {
    let dir = reachy_mini_dir();
    let result = roz_headless_in(
        "Enable motors with set_motors, then use move_to to tilt the head pitch to 0.2 radians \
         over 1 second. Report what you did.",
        Duration::from_secs(60),
        Some(&dir),
    );
    assert!(
        result.success,
        "roz should exit successfully.\nstderr: {}",
        result.stderr
    );

    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();

    assert!(
        response.contains("move")
            || response.contains("pitch")
            || response.contains("motor")
            || response.contains("motion")
            || response.contains("tilt"),
        "agent should describe the motion it performed.\n\
         Got response: {}",
        json["response"]
    );
}

/// Full pipeline: play an animation via the daemon.
#[test]
#[ignore = "requires authenticated roz CLI + Reachy Mini sim on localhost:8000"]
fn headless_play_animation() {
    let dir = reachy_mini_dir();
    let result = roz_headless_in(
        "Play the wake_up animation using the play_animation tool.",
        Duration::from_secs(60),
        Some(&dir),
    );
    assert!(
        result.success,
        "roz should exit successfully.\nstderr: {}",
        result.stderr
    );

    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();

    assert!(
        response.contains("wake")
            || response.contains("animation")
            || response.contains("played")
            || response.contains("play"),
        "agent should confirm animation was played.\n\
         Got response: {}",
        json["response"]
    );
}
