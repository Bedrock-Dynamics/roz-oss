use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Phoenix Channels v2 wire format: `[join_ref, ref, topic, event, payload]`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhoenixMessage {
    pub join_ref: Option<String>,
    pub msg_ref: Option<String>,
    pub topic: String,
    pub event: String,
    pub payload: Value,
}

impl PhoenixMessage {
    /// Parse from Phoenix v2 array format: `[join_ref, ref, topic, event, payload]`
    pub fn from_array(arr: &[Value]) -> Option<Self> {
        if arr.len() != 5 {
            return None;
        }
        Some(Self {
            join_ref: arr[0].as_str().map(String::from),
            msg_ref: arr[1].as_str().map(String::from),
            topic: arr[2].as_str()?.to_string(),
            event: arr[3].as_str()?.to_string(),
            payload: arr[4].clone(),
        })
    }

    /// Serialize to Phoenix v2 array format
    pub fn to_array(&self) -> Value {
        serde_json::json!([self.join_ref, self.msg_ref, self.topic, self.event, self.payload,])
    }

    /// Create a heartbeat response
    pub fn heartbeat_reply(msg_ref: Option<String>) -> Self {
        Self {
            join_ref: None,
            msg_ref,
            topic: "phoenix".to_string(),
            event: "phx_reply".to_string(),
            payload: serde_json::json!({"status": "ok", "response": {}}),
        }
    }

    /// Create a join reply
    pub fn join_reply(join_ref: Option<String>, msg_ref: Option<String>, topic: String) -> Self {
        Self {
            join_ref,
            msg_ref,
            topic,
            event: "phx_reply".to_string(),
            payload: serde_json::json!({"status": "ok", "response": {}}),
        }
    }
}

/// Channel topic patterns
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelTopic {
    /// `task:{task_id}`
    Task(String),
    /// `host:{host_id}:telemetry`
    HostTelemetry(String),
    /// `host:{host_id}:status`
    HostStatus(String),
    /// Unrecognized topic
    Unknown(String),
}

