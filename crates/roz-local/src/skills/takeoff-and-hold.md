---
name: takeoff-and-hold
description: Take off to a specified altitude and hold position
kind: ai
version: "1.0.0"
tags: [drone, flight, basic]
parameters:
  - name: altitude
    param_type: float
    required: true
    default: null
    range: [1.0, 100.0]
safety:
  max_velocity: 3.0
  require_confirmation: true
environment_constraints:
  - Flight controller must be connected
  - Vehicle must be disarmed on the ground
stream_requirements:
  - telemetry
success_criteria:
  - Vehicle reaches target altitude within 2m tolerance
  - Vehicle holds position with < 1m drift for 5 seconds
allowed_tools: []
---

You are commanding a drone to take off and hold at a specified altitude.

## Procedure:
1. Verify the vehicle is disarmed and on the ground
2. Check that GPS lock is adequate (>= 6 satellites)
3. Set flight mode to GUIDED (PX4: OFFBOARD)
4. Arm the vehicle
5. Command takeoff to the specified altitude
6. Monitor altitude during ascent — abort if climb rate exceeds safety limits
7. Once at altitude, confirm position hold is stable
8. Report final position (lat, lon, alt) and hold status

## Safety:
- If altitude parameter exceeds 50m, warn the operator and request confirmation
- If battery drops below 30% during ascent, abort and land
- If GPS lock degrades to < 4 satellites, abort and land
- Monitor for EKF errors during the entire maneuver

## Abort procedure:
If any safety condition triggers, immediately command RTL (Return to Launch).
Report the reason for abort clearly.
