<h1 align="center">
  <img src="extra/images/logo.png" width=200 height=200/><br>
  roz
</h1>

<p align="center">
  Open-source runtime and tooling for embodied AI agents.
</p>

> **Research Preview** — Roz is still evolving quickly. Interfaces, manifests, and safety behavior may change between releases.

## What Roz Is

Roz lets an agent work through two complementary control paths:

- High-level embodied tools through MCP and daemon-backed actions
- Low-level WebAssembly controllers executed through Copper at control-loop rate

The OSS repo is focused on local development, simulation, and the shared runtime crates behind those flows.

## Quick Start

```bash
cargo install --path crates/roz-cli
roz sim start manipulator
roz
```

Inside a project, Roz reads:

- `roz.toml` for project/model config
- `embodiment.toml` for robot or simulator embodiment config
- `AGENTS.md` for project-specific operating guidance

`robot.toml` is still accepted as a legacy fallback, but `embodiment.toml` is the canonical filename.

## Repo Layout

| Crate | Purpose |
|------|---------|
| `roz-core` | Shared domain types, manifests, embodiment/runtime primitives |
| `roz-agent` | Agent loop, prompt assembly, safety stack, model abstraction |
| `roz-copper` | WebAssembly controller runtime, safety filter, transport adapters |
| `roz-local` | Local runtime, Docker sim launcher, MCP integration |
| `roz-cli` | CLI and interactive TUI |
| `roz-server` | OSS REST and gRPC server surfaces |
| `roz-worker` | NATS worker runtime |
| `roz-db` | SQLx migrations and queries |
| `roz-nats` | Typed NATS subjects and payloads |

## Running Tests

Standard development checks:

```bash
cargo build --workspace
cargo test --workspace
```

Opt-in live and container-backed suites live under `crates/*/tests` and are marked `#[ignore]`. They require the relevant local infrastructure, credentials, or Docker images.

For the current live container matrix:

```bash
scripts/run_live_e2e_matrix.sh --help
```

## Examples

Examples under `examples/` show the expected project shape. In particular:

- `examples/reachy-mini/embodiment.toml` is the canonical Reachy Mini manifest
- `examples/reachy-mini/AGENTS.md` shows the expected embodied tool guidance shape

## Documentation

- [Integration policy](docs/integration-policy.md) — decision authority for native-vs-bridge backends (MAVLink, Gazebo, Spot, Franka, ROS2).

## Status

The OSS repo is aimed at:

- local runtime development
- shared embodied/runtime abstractions
- simulation and integration testing
- contributor-facing tooling

Private cloud deployment and operator workflow live outside this README.

## License

Apache-2.0
