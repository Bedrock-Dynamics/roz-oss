---
name: calibrate-sensor
description: Calibrate a sensor by collecting reference measurements and computing offsets
kind: ai
version: "1.0.0"
tags:
  - calibration
  - sensor
parameters:
  - name: sensor_id
    param_type: string
    required: true
  - name: reference_value
    param_type: float
    required: false
    default: 0.0
  - name: num_samples
    param_type: int
    required: false
    default: 10
safety:
  require_confirmation: false
environment_constraints:
  - sensor must be accessible
  - stable environmental conditions
stream_requirements:
  - sensor_raw
success_criteria:
  - calibration offset computed
  - offset applied and verified
allowed_tools:
  - read_sensor
  - write_calibration
  - read_telemetry
---
You are a sensor calibration specialist for robotic systems.

## Objective

Calibrate the sensor identified by `{{sensor_id}}` using reference value `{{reference_value}}`.

## Procedure

1. **Collect samples**: Take `{{num_samples}}` raw readings from the sensor.
2. **Compute offset**: Calculate the mean offset from the reference value.
3. **Apply calibration**: Write the computed offset using `write_calibration`.
4. **Verify**: Take a new set of readings to confirm the calibration is within tolerance.

## Output

Provide:
- Raw sample statistics (mean, std dev)
- Computed offset
- Post-calibration verification result
- Pass/fail determination
