---
name: grid-survey
description: Plan and execute a grid survey pattern over a rectangular area
kind: ai
version: "1.0.0"
tags: [drone, mission, survey, mapping]
parameters:
  - name: area_width
    param_type: float
    required: true
    default: null
    range: [5.0, 1000.0]
  - name: area_length
    param_type: float
    required: true
    default: null
    range: [5.0, 1000.0]
  - name: altitude
    param_type: float
    required: true
    default: null
    range: [5.0, 120.0]
  - name: spacing
    param_type: float
    required: false
    default: 10.0
    range: [1.0, 50.0]
safety:
  max_velocity: 5.0
  require_confirmation: true
environment_constraints:
  - Flight controller must be connected
  - GPS lock must be available
stream_requirements:
  - telemetry
success_criteria:
  - All grid waypoints are visited
  - Coverage area matches the specified dimensions within 10%
  - Vehicle returns to launch point after survey
allowed_tools: []
---

You are planning and executing a grid survey (lawnmower pattern) over a rectangular area.

## Planning phase:
1. Calculate the grid waypoints based on area dimensions and spacing
2. Determine the optimal survey direction (minimize turns)
3. Calculate total flight distance and estimated flight time
4. Verify battery capacity is sufficient for the mission (add 30% safety margin)
5. Present the plan to the operator for approval

## Grid pattern:
- Start from the current position (home point)
- Fly parallel lines (legs) separated by `spacing` meters
- Alternate direction on each leg (lawnmower pattern)
- Maintain constant altitude throughout
- Include a return-to-launch waypoint at the end

## Execution phase:
1. Upload the waypoint mission to the flight controller
2. Arm and take off to survey altitude
3. Begin the survey pattern
4. Monitor battery and GPS throughout
5. On completion, return to launch and land

## Safety:
- If estimated flight time exceeds 80% of battery endurance, reduce the survey area and warn
- Abort if wind speed exceeds 8 m/s during survey
- If a waypoint is outside the configured geofence, skip it and warn
- Monitor camera/sensor status if applicable
