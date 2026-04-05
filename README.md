<h1 align="center">
  <img src="extra/images/logo.png" width=200 height=200/><br>
  roz
</h1>

<h4 align="center">the robotics claw</h4>

<p align="center">
  An open-source physical AI agent runtime. An LLM agent that writes WASM code to control real robots at 100Hz.
</p>

> **Research Preview** — This is an early release for research and experimentation. APIs, interfaces, and safety guarantees are under active development and may change. Not recommended for production deployment on physical hardware without additional safety validation.

## Quick Start

```bash
cargo install roz-cli
roz sim start manipulator    # Requires Docker
roz                          # Interactive mode
> wave the arm back and forth
```

## How It Works

1. You describe what the robot should do
2. The agent reasons about the task (Claude, GPT-4, Gemini, or local Ollama)
3. For simple moves: calls MCP tools (move_to_pose, navigate_to, takeoff)
4. For complex control: writes WASM code that runs at 100Hz in a safety sandbox
5. Safety filter clamps all outputs (velocity, acceleration, position limits)
6. Sensor feedback streams back for closed-loop control

## Two Control Paths

**Path A — High-level MCP tools (1-3Hz)**
The agent calls tools like `move_to_pose`, `navigate_to`, `takeoff` via MCP. Each Docker sim container bundles its own MCP server with robot-specific tools.

**Path B — WASM controllers (100Hz)**
The agent writes WAT code that runs in a wasmtime sandbox at 100Hz. The WASM controller reads sensor state and writes motor commands through a robot-agnostic channel interface.

## Supported Robots

| Type | Sim Container | MCP Tools | WASM Channels |
|------|--------------|-----------|---------------|
| Manipulator (UR5) | `ros2-manipulator` | move_to_pose, get_joint_state, stop_arm | 6 joint velocities |
| Drone (PX4) | `px4-gazebo-humble` | takeoff, land, go_to | 4 body velocities |
| Drone (ArduPilot) | `ardupilot-gazebo` | arm, takeoff, go_to | 4 body velocities |
| Mobile (Nav2) | `ros2-nav2` | navigate_to, follow_waypoints | 2 twist components |

## Architecture

```
User -> roz CLI -> Agent (LLM)
                      |--> MCP tools -> Docker sim -> MoveIt2/Nav2/MAVLink
                      `--> WASM code -> Copper 100Hz -> Safety filter -> Bridge -> Gazebo
```

## Crate Map

| Crate | Purpose |
|-------|---------|
| `roz-core` | Domain types, channel manifests, no IO |
| `roz-agent` | Agent loop, safety guards, tool dispatch, model abstraction |
| `roz-copper` | WASM sandbox, 100Hz controller loop, IO traits, safety filter |
| `roz-local` | Docker launcher, MCP client, skill registry, local runtime |
| `roz-cli` | Interactive TUI, sim commands |
| `roz-safety` | Safety daemon (heartbeat monitoring, e-stop) |
| `roz-nats` | NATS client wrappers |
| `roz-zenoh` | Zenoh transport layer |

## Development

```bash
# Build
cargo build --workspace

# Test
cargo test --workspace

# Integration tests (require Docker sim containers)
cargo test --workspace -- --ignored --nocapture
```

## Paper-Grade E2E Matrix

The strongest live validation path now uses real LLM-authored raw WAT, real `promote_controller`, real Copper activation, and real observed motion in each supported sim container.

For mobile, PX4, and ArduPilot, the script recreates fresh sim containers before running the tests so the results are not polluted by stale vehicle state.

Run the full matrix:

```bash
export ANTHROPIC_API_KEY=...
scripts/run_paper_e2e_matrix.sh
```

Useful subsets:

```bash
scripts/run_paper_e2e_matrix.sh --paper-only
scripts/run_paper_e2e_matrix.sh --deterministic-only
scripts/run_paper_e2e_matrix.sh --paper-only --with-manipulator
```

The paper-grade authored-WAT suite covers:

- mobile, PX4, and ArduPilot via [live_claude_wasm_containers.rs](crates/roz-local/tests/live_claude_wasm_containers.rs)
- manipulator via [live_claude_wasm_gazebo.rs](crates/roz-local/tests/live_claude_wasm_gazebo.rs)
  This remains opt-in in the script and expects a known-good `ros2-manipulator` sim already running on `8094/9094`.

The deterministic Copper/WASM bridge suite covers:

- [mobile_wasm_cmd_vel.rs](crates/roz-copper/tests/mobile_wasm_cmd_vel.rs)
- [drone_wasm_velocity.rs](crates/roz-copper/tests/drone_wasm_velocity.rs)
- [ardupilot_wasm_velocity.rs](crates/roz-copper/tests/ardupilot_wasm_velocity.rs)

## Status

This is a **research preview**. The following are proven end-to-end with real infrastructure:

- LLM writes WASM, deploys to 100Hz loop, UR5 arm moves at commanded velocity
- LLM writes WASM, deploys, PX4 drone takes off and flies at commanded velocity
- Multi-turn conversations with real sensor feedback
- Safety filter clamps velocity, acceleration, and position limits
- Agent delegates spatial reasoning to vision models

Known limitations:

- Safety guarantees are simulation-validated only — additional validation required for physical hardware
- WASM channel interface covers manipulators and drones; legged robots not yet supported
- Cloud features (multi-tenant API, team coordination) are available separately

## License

Apache-2.0
