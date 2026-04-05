#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="all"

for arg in "$@"; do
  case "$arg" in
    --authored-only)
      MODE="authored"
      ;;
    --deterministic-only)
      MODE="deterministic"
      ;;
    --with-manipulator)
      ;;
    --skip-manipulator)
      export ROZ_SKIP_MANIPULATOR_LIVE_TEST=1
      ;;
    --help)
  cat <<'EOF'
run_live_e2e_matrix.sh

Runs the live Roz + Copper + WASM container matrix against the supported sim containers.

Usage:
  scripts/run_live_e2e_matrix.sh
  scripts/run_live_e2e_matrix.sh --authored-only
  scripts/run_live_e2e_matrix.sh --deterministic-only
  scripts/run_live_e2e_matrix.sh --authored-only --skip-manipulator

Environment:
  ANTHROPIC_API_KEY  Required for the authored-WAT live tests
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
  local timeout_secs="$2"
  shift
  shift

  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --rm --name "$name" "$@" >/dev/null

  local has_healthcheck
  has_healthcheck="$(docker inspect -f '{{if .State.Health}}yes{{else}}no{{end}}' "$name" 2>/dev/null || true)"

  local deadline=$((SECONDS + timeout_secs))
  while (( SECONDS < deadline )); do
    local status
    status="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$name" 2>/dev/null || true)"
    if [[ "$has_healthcheck" == "yes" && "$status" == "healthy" ]]; then
      return 0
    fi
    if [[ "$has_healthcheck" != "yes" && "$status" == "running" ]]; then
      return 0
    fi
    if [[ "$status" == "unhealthy" ]]; then
      echo "container $name became unhealthy" >&2
      docker logs "$name" >&2 || true
      exit 1
    fi
    sleep 2
  done

  echo "container $name did not become ready" >&2
  docker logs "$name" >&2 || true
  exit 1
}

run_authored() {
  if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "ANTHROPIC_API_KEY is required for authored-WAT live tests" >&2
    exit 1
  fi

  run cargo test -p roz-local --test live_claude_wasm -- --ignored --nocapture
  run cargo test -p roz-local --test live_claude_wasm_containers -- --ignored --nocapture
  if [[ "${ROZ_SKIP_MANIPULATOR_LIVE_TEST:-0}" == "1" ]]; then
    printf '\n==> skipping manipulator authored-WAT live test because ROZ_SKIP_MANIPULATOR_LIVE_TEST=1\n'
  else
    run cargo test -p roz-local --test live_claude_wasm_gazebo full_vertical_claude_wasm_gazebo -- --ignored --nocapture
  fi
}

run_deterministic() {
  reset_container roz-test-nav2 240 \
    -p 9096:9090 -p 8096:8090 \
    -e ROBOT_MODEL=turtlebot3_waffle \
    -e GZ_WORLD=turtlebot3_world \
    -e USE_SLAM=true \
    -e ROS_LOCALHOST_ONLY=1 \
    -e ROS_DOMAIN_ID=41 \
    bedrockdynamics/substrate-sim:ros2-nav2
  run cargo test -p roz-copper --test mobile_wasm_cmd_vel mobile_wasm_cmd_vel_through_bridge -- --ignored --nocapture

  reset_container roz-test-px4 120 \
    -p 9090:9090 -p 14540:14540/udp -p 14550:14550/udp \
    bedrockdynamics/substrate-sim:px4-gazebo-humble
  run cargo test -p roz-copper --test drone_wasm_velocity drone_wasm_velocity_through_bridge -- --ignored --nocapture

  reset_container roz-test-ardu 120 \
    -p 9097:9090 -p 8098:8090 -p 14551:14550/udp \
    bedrockdynamics/substrate-sim:ardupilot-gazebo
  run cargo test -p roz-copper --test ardupilot_wasm_velocity ardupilot_wasm_velocity_through_bridge -- --ignored --nocapture
}

case "$MODE" in
  authored)
    run_authored
    ;;
  deterministic)
    run_deterministic
    ;;
  all)
    run_authored
    run_deterministic
    ;;
esac
