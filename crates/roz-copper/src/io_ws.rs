//! WebSocket bridge IO backends for the controller loop.
//!
//! [`WsActuatorSink`] sends [`CommandFrame`]s through a bounded `mpsc` channel
//! to a background tokio task that forwards them as JSON text over a persistent
//! WebSocket connection.  [`WsSensorSource`] reads the latest sensor state from
//! an [`ArcSwap`] that a background read task keeps up to date.
//!
//! The Copper controller thread runs on a plain `std::thread` at 50 Hz.
//! The WebSocket connection runs on tokio.  The two worlds are bridged by
//! `std::sync::mpsc::sync_channel` (actuator commands, controller -> tokio)
//! and `ArcSwap` (sensor data, tokio -> controller).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

use arc_swap::ArcSwap;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use roz_core::command::CommandFrame;
use roz_core::embodiment::FrameSnapshotInput;
use roz_core::template::render_template;

use crate::io::{ActuatorSink, SensorFrame, SensorSource};

// ---------------------------------------------------------------------------
// Shared sensor state
// ---------------------------------------------------------------------------

/// Sensor state updated by the WebSocket read task.
///
/// Stored behind an `ArcSwap` so the controller thread can read the latest
/// value without locking.  The `seq` counter lets `WsSensorSource` detect
/// whether a new reading has arrived since the last poll.
#[derive(Debug, Clone, Default)]
pub struct SensorState {
    pub joint_positions: Vec<f64>,
    pub seq: u64,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for a WebSocket bridge connection.
pub struct WsBridgeConfig {
    /// WebSocket URL to connect to (e.g. `ws://localhost:8080/ws`).
    pub url: String,
    /// The `type` field value for set-target messages (e.g. `"set_target"`).
    pub set_target_type: String,
    /// Body template with `{{channel_name}}` placeholders.
    pub body_template: String,
    /// Channel names in manifest order — indices match `CommandFrame.values`.
    pub channel_names: Vec<String>,
    /// Legacy default values. Partial actuator frames are rejected before render.
    pub channel_defaults: Vec<f64>,
}

// ---------------------------------------------------------------------------
// WsActuatorSink
// ---------------------------------------------------------------------------

/// Non-blocking actuator sink that forwards command frames over WebSocket.
///
/// The `send` method uses `try_send` on a bounded `sync_channel(4)` so it
/// never blocks the Copper controller thread.  If the channel is full
/// (i.e. the WS write loop is behind), the oldest unconsumed frame is
/// effectively dropped.
pub struct WsActuatorSink {
    pub(crate) tx: SyncSender<CommandFrame>,
}

impl ActuatorSink for WsActuatorSink {
    fn send(&self, frame: &CommandFrame) -> anyhow::Result<()> {
        match self.tx.try_send(frame.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => anyhow::bail!("WS bridge channel disconnected"),
        }
    }
}

// ---------------------------------------------------------------------------
// WsSensorSource
// ---------------------------------------------------------------------------

/// Lock-free sensor source backed by `ArcSwap<SensorState>`.
///
/// This type is `Send` but not `Sync` — it is moved into the controller
/// thread and polled each tick.  It tracks the last sequence number to
/// avoid returning the same reading twice.
pub struct WsSensorSource {
    pub(crate) state: Arc<ArcSwap<SensorState>>,
    pub(crate) last_seq: u64,
}

impl SensorSource for WsSensorSource {
    fn try_recv(&mut self) -> Option<SensorFrame> {
        let current = self.state.load();
        if current.seq == self.last_seq {
            return None;
        }
        self.last_seq = current.seq;
        Some(SensorFrame {
            entities: Vec::new(),
            joint_positions: current.joint_positions.clone(),
            joint_velocities: Vec::new(),
            sim_time_ns: 0,
            wrench: None,
            contact: None,
            frame_snapshot_input: FrameSnapshotInput::default(),
        })
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create a WebSocket bridge producing an actuator sink and a sensor source.
///
/// The bridge spawns a supervisor task on the provided tokio runtime that
/// maintains a persistent WebSocket connection with automatic reconnection.
///
/// Returns `(sink, source, supervisor_handle)`.
pub fn create_ws_bridge(
    config: WsBridgeConfig,
    runtime: &tokio::runtime::Handle,
) -> (Arc<WsActuatorSink>, Box<WsSensorSource>, JoinHandle<()>) {
    assert!(
        !config.body_template.is_empty() || config.channel_names.is_empty(),
        "WsBridgeConfig has {} command channels but no body_template — \
         commands will never reach the daemon. Add set_target_body to [daemon.websocket] in embodiment.toml (legacy robot.toml also accepted)",
        config.channel_names.len(),
    );

    let (tx, rx) = std::sync::mpsc::sync_channel(4);

    let sensor_state = Arc::new(ArcSwap::from_pointee(SensorState::default()));

    let sink = Arc::new(WsActuatorSink { tx });
    let source = Box::new(WsSensorSource {
        state: Arc::clone(&sensor_state),
        last_seq: 0,
    });

    let handle = runtime.spawn(ws_supervisor(config, rx, sensor_state));

    (sink, source, handle)
}

// ---------------------------------------------------------------------------
// Supervisor (reconnection loop)
// ---------------------------------------------------------------------------

/// Reconnection supervisor that maintains a persistent WebSocket connection.
///
/// On connection or protocol errors, logs a warning and retries with
/// exponential backoff (1 s initial, 30 s cap).  On clean shutdown
/// (channel disconnected), exits normally.
async fn ws_supervisor(
    config: WsBridgeConfig,
    rx: std::sync::mpsc::Receiver<CommandFrame>,
    sensor_state: Arc<ArcSwap<SensorState>>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    // We need to pass the receiver through iterations. Since `connect_and_run`
    // may fail and we need to retry, we wrap in an Option to move in/out.
    let mut rx_opt = Some(rx);

    loop {
        let rx_inner = rx_opt.take().expect("receiver must be available");

        match connect_and_run(&config, rx_inner, &sensor_state).await {
            Ok(returned_rx) => {
                // Normal shutdown — the mpsc sender was dropped.
                tracing::info!("WS bridge shut down cleanly");
                drop(returned_rx);
                return;
            }
            Err((err, returned_rx)) => {
                tracing::warn!(error = %err, backoff_ms = backoff.as_millis(), "WS connection error, reconnecting");
                rx_opt = Some(returned_rx);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Single WS session
// ---------------------------------------------------------------------------

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Run a single WebSocket session until it errors or the mpsc sender disconnects.
///
/// On error, returns the receiver so the supervisor can retry.
/// On clean shutdown (sender dropped), returns `Ok(receiver)`.
async fn connect_and_run(
    config: &WsBridgeConfig,
    rx: std::sync::mpsc::Receiver<CommandFrame>,
    sensor_state: &Arc<ArcSwap<SensorState>>,
) -> Result<std::sync::mpsc::Receiver<CommandFrame>, (anyhow::Error, std::sync::mpsc::Receiver<CommandFrame>)> {
    let ws_stream = match connect_async(&config.url).await {
        Ok((stream, _response)) => stream,
        Err(e) => return Err((anyhow::anyhow!("WS connect failed: {e}"), rx)),
    };

    tracing::info!(url = %config.url, "WS bridge connected");

    let (ws_write, ws_read) = ws_stream.split();

    // Spawn the read task that updates sensor state.
    let sensor_clone = Arc::clone(sensor_state);
    let read_handle = tokio::spawn(ws_read_task(ws_read, sensor_clone));

    // Run the write loop on the current task. It drains the mpsc and
    // sends rendered messages over the WebSocket.
    let result = ws_write_loop(config, rx, ws_write, &read_handle).await;

    // Ensure the read task is cleaned up.
    read_handle.abort();

    result
}

/// Background read task: parses incoming WebSocket messages and updates the
/// shared `SensorState` via `ArcSwap`.
async fn ws_read_task(mut ws_read: SplitStream<WsStream>, sensor_state: Arc<ArcSwap<SensorState>>) {
    let mut seq: u64 = 0;

    while let Some(msg_result) = ws_read.next().await {
        let msg = match msg_result {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!(error = %e, "WS read error");
                return;
            }
        };

        match msg {
            Message::Text(text) => {
                if let Some(positions) = parse_sensor_message(&text) {
                    seq += 1;
                    sensor_state.store(Arc::new(SensorState {
                        joint_positions: positions,
                        seq,
                    }));
                }
            }
            Message::Close(_) => {
                tracing::info!("WS read: received close frame");
                return;
            }
            // Binary, Ping, Pong, Frame — skip.
            _ => {}
        }
    }
}

/// Write loop: drains the `std::sync::mpsc` receiver and sends rendered
/// JSON messages over the WebSocket.
///
/// Uses `recv_timeout(100ms)` so it periodically checks whether the read
/// task has died (which signals a broken connection).
async fn ws_write_loop(
    config: &WsBridgeConfig,
    rx: std::sync::mpsc::Receiver<CommandFrame>,
    mut ws_write: SplitSink<WsStream, Message>,
    read_handle: &JoinHandle<()>,
) -> Result<std::sync::mpsc::Receiver<CommandFrame>, (anyhow::Error, std::sync::mpsc::Receiver<CommandFrame>)> {
    loop {
        // Check if the read task died (connection broken).
        if read_handle.is_finished() {
            return Err((anyhow::anyhow!("WS read task terminated unexpectedly"), rx));
        }

        // Use spawn_blocking to avoid blocking the tokio runtime with
        // the std::sync::mpsc recv_timeout call.
        let rx_ref = &rx;
        let frame = tokio::task::block_in_place(|| rx_ref.recv_timeout(Duration::from_millis(100)));

        match frame {
            Ok(frame) => {
                let msg_text = match render_frame(config, &frame) {
                    Ok(msg_text) => msg_text,
                    Err(error) => {
                        tracing::error!(%error, "dropping invalid partial WS actuator frame");
                        continue;
                    }
                };
                if let Err(e) = ws_write.send(Message::Text(msg_text.into())).await {
                    return Err((anyhow::anyhow!("WS write error: {e}"), rx));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // No command this tick — loop back and check read task health.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Sender dropped — clean shutdown.
                let _ = ws_write.send(Message::Close(None)).await;
                return Ok(rx);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Message rendering
// ---------------------------------------------------------------------------

/// Render a `CommandFrame` into a JSON text message using the config template.
///
/// Builds a `HashMap` of `channel_name` -> value string, then calls
/// `render_template` to substitute `{{channel_name}}` placeholders.
pub fn render_frame(config: &WsBridgeConfig, frame: &CommandFrame) -> anyhow::Result<String> {
    if frame.values.len() != config.channel_names.len() {
        anyhow::bail!(
            "command frame width {} does not match WS actuator layout {}",
            frame.values.len(),
            config.channel_names.len()
        );
    }

    let mut values = HashMap::with_capacity(config.channel_names.len());

    for (i, name) in config.channel_names.iter().enumerate() {
        let val = frame.values[i];
        values.insert(name.clone(), val.to_string());
    }

    Ok(render_template(&config.body_template, &values))
}

// ---------------------------------------------------------------------------
// Sensor message parsing
// ---------------------------------------------------------------------------

/// Parse a discriminated JSON message from the robot daemon into joint positions.
///
/// Currently handles:
/// - `{"type": "joint_positions", "head_joint_positions": [...], "antennas_joint_positions": [...]}`
///   -> concatenates head + antennas.
/// - `{"type": "head_pose", "head_pose": [[r00,r01,r02,r03],[r10,r11,r12,r13],[r20,r21,r22,r23],[r30,r31,r32,r33]]}`
///   -> extracts Euler angles (roll, pitch, yaw) from the 4x4 rotation matrix.
///
/// This parsing is Reachy-specific for now. A generic parser driven by the
/// embodiment manifest is a follow-up.
fn parse_sensor_message(text: &str) -> Option<Vec<f64>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = value.as_object()?;
    let msg_type = obj.get("type")?.as_str()?;

    match msg_type {
        "joint_positions" => parse_joint_positions(obj),
        "head_pose" => parse_head_pose(obj),
        _ => {
            tracing::trace!(msg_type, "ignoring unknown sensor message type");
            None
        }
    }
}

/// Parse `joint_positions`: concatenate `head_joint_positions` + `antennas_joint_positions`.
fn parse_joint_positions(obj: &serde_json::Map<String, serde_json::Value>) -> Option<Vec<f64>> {
    let head = obj.get("head_joint_positions")?.as_array()?;
    let antennas = obj.get("antennas_joint_positions")?.as_array()?;

    let mut positions = Vec::with_capacity(head.len() + antennas.len());
    for v in head.iter().chain(antennas.iter()) {
        positions.push(v.as_f64()?);
    }
    Some(positions)
}

/// Parse `head_pose`: extract Euler angles from a 4x4 homogeneous transform matrix.
///
/// The matrix is expected as `[[r00,r01,r02,tx],[r10,r11,r12,ty],[r20,r21,r22,tz],[0,0,0,1]]`.
/// Euler angles are extracted using ZYX (yaw-pitch-roll) convention:
/// - roll  = atan2(r21, r22)
/// - pitch = asin(-r20)
/// - yaw   = atan2(r10, r00)
fn parse_head_pose(obj: &serde_json::Map<String, serde_json::Value>) -> Option<Vec<f64>> {
    let matrix = obj.get("head_pose")?.as_array()?;
    if matrix.len() < 3 {
        return None;
    }

    let row = |i: usize| -> Option<Vec<f64>> { matrix[i].as_array()?.iter().map(serde_json::Value::as_f64).collect() };

    let r0 = row(0)?;
    let r1 = row(1)?;
    let r2 = row(2)?;

    if r0.len() < 3 || r1.len() < 3 || r2.len() < 3 {
        return None;
    }

    // ZYX Euler angles from rotation matrix
    let roll = r2[1].atan2(r2[2]);
    let pitch = (-r2[0]).asin();
    let yaw = r1[0].atan2(r0[0]);

    Some(vec![roll, pitch, yaw])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_actuator_sink_non_blocking() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let sink = WsActuatorSink { tx };
        let frame = CommandFrame { values: vec![0.1, 0.2] };
        sink.send(&frame).unwrap();
        sink.send(&frame).unwrap(); // should not block — drops if full
        assert_eq!(rx.try_recv().unwrap().values, vec![0.1, 0.2]);
    }

    #[test]
    fn ws_actuator_sink_disconnected_returns_error() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let sink = WsActuatorSink { tx };
        drop(rx); // disconnect
        let frame = CommandFrame { values: vec![1.0] };
        assert!(sink.send(&frame).is_err());
    }

    #[test]
    fn ws_sensor_returns_latest() {
        let state = Arc::new(ArcSwap::from_pointee(SensorState::default()));
        let mut source = WsSensorSource {
            state: Arc::clone(&state),
            last_seq: 0,
        };
        assert!(source.try_recv().is_none());

        state.store(Arc::new(SensorState {
            seq: 1,
            joint_positions: vec![0.5],
        }));
        let frame = source.try_recv().unwrap();
        assert_eq!(frame.joint_positions, vec![0.5]);
        assert!(source.try_recv().is_none()); // already consumed
    }

    #[test]
    fn ws_sensor_skips_to_latest() {
        let state = Arc::new(ArcSwap::from_pointee(SensorState::default()));
        let mut source = WsSensorSource {
            state: Arc::clone(&state),
            last_seq: 0,
        };

        // Write multiple updates before reading
        state.store(Arc::new(SensorState {
            seq: 1,
            joint_positions: vec![1.0],
        }));
        state.store(Arc::new(SensorState {
            seq: 2,
            joint_positions: vec![2.0],
        }));

        // Should get the latest, not the first
        let frame = source.try_recv().unwrap();
        assert_eq!(frame.joint_positions, vec![2.0]);
        assert!(source.try_recv().is_none());
    }

    #[test]
    fn render_frame_substitutes_channels() {
        let config = WsBridgeConfig {
            url: String::new(),
            set_target_type: "set_target".into(),
            body_template: r#"{"type": "set_target", "x": {{head_x}}, "y": {{head_y}}}"#.into(),
            channel_names: vec!["head_x".into(), "head_y".into()],
            channel_defaults: vec![0.0, 0.0],
        };
        let frame = CommandFrame { values: vec![0.1, 0.2] };
        let msg = render_frame(&config, &frame).unwrap();
        assert!(msg.contains("0.1"), "should contain head_x value: {msg}");
        assert!(msg.contains("0.2"), "should contain head_y value: {msg}");
    }

    #[test]
    fn render_frame_rejects_partial_channels() {
        let config = WsBridgeConfig {
            url: String::new(),
            set_target_type: "set_target".into(),
            body_template: r#"{"x": {{x}}, "y": {{y}}}"#.into(),
            channel_names: vec!["x".into(), "y".into()],
            channel_defaults: vec![0.0, 99.0],
        };
        let frame = CommandFrame { values: vec![1.5] };
        let err = render_frame(&config, &frame).unwrap_err();
        assert!(err.to_string().contains("width 1"), "unexpected error: {err}");
    }

    #[test]
    fn parse_joint_positions_message() {
        let msg = r#"{"type": "joint_positions", "head_joint_positions": [0.1, 0.2, 0.3], "antennas_joint_positions": [0.4, 0.5]}"#;
        let positions = parse_sensor_message(msg).unwrap();
        assert_eq!(positions, vec![0.1, 0.2, 0.3, 0.4, 0.5]);
    }

    #[test]
    fn parse_head_pose_identity_matrix() {
        // Identity rotation = zero Euler angles
        let msg = r#"{"type": "head_pose", "head_pose": [[1,0,0,0],[0,1,0,0],[0,0,1,0],[0,0,0,1]]}"#;
        let angles = parse_sensor_message(msg).unwrap();
        assert_eq!(angles.len(), 3);
        assert!((angles[0]).abs() < 1e-10, "roll should be ~0: {}", angles[0]);
        assert!((angles[1]).abs() < 1e-10, "pitch should be ~0: {}", angles[1]);
        assert!((angles[2]).abs() < 1e-10, "yaw should be ~0: {}", angles[2]);
    }

    #[test]
    fn parse_head_pose_90deg_yaw() {
        // 90-degree yaw (rotation around Z): [[0,-1,0,0],[1,0,0,0],[0,0,1,0],[0,0,0,1]]
        let msg = r#"{"type": "head_pose", "head_pose": [[0,-1,0,0],[1,0,0,0],[0,0,1,0],[0,0,0,1]]}"#;
        let angles = parse_sensor_message(msg).unwrap();
        let half_pi = std::f64::consts::FRAC_PI_2;
        assert!((angles[0]).abs() < 1e-10, "roll should be ~0: {}", angles[0]);
        assert!((angles[1]).abs() < 1e-10, "pitch should be ~0: {}", angles[1]);
        assert!(
            (angles[2] - half_pi).abs() < 1e-10,
            "yaw should be ~pi/2: {}",
            angles[2]
        );
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        let msg = r#"{"type": "battery_level", "level": 85}"#;
        assert!(parse_sensor_message(msg).is_none());
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_sensor_message("not json").is_none());
    }

    #[test]
    fn render_frame_produces_valid_json() {
        let config = WsBridgeConfig {
            url: String::new(),
            set_target_type: "set_full_target".into(),
            body_template: r#"{"type": "set_full_target", "head_pose": {"x": {{head_x}}, "y": {{head_y}}, "z": {{head_z}}, "roll": {{head_roll}}, "pitch": {{head_pitch}}, "yaw": {{head_yaw}}}, "antennas": [{{right_antenna}}, {{left_antenna}}], "body_yaw": {{body_yaw}}}"#.into(),
            channel_names: vec![
                "head_x".into(), "head_y".into(), "head_z".into(),
                "head_roll".into(), "head_pitch".into(), "head_yaw".into(),
                "body_yaw".into(), "left_antenna".into(), "right_antenna".into(),
            ],
            channel_defaults: vec![0.0; 9],
        };
        let frame = CommandFrame {
            values: vec![0.0, 0.0, 0.0, 0.0, 0.3, 0.0, 0.0, 0.0, 0.0],
        };
        let msg = render_frame(&config, &frame).unwrap();

        // Must be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&msg)
            .unwrap_or_else(|e| panic!("rendered frame is not valid JSON: {e}\n  output: {msg}"));

        // Must have the type field
        assert_eq!(parsed["type"], "set_full_target", "missing or wrong type field");

        // Must not have unsubstituted placeholders
        assert!(!msg.contains("{{"), "unsubstituted placeholder in: {msg}");
    }

    #[test]
    fn render_frame_empty_template_produces_empty_string() {
        let config = WsBridgeConfig {
            url: String::new(),
            set_target_type: String::new(),
            body_template: String::new(),
            channel_names: vec![],
            channel_defaults: vec![],
        };
        let frame = CommandFrame { values: vec![] };
        assert_eq!(render_frame(&config, &frame).unwrap(), "");
    }

    #[test]
    #[should_panic(expected = "body_template")]
    fn create_ws_bridge_panics_on_empty_template_with_channels() {
        let config = WsBridgeConfig {
            url: "ws://localhost:9999/ws".into(),
            set_target_type: "set_target".into(),
            body_template: String::new(),
            channel_names: vec!["joint_0".into()],
            channel_defaults: vec![0.0],
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _bridge = create_ws_bridge(config, rt.handle());
    }
}
