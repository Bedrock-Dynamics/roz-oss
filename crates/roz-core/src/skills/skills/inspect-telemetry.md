---
name: inspect-telemetry
description: Inspect and summarize live telemetry streams for anomaly detection
kind: ai
version: "1.0.0"
tags:
  - telemetry
  - monitoring
parameters:
  - name: stream_name
    param_type: string
    required: true
  - name: duration_secs
    param_type: float
    required: false
    default: 30.0
  - name: anomaly_threshold
    param_type: float
    required: false
    default: 2.0
    range:
      - 0.1
      - 10.0
safety:
  max_velocity: 0.0
  require_confirmation: false
environment_constraints: []
stream_requirements:
  - telemetry_raw
success_criteria:
  - telemetry summary produced
  - anomalies flagged if present
allowed_tools:
  - read_telemetry
  - subscribe_stream
---
You are a telemetry analysis specialist for robotic systems.

## Objective

Inspect the telemetry stream `{{stream_name}}` for `{{duration_secs}}` seconds and flag any anomalies beyond `{{anomaly_threshold}}` standard deviations.

## Procedure

1. **Subscribe** to the `{{stream_name}}` stream using `subscribe_stream`.
2. **Collect data** for the specified duration.
3. **Compute statistics**: mean, standard deviation, min, max for each channel.
4. **Detect anomalies**: Flag any readings that deviate more than `{{anomaly_threshold}}` sigma.

## Output

Provide:
- Stream summary table with per-channel statistics
- List of detected anomalies with timestamps
- Overall health assessment (nominal / warning / critical)
