# Companion Setup

This guide prepares a Raspberry Pi 5 style companion computer to run
`roz-worker` directly against Pixhawk MAVLink.

## Operating System

Install Ubuntu Server 22.04 or newer for arm64. Enable SSH during imaging.
After first boot:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev protobuf-compiler
sudo usermod -aG dialout "$USER"
```

Log out and back in so the `dialout` group applies.

## Serial Device

Connect Pixhawk TELEM2 to the companion UART or USB serial adapter. Verify the
device appears:

```sh
ls -l /dev/ttyUSB* /dev/ttyAMA* 2>/dev/null
```

Use a stable udev rule for production instead of a transient `/dev/ttyUSB0`
name if multiple serial devices are attached.

## Install Roz Worker

Build or install the worker binary on the companion:

```sh
git clone https://github.com/bedrock-dynamics/roz-oss.git
cd roz-oss
cargo build --release -p roz-worker
sudo install -m 0755 target/release/roz-worker /usr/local/bin/roz-worker
```

Create the runtime directory:

```sh
sudo mkdir -p /var/lib/roz
sudo chown "$USER":"$USER" /var/lib/roz
```

## Worker Config

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
posture = "off" # direct USB/serial bench link

[observability.camera]
record = "off"
```

Use `posture = "on"` for RF or routed links where MAVLink traffic leaves the
physical companion/FCU pair.

## Systemd Unit

Create `/etc/systemd/system/roz-worker.service`:

```ini
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
```

Enable it:

```sh
sudo systemctl daemon-reload
sudo systemctl enable roz-worker
sudo systemctl start roz-worker
sudo journalctl -u roz-worker -f
```

## Enrollment

Enroll the host before flight testing so signed dispatch and safety policy
binding use real server state:

```sh
roz device provision-key --host pixhawk-bench-01
roz safety policy bind --host pixhawk-bench-01 --policy bench-land-on-loss
```

Keep the returned private key material on the companion only. Rotate it if the
companion image is copied or lost.
