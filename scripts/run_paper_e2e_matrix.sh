#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="all"
WITH_MANIPULATOR=0

for arg in "$@"; do
  case "$arg" in
    --paper-only)
      MODE="paper"
      ;;
    --deterministic-only)
      MODE="deterministic"
      ;;
    --with-manipulator)
      WITH_MANIPULATOR=1
      ;;
    --help)
  cat <<'EOF'
run_paper_e2e_matrix.sh

Runs the live Roz + Copper + WASM container matrix used for paper-grade validation.

Usage:
  scripts/run_paper_e2e_matrix.sh
  scripts/run_paper_e2e_matrix.sh --paper-only
  scripts/run_paper_e2e_matrix.sh --deterministic-only
  scripts/run_paper_e2e_matrix.sh --paper-only --with-manipulator

Environment:
  ANTHROPIC_API_KEY  Required for the authored-WAT paper tests
EOF
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 1
      ;;
  esac
done

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required" >&2
  exit 1
fi

if ! docker info >/dev/null 2>&1; then
  echo "docker daemon is not reachable" >&2
  exit 1
fi

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

reset_container() {
  local name="$1"
  shift

  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --rm --name "$name" "$@" >/dev/null

  local deadline=$((SECONDS + 120))
  while (( SECONDS < deadline )); do
    local status
    status="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$name" 2>/dev/null || true)"
    if [[ "$status" == "healthy" || "$status" == "running" ]]; then
      return 0
    fi
    sleep 2
  done

  echo "container $name did not become ready" >&2
  docker logs "$name" >&2 || true
  exit 1
}

run_paper() {
  if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "ANTHROPIC_API_KEY is required for paper-grade authored-WAT tests" >&2
    exit 1
  fi

  run cargo test -p roz-local --test live_claude_wasm -- --ignored --nocapture
  run cargo test -p roz-local --test live_claude_wasm_containers -- --ignored --nocapture
  if [[ "$WITH_MANIPULATOR" -eq 1 ]]; then
    run cargo test -p roz-local --test live_claude_wasm_gazebo full_vertical_claude_wasm_gazebo -- --ignored --nocapture
  else
    printf '\n==> skipping manipulator paper test (pass --with-manipulator when a known-good ros2-manipulator sim is already running on 8094/9094)\n'
  fi
}

run_deterministic() {
  reset_container roz-test-nav2 \
    -p 9096:9090 -p 8096:8090 \
    -e ROBOT_MODEL=turtlebot3_waffle \
    -e GZ_WORLD=turtlebot3_world \
    -e USE_SLAM=true \
    bedrockdynamics/substrate-sim:ros2-nav2
  run cargo test -p roz-copper --test mobile_wasm_cmd_vel mobile_wasm_cmd_vel_through_bridge -- --ignored --nocapture

  reset_container roz-test-px4 \
    -p 9090:9090 -p 14540:14540/udp -p 14550:14550/udp \
    bedrockdynamics/substrate-sim:px4-gazebo-humble
  run cargo test -p roz-copper --test drone_wasm_velocity drone_wasm_velocity_through_bridge -- --ignored --nocapture

  reset_container roz-test-ardu \
    -p 9097:9090 -p 8098:8090 -p 14551:14550/udp \
    bedrockdynamics/substrate-sim:ardupilot-gazebo
  run cargo test -p roz-copper --test ardupilot_wasm_velocity ardupilot_wasm_velocity_through_bridge -- --ignored --nocapture
}

case "$MODE" in
  paper)
    run_paper
    ;;
  deterministic)
    run_deterministic
    ;;
  all)
    run_paper
    run_deterministic
    ;;
esac
