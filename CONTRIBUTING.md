# Contributing to Roz

Thank you for your interest in contributing to Roz!

## Getting Started

```bash
git clone https://github.com/BedrockDynamics/roz.git
cd roz
git config core.hooksPath .githooks
cargo build --workspace
cargo test --workspace --exclude roz-db --exclude roz-server
```

## Development Workflow

1. Fork the repository and create a feature branch
2. Make your changes following the conventions below
3. Run `cargo fmt --check && cargo clippy --workspace -- -D warnings`
4. Run `cargo test --workspace --exclude roz-db --exclude roz-server`
5. Submit a pull request

## Conventions

- **Edition 2024** with `rust-version = "1.92.0"`
- **Clippy pedantic + nursery** enabled, denied in CI
- **`unsafe` is denied** workspace-wide (safety-critical robotics platform)
- **Line width** 120 chars (`.rustfmt.toml`)
- All domain types in `roz-core` with `Serialize`/`Deserialize` derives
- Error handling via `thiserror` for libraries, `anyhow` sparingly for binaries

## Backend integrations

PRs that add or change a vendor backend (MAVLink, Gazebo, Spot, Franka, ROS2, or any new robot family) must cite `docs/integration-policy.md` in the PR description and justify the native-vs-bridge choice against the rubric documented there.

New backends are evaluated per the rubric in `docs/integration-policy.md`: Rust bindings availability, copper's 10 ms non-blocking tick compatibility, and vendor timing requirements.

## Testing

- `cargo test --workspace --exclude roz-db --exclude roz-server` for local tests
- Tests requiring Docker sim containers are marked `#[ignore]`
- Database tests (`roz-db`) require a Postgres instance

## License

By contributing, you agree that your contributions will be licensed under the Apache-2.0 License.