impl ChannelTopic {
    pub fn parse(topic: &str) -> Self {
        topic.strip_prefix("task:").map_or_else(
            || {
                topic.strip_prefix("host:").map_or_else(
                    || Self::Unknown(topic.to_string()),
                    |rest| {
                        rest.strip_suffix(":telemetry").map_or_else(
                            || {
                                rest.strip_suffix(":status").map_or_else(
                                    || Self::Unknown(topic.to_string()),
                                    |host_id| Self::HostStatus(host_id.to_string()),
                                )
                            },
                            |host_id| Self::HostTelemetry(host_id.to_string()),
                        )
                    },
                )
            },
            |task_id| Self::Task(task_id.to_string()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_array_valid_5_element() {
        let arr = vec![
            json!("join-1"),
            json!("ref-1"),
            json!("task:abc"),
            json!("phx_join"),
            json!({"token": "xyz"}),
        ];
        let msg = PhoenixMessage::from_array(&arr).unwrap();
        assert_eq!(msg.join_ref, Some("join-1".to_string()));
        assert_eq!(msg.msg_ref, Some("ref-1".to_string()));
        assert_eq!(msg.topic, "task:abc");
        assert_eq!(msg.event, "phx_join");
        assert_eq!(msg.payload, json!({"token": "xyz"}));
    }

    #[test]
    fn from_array_with_null_refs() {
        let arr = vec![
            json!(null),
            json!(null),
            json!("phoenix"),
            json!("heartbeat"),
            json!({}),
        ];
        let msg = PhoenixMessage::from_array(&arr).unwrap();
        assert_eq!(msg.join_ref, None);
        assert_eq!(msg.msg_ref, None);
        assert_eq!(msg.topic, "phoenix");
        assert_eq!(msg.event, "heartbeat");
    }

    #[test]
    fn from_array_wrong_length_returns_none() {
        let arr = vec![json!("a"), json!("b"), json!("c")];
        assert!(PhoenixMessage::from_array(&arr).is_none());

        let arr = vec![json!("a"), json!("b"), json!("c"), json!("d"), json!("e"), json!("f")];
        assert!(PhoenixMessage::from_array(&arr).is_none());
    }

    #[test]
    fn from_array_non_string_topic_returns_none() {
        let arr = vec![
            json!(null),
            json!(null),
            json!(42), // topic must be a string
            json!("event"),
            json!({}),
        ];
        assert!(PhoenixMessage::from_array(&arr).is_none());
    }

    #[test]
    fn to_array_round_trips() {
        let original = PhoenixMessage {
            join_ref: Some("j1".to_string()),
            msg_ref: Some("r1".to_string()),
            topic: "task:123".to_string(),
            event: "new_msg".to_string(),
            payload: json!({"data": "hello"}),
        };
        let arr_val = original.to_array();
        let arr = arr_val.as_array().unwrap();
        let reconstructed = PhoenixMessage::from_array(arr).unwrap();

        assert_eq!(reconstructed.join_ref, original.join_ref);
        assert_eq!(reconstructed.msg_ref, original.msg_ref);
        assert_eq!(reconstructed.topic, original.topic);
        assert_eq!(reconstructed.event, original.event);
        assert_eq!(reconstructed.payload, original.payload);
    }

    #[test]
    fn to_array_round_trips_with_null_refs() {
        let original = PhoenixMessage {
            join_ref: None,
            msg_ref: None,
            topic: "phoenix".to_string(),
            event: "heartbeat".to_string(),
            payload: json!({}),
        };
        let arr_val = original.to_array();
        let arr = arr_val.as_array().unwrap();
        let reconstructed = PhoenixMessage::from_array(arr).unwrap();

        assert_eq!(reconstructed.join_ref, None);
        assert_eq!(reconstructed.msg_ref, None);
        assert_eq!(reconstructed.topic, "phoenix");
        assert_eq!(reconstructed.event, "heartbeat");
    }

    #[test]
    fn heartbeat_reply_creates_correct_message() {
        let reply = PhoenixMessage::heartbeat_reply(Some("ref-42".to_string()));
        assert_eq!(reply.join_ref, None);
        assert_eq!(reply.msg_ref, Some("ref-42".to_string()));
        assert_eq!(reply.topic, "phoenix");
        assert_eq!(reply.event, "phx_reply");
        assert_eq!(reply.payload["status"], "ok");
        assert_eq!(reply.payload["response"], json!({}));
    }

    #[test]
    fn heartbeat_reply_with_none_ref() {
        let reply = PhoenixMessage::heartbeat_reply(None);
        assert_eq!(reply.msg_ref, None);
        assert_eq!(reply.topic, "phoenix");
        assert_eq!(reply.event, "phx_reply");
    }

    #[test]
    fn join_reply_creates_correct_message() {
        let reply =
            PhoenixMessage::join_reply(Some("j-1".to_string()), Some("r-1".to_string()), "task:abc".to_string());
        assert_eq!(reply.join_ref, Some("j-1".to_string()));
        assert_eq!(reply.msg_ref, Some("r-1".to_string()));
        assert_eq!(reply.topic, "task:abc");
        assert_eq!(reply.event, "phx_reply");
        assert_eq!(reply.payload["status"], "ok");
        assert_eq!(reply.payload["response"], json!({}));
    }

    #[test]
    fn channel_topic_parse_task() {
        assert_eq!(
            ChannelTopic::parse("task:abc-123"),
            ChannelTopic::Task("abc-123".to_string())
        );
    }

    #[test]
    fn channel_topic_parse_host_telemetry() {
        assert_eq!(
            ChannelTopic::parse("host:robot-7:telemetry"),
            ChannelTopic::HostTelemetry("robot-7".to_string())
        );
    }

    #[test]
    fn channel_topic_parse_host_status() {
        assert_eq!(
            ChannelTopic::parse("host:robot-7:status"),
            ChannelTopic::HostStatus("robot-7".to_string())
        );
    }

    #[test]
    fn channel_topic_parse_unknown() {
        assert_eq!(
            ChannelTopic::parse("something:else"),
            ChannelTopic::Unknown("something:else".to_string())
        );
    }

    #[test]
    fn channel_topic_parse_host_without_suffix_is_unknown() {
        // "host:robot-7" without :telemetry or :status is unknown
        assert_eq!(
            ChannelTopic::parse("host:robot-7"),
            ChannelTopic::Unknown("host:robot-7".to_string())
        );
    }
}
