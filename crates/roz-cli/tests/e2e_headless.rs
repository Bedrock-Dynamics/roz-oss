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
    roz_headless_in_with_env(task, timeout, None, &[])
}

/// Run `roz --non-interactive --task <task>` in a specific directory.
fn roz_headless_in(task: &str, timeout: Duration, working_dir: Option<&std::path::Path>) -> HeadlessResult {
    roz_headless_in_with_env(task, timeout, working_dir, &[])
}

/// Run `roz --non-interactive --task <task>` in a specific directory with
/// explicit environment overrides.
fn roz_headless_in_with_env(
    task: &str,
    timeout: Duration,
    working_dir: Option<&std::path::Path>,
    envs: &[(&str, &str)],
) -> HeadlessResult {
    let mut cmd = Command::new(cargo_bin("roz"));
    cmd.args(["--non-interactive", "--task", task]);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }

    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn roz binary");

    // Capture the PID before moving the child into the thread so we can
    // kill it on timeout without `unsafe` (workspace denies unsafe).
    let pid = child.id();

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
            // Kill the subprocess via `kill -TERM <pid>` to avoid leaking it.
            let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
            return HeadlessResult {
                success: false,
                stdout: String::new(),
                stderr: format!("process timed out after {}s", timeout.as_secs()),
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Run `roz` against an explicitly provided cloud deployment using only env
/// overrides, not any persisted local auth state.
fn roz_headless_with_cloud_override(task: &str, timeout: Duration, api_url: &str, api_key: &str) -> HeadlessResult {
    let home = tempfile::TempDir::new().expect("create temp HOME");
    roz_headless_in_with_env(
        task,
        timeout,
        None,
        &[
            ("HOME", home.path().to_str().expect("HOME path should be valid UTF-8")),
            ("ROZ_API_URL", api_url),
            ("ROZ_API_KEY", api_key),
            ("ROZ_PROFILE", "dev-e2e-override"),
        ],
    )
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

/// The headless CLI must honor `ROZ_API_URL` and `ROZ_API_KEY` environment
/// overrides so it can be pointed at dev without relying on local auth state
/// or the default production endpoint.
#[test]
#[ignore = "requires ROZ_API_URL + ROZ_API_KEY for a live cloud deployment"]
fn headless_respects_api_url_override() {
    let api_url = std::env::var("ROZ_API_URL").expect("ROZ_API_URL must be set for this test");
    let api_key = std::env::var("ROZ_API_KEY").expect("ROZ_API_KEY must be set for this test");
    let result = roz_headless_with_cloud_override(
        "Say hello in exactly 3 words.",
        Duration::from_secs(30),
        &api_url,
        &api_key,
    );
    assert!(
        result.success,
        "roz should exit successfully when pointed at an overridden cloud URL.\nstderr: {}",
        result.stderr
    );

    let json = parse_output(&result);
    assert_eq!(json["status"], "success", "status should be success: {json}");
    assert!(
        json["response"].as_str().is_some_and(|s| !s.is_empty()),
        "response should contain text: {json}"
    );
}

/// The dev cloud override should still permit client-side bash execution.
#[test]
#[ignore = "requires ROZ_API_URL + ROZ_API_KEY for a live cloud deployment"]
fn headless_cloud_bash_tool_respects_api_url_override() {
    let api_url = std::env::var("ROZ_API_URL").expect("ROZ_API_URL must be set for this test");
    let api_key = std::env::var("ROZ_API_KEY").expect("ROZ_API_KEY must be set for this test");

    let result = roz_headless_with_cloud_override(
        "Use the bash tool to run 'echo roz_e2e_override_marker'. Report the exact output.",
        Duration::from_secs(45),
        &api_url,
        &api_key,
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();
    assert!(
        response.contains("roz_e2e_override_marker"),
        "agent should report the bash output containing our override marker.\nGot: {}",
        json["response"]
    );
    let cycles = json["cycles"].as_u64().unwrap_or(0);
    assert!(cycles >= 1, "expected at least one tool cycle, got {cycles}");
}

/// The dev cloud override should still permit client-side file reads with no
/// dependence on persisted auth state.
#[test]
#[ignore = "requires ROZ_API_URL + ROZ_API_KEY for a live cloud deployment"]
fn headless_cloud_read_file_respects_api_url_override() {
    let api_url = std::env::var("ROZ_API_URL").expect("ROZ_API_URL must be set for this test");
    let api_key = std::env::var("ROZ_API_KEY").expect("ROZ_API_KEY must be set for this test");

    let dir = tempfile::TempDir::new().expect("create temp dir");
    let test_file = dir.path().join("roz_override_test_file.txt");
    std::fs::write(&test_file, "roz_override_file_content_67890").expect("write temp file");

    let task = format!(
        "Use the read_file tool to read the file at '{}' and tell me its exact contents.",
        test_file.display()
    );
    let result = roz_headless_with_cloud_override(&task, Duration::from_secs(45), &api_url, &api_key);
    assert!(result.success, "stderr: {}", result.stderr);

    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        response.contains("roz_override_file_content_67890"),
        "agent should report the file contents.\nGot: {}",
        json["response"]
    );
}

/// The dev cloud override should preserve multi-step cloud execution and return
/// both requested outputs in a single headless session.
#[test]
#[ignore = "requires ROZ_API_URL + ROZ_API_KEY for a live cloud deployment"]
fn headless_multi_step_bash_respects_api_url_override() {
    let api_url = std::env::var("ROZ_API_URL").expect("ROZ_API_URL must be set for this test");
    let api_key = std::env::var("ROZ_API_KEY").expect("ROZ_API_KEY must be set for this test");

    let result = roz_headless_with_cloud_override(
        "You must invoke the bash tool exactly twice. The first tool call may only run 'echo override_step1_done'. \
         The second tool call may only run 'echo override_step2_done'. After the second tool call, report both outputs.",
        Duration::from_secs(60),
        &api_url,
        &api_key,
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();
    assert!(
        response.contains("override_step1_done") && response.contains("override_step2_done"),
        "agent should report both bash outputs.\nGot: {}",
        json["response"]
    );
    let cycles = json["cycles"].as_u64().unwrap_or(0);
    assert!(cycles >= 1, "expected at least one agent cycle, got {cycles}");
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

// ---------------------------------------------------------------------------
// Client-side tool execution tests (no daemon needed)
// ---------------------------------------------------------------------------

/// Cloud agent executes bash tool client-side.
/// This is the key regression test for client-side tool execution.
#[test]
#[ignore = "requires authenticated roz CLI"]
fn headless_cloud_bash_tool() {
    let result = roz_headless(
        "Use the bash tool to run 'echo roz_e2e_test_marker'. Report the exact output.",
        Duration::from_secs(45),
    );
    assert!(result.success, "stderr: {}", result.stderr);
    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();
    assert!(
        response.contains("roz_e2e_test_marker"),
        "agent should report the bash output containing our marker.\nGot: {}",
        json["response"]
    );
}

/// Cloud agent reads a file via `read_file` tool executed client-side.
#[test]
#[ignore = "requires authenticated roz CLI"]
fn headless_cloud_read_file() {
    // Create a temp file with known content
    let dir = tempfile::TempDir::new().unwrap();
    let test_file = dir.path().join("roz_test_file.txt");
    std::fs::write(&test_file, "roz_file_content_12345").unwrap();

    let task = format!(
        "Use the read_file tool to read the file at '{}' and tell me its exact contents.",
        test_file.display()
    );
    let result = roz_headless(&task, Duration::from_secs(45));
    assert!(result.success, "stderr: {}", result.stderr);
    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        response.contains("roz_file_content_12345"),
        "agent should report the file contents.\nGot: {}",
        json["response"]
    );
}

/// Cloud agent handles tool errors gracefully (doesn't hang).
#[test]
#[ignore = "requires authenticated roz CLI"]
fn headless_tool_error_handled() {
    let result = roz_headless(
        "Use the read_file tool to read '/nonexistent/roz_e2e_test_path.txt'. Report what happened.",
        Duration::from_secs(45),
    );
    assert!(
        result.success,
        "roz should exit successfully even on tool errors.\nstderr: {}",
        result.stderr
    );
    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();
    assert!(
        response.contains("error")
            || response.contains("not found")
            || response.contains("no such")
            || response.contains("does not exist"),
        "agent should report the file error.\nGot: {}",
        json["response"]
    );
}

/// Cloud agent can chain multiple tool calls in one session.
#[test]
#[ignore = "requires authenticated roz CLI"]
fn headless_multiple_tool_calls() {
    let result = roz_headless(
        "First use bash to run 'echo step1_done', then use bash to run 'echo step2_done'. \
         Report both outputs.",
        Duration::from_secs(60),
    );
    assert!(result.success, "stderr: {}", result.stderr);
    let json = parse_output(&result);
    let response = json["response"].as_str().unwrap_or("").to_lowercase();
    assert!(
        response.contains("step1_done") && response.contains("step2_done"),
        "agent should report both bash outputs.\nGot: {}",
        json["response"]
    );
    let cycles = json["cycles"].as_u64().unwrap_or(0);
    assert!(
        cycles >= 2,
        "should have at least 2 cycles for 2 tool calls, got {cycles}"
    );
}

/// Verify the timeout mechanism works -- a long-running task gets killed.
#[test]
#[ignore = "requires authenticated roz CLI"]
fn headless_timeout_works() {
    // 1-second timeout is too short for any cloud round-trip (gRPC handshake
    // alone takes longer), making this deterministic regardless of prompt.
    let result = roz_headless("Say hello", Duration::from_secs(1));
    assert!(!result.success, "should fail with 1s timeout");
    assert!(
        result.stderr.contains("timed out"),
        "stderr should mention timeout.\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr
    );
}
