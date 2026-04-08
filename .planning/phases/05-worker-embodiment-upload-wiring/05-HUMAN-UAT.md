---
status: partial
phase: 05-worker-embodiment-upload-wiring
source: [05-VERIFICATION.md]
started: 2026-04-08T18:30:00Z
updated: 2026-04-08T18:30:00Z
---

## Current Test

[awaiting human testing]

## Tests

### 1. End-to-end upload flow
expected: Start worker with ROZ_ROBOT_TOML pointing to valid robot.toml; log line "embodiment model uploaded" appears; server DB has embodiment_model populated for the host
result: [pending]

### 2. Conditional skip on restart
expected: Restart same worker without changing manifest; second startup logs "embodiment model uploaded" but server returns 204 (digest match, no write)
result: [pending]

### 3. No-config skip path
expected: Start worker without ROZ_ROBOT_TOML set; no embodiment upload attempt; no upload-related log lines
result: [pending]

## Summary

total: 3
passed: 0
issues: 0
pending: 3
skipped: 0
blocked: 0

## Gaps
