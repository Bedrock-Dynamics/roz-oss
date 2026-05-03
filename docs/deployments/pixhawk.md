# Pixhawk Quickstart

This quickstart gets a Linux-comfortable operator from a fresh checkout to
tethered bench-flight readiness with one Roz worker binary talking MAVLink
directly to Pixhawk.

## 1. Hardware

Use a Pixhawk 6C-class FCU, a Raspberry Pi 5-class companion, TELEM2-to-UART
cable, separate 5 V companion power, QGroundControl, a physical battery cutoff,
and a tethered stand. Remove propellers until the final tethered step.

Wire Pixhawk TELEM2 TX/RX/GND to the companion UART RX/TX/GND. Do not power the
companion from TELEM2.

## 2. Companion OS

Flash Ubuntu Server 22.04+ arm64. Then:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev protobuf-compiler
sudo usermod -aG dialout "$USER"
```

Log out and back in. Confirm the Pixhawk serial device:

```sh
ls -l /dev/ttyUSB* /dev/ttyAMA* 2>/dev/null
```

## 3. Install Roz Worker

```sh
git clone https://github.com/bedrock-dynamics/roz-oss.git
cd roz-oss
cargo build --release -p roz-worker
sudo install -m 0755 target/release/roz-worker /usr/local/bin/roz-worker
sudo mkdir -p /var/lib/roz
```

## 4. Configure MAVLink

Create `/etc/roz-worker.toml`:

```toml
worker_id = "pixhawk-bench-01"
api_url = "https://roz.example.com"
nats_url = "nats://roz.example.com:4222"
data_dir = "/var/lib/roz"

[mavlink]
transport = "serial:/dev/ttyUSB0:921600"
autopilot_hint = "px4"

[mavlink.signing]
posture = "off"
```

Use signing `on` for RF/routed links. Direct serial bench links can stay off.

## 5. Run as a Service

```sh
sudo tee /etc/systemd/system/roz-worker.service >/dev/null <<'UNIT'
[Unit]
Description=Roz Worker
After=network-online.target
Wants=network-online.target

[Service]
Environment=ROZ_WORKER_CONFIG=/etc/roz-worker.toml
ExecStart=/usr/local/bin/roz-worker
Restart=on-failure
RestartSec=5
User=ubuntu
WorkingDirectory=/var/lib/roz

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable --now roz-worker
sudo journalctl -u roz-worker -f
```

## 6. Enroll and Bind Safety

From an operator machine with server access:

```sh
roz device provision-key --host pixhawk-bench-01
roz safety policy bind --host pixhawk-bench-01 --policy bench-land-on-loss
```

Confirm the host is trusted before any motion command.

## 7. Dry Run

With propellers removed:

1. Open QGroundControl and confirm stable heartbeat.
2. Confirm Roz telemetry shows PX4 readiness.
3. Issue a safe task that arms, waits, lands, and disarms.
4. Verify QGroundControl can still command Land.
5. Test the hardware battery cutoff.

## 8. Tethered Flight

Install propellers only after the dry run passes. Use the tether, assign a kill
switch operator, cap takeoff altitude, and run the shortest useful task.

## 9. Export Evidence

After the session:

```sh
roz session export <session_id> --format mcap --output session.mcap
```

Open the MCAP in Foxglove and keep the file plus a screenshot or short video as
the acceptance record.

## Current Milestone Status

These docs are ready for operator review. The v3.0 milestone still requires a
real RPi 5 + Pixhawk 6C bench validation artifact before RD-03 can be marked
complete. Use `docs/deployments/v3-acceptance.md` for the full acceptance
checklist.
