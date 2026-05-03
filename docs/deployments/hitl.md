# HITL Bench Deployment

This runbook describes the minimum bench needed to test Roz against a real
Pixhawk-class flight controller before any untethered flight.

## Hardware

- Pixhawk 6C or equivalent PX4-compatible controller
- Raspberry Pi 5 or similar Linux companion
- TELEM2-to-UART cable from Pixhawk to companion
- 5 V companion power independent from flight battery
- Battery-cutoff relay or power module with a physical kill switch
- Propellers removed for all bench tests
- Tether or fixed test stand before any motorized lift test
- QGroundControl laptop on the normal telemetry/GCS link

## Wiring

Use TELEM2 for the companion link:

| Pixhawk TELEM2 | Companion UART |
|----------------|----------------|
| TX             | RX             |
| RX             | TX             |
| GND            | GND            |

Do not power the companion from Pixhawk telemetry pins. Use a dedicated 5 V
regulator. Keep QGroundControl on its own telemetry/radio path.

## Flight Controller Setup

PX4 defaults are acceptable for the first bench pass:

- MAVLink 2 enabled on TELEM2
- TELEM2 baud: `921600`
- Companion/offboard mode enabled
- RC/manual control still available
- Failsafe action set to Land or Hold, never Continue

For SITL port behavior and companion IDs, see
[`docs/mavlink-coexistence.md`](../mavlink-coexistence.md).

## Safety Model

Use two independent stops:

1. Software stop: Roz sends a flight command that transitions the FCU to Land
   or another configured safe mode.
2. Hardware stop: a human-accessible battery cutoff relay or equivalent removes
   motor power.

Roz software safety is not a substitute for the hardware stop. The operator
must be able to cut motor power without relying on Wi-Fi, NATS, the companion,
or the flight controller accepting a command.

## Pre-Flight Checklist

Before any powered bench run:

- Propellers removed
- Battery physically restrained
- Hardware cutoff tested
- QGroundControl connected and showing stable heartbeat
- Pixhawk mode, arming state, and GPS/EKF state understood
- Companion can read the serial device
- Roz device enrollment completed
- Safety policy bound to the host
- MCAP export path verified after a dry run

Before a tethered lift:

- Propellers installed only after dry bench tests pass
- Tether inspected and anchored
- Kill switch operator assigned
- Takeoff altitude limited to the minimum useful height
- Test area clear

## Acceptance Record

The v3.0 milestone still needs a real bench acceptance record: controller
model, companion model, Pixhawk firmware version, Roz commit, exported MCAP,
and a screenshot or short video showing the MCAP replay in Foxglove.

Use `docs/deployments/v3-acceptance.md` for the full simulator, direct-MAVLink,
and hardware acceptance checklist.
