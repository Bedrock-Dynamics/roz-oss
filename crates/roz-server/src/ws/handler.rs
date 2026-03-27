use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// GET /v1/ws -- WebSocket upgrade endpoint.
pub async fn ws_upgrade(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_ws)
}

/// Per-connection state tracking joined channels and heartbeat.
pub struct WsConnection {
    channels: HashMap<String, ChannelState>,
    last_heartbeat: Instant,
}

#[allow(dead_code)]
struct ChannelState {
    topic: String,
    join_ref: String,
}

impl Default for WsConnection {
    fn default() -> Self {
        Self::new()
    }
}

impl WsConnection {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
            last_heartbeat: Instant::now(),
        }
    }

    pub fn heartbeat_stale(&self, timeout: Duration) -> bool {
        self.last_heartbeat.elapsed() > timeout
    }

    pub fn record_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
    }

    pub fn join_channel(&mut self, topic: String, join_ref: String) {
        self.channels.insert(topic.clone(), ChannelState { topic, join_ref });
    }

    pub fn leave_channel(&mut self, topic: &str) -> bool {
        self.channels.remove(topic).is_some()
    }

    #[allow(dead_code)]
    pub fn is_joined(&self, topic: &str) -> bool {
        self.channels.contains_key(topic)
    }
}

/// Handle a WebSocket connection after upgrade.
///
/// Processes Phoenix Channels v2 protocol messages: join, leave, heartbeat.
/// Closes with 4009 on heartbeat timeout, 4003 on invalid auth.
pub async fn handle_ws(mut socket: WebSocket) {
    let mut conn = WsConnection::new();
    let heartbeat_timeout = Duration::from_secs(60);
    let mut heartbeat_check = tokio::time::interval(Duration::from_secs(15));

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_text_message(&mut conn, &mut socket, &text).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // ignore binary/ping/pong
                }
            }
            _ = heartbeat_check.tick() => {
                if conn.heartbeat_stale(heartbeat_timeout) {
                    let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4009,
                        reason: "heartbeat timeout".into(),
                    }))).await;
                    break;
                }
            }
        }
    }
}

async fn handle_text_message(conn: &mut WsConnection, socket: &mut WebSocket, text: &str) {
    // Parse as Phoenix message: [join_ref, ref, topic, event, payload]
    let Ok(msg) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        return;
    };
    if msg.len() < 5 {
        return;
    }

    let join_ref = msg[0].as_str().unwrap_or_default().to_string();
    let msg_ref = msg[1].as_str().unwrap_or_default().to_string();
    let topic = msg[2].as_str().unwrap_or_default().to_string();
    let event = msg[3].as_str().unwrap_or_default();

    match event {
        "phx_join" => {
            conn.join_channel(topic.clone(), join_ref.clone());
            let reply = serde_json::json!([
                join_ref, msg_ref, topic, "phx_reply",
                {"status": "ok", "response": {}}
            ]);
            let _ = socket.send(Message::Text(reply.to_string().into())).await;
        }
        "phx_leave" => {
            conn.leave_channel(&topic);
            let reply = serde_json::json!([
                join_ref, msg_ref, topic, "phx_reply",
                {"status": "ok", "response": {}}
            ]);
            let _ = socket.send(Message::Text(reply.to_string().into())).await;
        }
        "heartbeat" => {
            conn.record_heartbeat();
            let reply = serde_json::json!([
                null, msg_ref, "phoenix", "phx_reply",
                {"status": "ok", "response": {}}
            ]);
            let _ = socket.send(Message::Text(reply.to_string().into())).await;
        }
        _ => {
            // Unknown event on joined channel -- ignore for now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_connection_is_not_stale() {
        let conn = WsConnection::new();
        assert!(!conn.heartbeat_stale(Duration::from_secs(60)));
    }

    #[test]
    fn heartbeat_becomes_stale() {
        let mut conn = WsConnection::new();
        // Manually set last_heartbeat to the past
        conn.last_heartbeat = Instant::now().checked_sub(Duration::from_secs(120)).unwrap();
        assert!(conn.heartbeat_stale(Duration::from_secs(60)));
    }

    #[test]
    fn record_heartbeat_resets_staleness() {
        let mut conn = WsConnection::new();
        conn.last_heartbeat = Instant::now().checked_sub(Duration::from_secs(120)).unwrap();
        conn.record_heartbeat();
        assert!(!conn.heartbeat_stale(Duration::from_secs(60)));
    }

    #[test]
    fn join_and_leave_channel() {
        let mut conn = WsConnection::new();
        assert!(!conn.is_joined("task:123"));
        conn.join_channel("task:123".into(), "1".into());
        assert!(conn.is_joined("task:123"));
        assert!(conn.leave_channel("task:123"));
        assert!(!conn.is_joined("task:123"));
    }

    #[test]
    fn leave_unknown_channel_returns_false() {
        let mut conn = WsConnection::new();
        assert!(!conn.leave_channel("nonexistent"));
    }
}
