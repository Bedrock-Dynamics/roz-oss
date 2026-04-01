# Reachy Mini

Pollen Robotics Reachy Mini wireless — a tabletop expressive robot with a 6-DOF Stewart platform head, body rotation, and two antenna actuators.

## Hardware
- **Head**: 6-DOF Stewart platform (pitch/roll ±40°, yaw ±180°)
- **Body**: 1-DOF rotation (yaw ±160°)
- **Antennas**: 2 actuators (0° to ~120°), also function as physical buttons
- **Camera**: Pi Camera V3 Wide (12MP, 120° FOV)
- **Mic array**: 4x MEMS with Direction of Arrival
- **IMU**: Bosch BMI088 (accelerometer + gyroscope + orientation)

## Tools

All angle parameters are in **radians**. Use `get_robot_state` to check current state before commanding motion.

### Observe
- `get_robot_state` — returns head pose (x,y,z,roll,pitch,yaw), body yaw, antennas, motor status

### Move
- `set_motors(mode)` — "enabled", "disabled", or "gravity_compensation"
- `move_to(channels, duration_secs)` — smooth interpolated motion to target channel positions
- `play_animation(name)` — built-in animations: "wake_up", "goto_sleep"

### State Response Shape
`get_robot_state` returns JSON:
- `head_pose`: {x, y, z, roll, pitch, yaw} in meters/radians
- `body_yaw`: radians
- `antennas_position`: [right, left] in radians
- `control_mode`: "enabled" | "disabled" | "gravity_compensation"
- `timestamp`: ISO 8601

## Motion Sequencing
- `move_to` returns immediately — the motion takes `duration_secs` to complete
- Before issuing a follow-up motion, call `get_robot_state` to verify the previous one finished
- For chained motions: move_to → wait → get_robot_state → verify → next move_to

## Safety
- Cable constraint: |head_yaw - body_yaw| must be ≤ 65° (1.13 rad)
- Always call `set_motors(mode="enabled")` before commanding motion
- `set_motors(mode="disabled")` = emergency stop (robot goes limp, safe)
- The robot is 1.2 kg — motors off (limp) is always safe
- The robot is in **simulation mode** unless otherwise stated
