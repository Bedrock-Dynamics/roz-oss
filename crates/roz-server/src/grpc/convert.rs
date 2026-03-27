//! Conversions between generated protobuf types (`roz_v1`) and domain types
//! in `roz_agent::model::types` and `roz_core::tools`.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use roz_agent::model::types::{ContentPart as DomainContentPart, Message, MessageRole};

use super::roz_v1;

// ---------------------------------------------------------------------------
// serde_json::Value <-> prost_types::Struct helpers
// ---------------------------------------------------------------------------

/// Convert a `serde_json::Value` (must be an Object) into a `prost_types::Struct`.
///
/// Non-object values are wrapped in a single-key struct with key `"value"`.
pub fn value_to_struct(value: serde_json::Value) -> prost_types::Struct {
    match value {
        serde_json::Value::Object(map) => prost_types::Struct {
            fields: map.into_iter().map(|(k, v)| (k, json_to_prost_value(v))).collect(),
        },
        other => {
            let mut fields = BTreeMap::new();
            fields.insert("value".to_string(), json_to_prost_value(other));
            prost_types::Struct { fields }
        }
    }
}

/// Convert a `prost_types::Struct` back into a `serde_json::Value` (always an Object).
pub fn struct_to_value(s: prost_types::Struct) -> serde_json::Value {
    serde_json::Value::Object(s.fields.into_iter().map(|(k, v)| (k, prost_value_to_json(v))).collect())
}

fn json_to_prost_value(v: serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;

    let kind = match v {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(b),
        serde_json::Value::Number(n) => {
            // protobuf Value only supports f64; precision loss is inherent for large integers (>2^53).
            #[allow(clippy::cast_precision_loss)]
            let f = n.as_f64().unwrap_or_else(|| {
                n.as_u64()
                    .map_or_else(|| n.as_i64().map_or(0.0, |i| i as f64), |u| u as f64)
            });
            Kind::NumberValue(f)
        }
        serde_json::Value::String(s) => Kind::StringValue(s),
        serde_json::Value::Array(arr) => Kind::ListValue(prost_types::ListValue {
            values: arr.into_iter().map(json_to_prost_value).collect(),
        }),
        serde_json::Value::Object(map) => Kind::StructValue(prost_types::Struct {
            fields: map.into_iter().map(|(k, v2)| (k, json_to_prost_value(v2))).collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}

fn prost_value_to_json(v: prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;

    match v.kind {
        None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(b),
        Some(Kind::NumberValue(n)) => {
            serde_json::Number::from_f64(n).map_or(serde_json::Value::Null, serde_json::Value::Number)
        }
        Some(Kind::StringValue(s)) => serde_json::Value::String(s),
        Some(Kind::ListValue(list)) => {
            serde_json::Value::Array(list.values.into_iter().map(prost_value_to_json).collect())
        }
        Some(Kind::StructValue(s)) => struct_to_value(s),
    }
}

// ---------------------------------------------------------------------------
// ContentPart conversions
// ---------------------------------------------------------------------------

/// Error returned when a proto message cannot be converted to a domain type.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("unknown message role: {0}")]
    UnknownRole(String),
    #[error("content part has no oneof variant set")]
    EmptyContentPart,
    #[error("invalid base64 image data: {0}")]
    InvalidBase64(#[from] base64::DecodeError),
}

impl TryFrom<roz_v1::ContentPart> for DomainContentPart {
    type Error = ConvertError;

    fn try_from(proto: roz_v1::ContentPart) -> Result<Self, Self::Error> {
        use roz_v1::content_part::Part;

        let part = proto.part.ok_or(ConvertError::EmptyContentPart)?;
        match part {
            Part::Text(t) => Ok(Self::Text { text: t.text }),
            Part::ToolUse(tu) => Ok(Self::ToolUse {
                id: tu.id,
                name: tu.name,
                input: tu.input.map_or_else(
                    || serde_json::Value::Object(serde_json::Map::default()),
                    struct_to_value,
                ),
            }),
            Part::ToolResult(tr) => Ok(Self::ToolResult {
                tool_use_id: tr.tool_use_id,
                name: tr.name,
                content: tr.content,
                is_error: tr.is_error,
            }),
            Part::Thinking(th) => Ok(Self::Thinking {
                thinking: th.thinking,
                signature: th.signature,
            }),
            Part::Image(img) => {
                let data = BASE64.encode(&img.data);
                Ok(Self::Image {
                    media_type: img.media_type,
                    data,
                })
            }
        }
    }
}

impl TryFrom<&DomainContentPart> for roz_v1::ContentPart {
    type Error = ConvertError;

    fn try_from(domain: &DomainContentPart) -> Result<Self, Self::Error> {
        use roz_v1::content_part::Part;

        let part = match domain {
            DomainContentPart::Text { text } => Part::Text(roz_v1::TextContent { text: text.clone() }),
            DomainContentPart::ToolUse { id, name, input } => Part::ToolUse(roz_v1::ToolUseContent {
                id: id.clone(),
                name: name.clone(),
                input: Some(value_to_struct(input.clone())),
            }),
            DomainContentPart::ToolResult {
                tool_use_id,
                name,
                content,
                is_error,
            } => Part::ToolResult(roz_v1::ToolResultContent {
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            DomainContentPart::Thinking { thinking, signature } => Part::Thinking(roz_v1::ThinkingContent {
                thinking: thinking.clone(),
                signature: signature.clone(),
            }),
            DomainContentPart::Image { media_type, data } => {
                // Domain stores base64 string; proto uses raw bytes.
                let bytes = BASE64.decode(data)?;
                Part::Image(roz_v1::ImageContent {
                    media_type: media_type.clone(),
                    data: bytes,
                })
            }
        };

        Ok(Self { part: Some(part) })
    }
}

// ---------------------------------------------------------------------------
// ConversationMessage <-> Message
// ---------------------------------------------------------------------------

impl TryFrom<roz_v1::ConversationMessage> for Message {
    type Error = ConvertError;

    fn try_from(proto: roz_v1::ConversationMessage) -> Result<Self, Self::Error> {
        let role = match proto.role.as_str() {
            "system" => MessageRole::System,
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            other => return Err(ConvertError::UnknownRole(other.to_string())),
        };

        let parts: Result<Vec<DomainContentPart>, ConvertError> =
            proto.parts.into_iter().map(DomainContentPart::try_from).collect();

        Ok(Self { role, parts: parts? })
    }
}

impl TryFrom<&Message> for roz_v1::ConversationMessage {
    type Error = ConvertError;

    fn try_from(msg: &Message) -> Result<Self, Self::Error> {
        let role = match msg.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };

        let parts: Result<Vec<roz_v1::ContentPart>, ConvertError> =
            msg.parts.iter().map(roz_v1::ContentPart::try_from).collect();

        Ok(Self {
            role: role.to_string(),
            parts: parts?,
        })
    }
}

// ---------------------------------------------------------------------------
// ToolSchema conversion (proto -> domain only, one-way)
// ---------------------------------------------------------------------------

/// Note: `timeout_ms` from the proto is intentionally dropped — the domain `ToolSchema`
/// does not carry a timeout. Timeout is handled at the dispatch layer (`ToolRequest.timeout_ms`).
impl From<roz_v1::ToolSchema> for roz_core::tools::ToolSchema {
    fn from(proto: roz_v1::ToolSchema) -> Self {
        Self {
            name: proto.name,
            description: proto.description,
            parameters: proto.parameters_schema.map_or_else(
                || serde_json::Value::Object(serde_json::Map::default()),
                struct_to_value,
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn conversation_message_roundtrip() {
        let domain = Message {
            role: MessageRole::User,
            parts: vec![
                DomainContentPart::Text {
                    text: "Hello".to_string(),
                },
                DomainContentPart::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "move_arm".to_string(),
                    input: json!({"x": 1.0, "y": 2.0}),
                },
            ],
        };

        // Domain -> Proto
        let proto = roz_v1::ConversationMessage::try_from(&domain).unwrap();
        assert_eq!(proto.role, "user");
        assert_eq!(proto.parts.len(), 2);

        // Proto -> Domain
        let roundtripped = Message::try_from(proto).expect("should convert back");
        assert_eq!(roundtripped.role, MessageRole::User);
        assert_eq!(roundtripped.parts.len(), 2);

        // Verify text part
        match &roundtripped.parts[0] {
            DomainContentPart::Text { text } => assert_eq!(text, "Hello"),
            other => panic!("expected Text, got {other:?}"),
        }

        // Verify tool_use part
        match &roundtripped.parts[1] {
            DomainContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "move_arm");
                assert_eq!(input["x"], 1.0);
                assert_eq!(input["y"], 2.0);
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn content_part_text_roundtrip() {
        let domain = DomainContentPart::Text {
            text: "some text".to_string(),
        };
        let proto = roz_v1::ContentPart::try_from(&domain).unwrap();
        let back = DomainContentPart::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn content_part_tool_use_roundtrip() {
        let domain = DomainContentPart::ToolUse {
            id: "toolu_42".to_string(),
            name: "read_sensor".to_string(),
            input: json!({"sensor": "lidar", "range": 10.5}),
        };
        let proto = roz_v1::ContentPart::try_from(&domain).unwrap();

        // Verify the Struct was populated
        let tool_use = match &proto.part {
            Some(roz_v1::content_part::Part::ToolUse(tu)) => tu,
            other => panic!("expected ToolUse, got {other:?}"),
        };
        assert!(tool_use.input.is_some());

        let back = DomainContentPart::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn content_part_tool_result_roundtrip() {
        let domain = DomainContentPart::ToolResult {
            tool_use_id: "toolu_99".to_string(),
            name: "gripper_open".to_string(),
            content: "gripper opened successfully".to_string(),
            is_error: false,
        };
        let proto = roz_v1::ContentPart::try_from(&domain).unwrap();
        let back = DomainContentPart::try_from(proto).unwrap();
        assert_eq!(back, domain);

        // Also test error variant
        let domain_err = DomainContentPart::ToolResult {
            tool_use_id: "toolu_err".to_string(),
            name: "move_arm".to_string(),
            content: "collision detected".to_string(),
            is_error: true,
        };
        let proto_err = roz_v1::ContentPart::try_from(&domain_err).unwrap();
        let back_err = DomainContentPart::try_from(proto_err).unwrap();
        assert_eq!(back_err, domain_err);
    }

    #[test]
    fn content_part_thinking_roundtrip() {
        let domain = DomainContentPart::Thinking {
            thinking: "I need to check sensor readings before moving the arm.".to_string(),
            signature: "sig_abc123".to_string(),
        };
        let proto = roz_v1::ContentPart::try_from(&domain).unwrap();
        let back = DomainContentPart::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn content_part_image_roundtrip() {
        // Raw bytes -> base64 -> raw bytes roundtrip
        let raw_bytes: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // PNG header
        let base64_str = BASE64.encode(&raw_bytes);

        let domain = DomainContentPart::Image {
            media_type: "image/png".to_string(),
            data: base64_str,
        };
        let proto = roz_v1::ContentPart::try_from(&domain).unwrap();

        // Verify proto stores raw bytes
        let img = match &proto.part {
            Some(roz_v1::content_part::Part::Image(img)) => img,
            other => panic!("expected Image, got {other:?}"),
        };
        assert_eq!(img.data, raw_bytes);
        assert_eq!(img.media_type, "image/png");

        let back = DomainContentPart::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn tool_schema_conversion() {
        let proto = roz_v1::ToolSchema {
            name: "move_arm".to_string(),
            description: "Move the robot arm to a position".to_string(),
            parameters_schema: Some(value_to_struct(json!({
                "type": "object",
                "properties": {
                    "x": {"type": "number"},
                    "y": {"type": "number"},
                    "z": {"type": "number"}
                },
                "required": ["x", "y", "z"]
            }))),
            timeout_ms: 5000,
            category: roz_v1::ToolCategoryHint::ToolCategoryPhysical.into(),
        };

        let domain: roz_core::tools::ToolSchema = proto.into();
        assert_eq!(domain.name, "move_arm");
        assert_eq!(domain.description, "Move the robot arm to a position");
        assert_eq!(domain.parameters["type"], "object");
        assert!(domain.parameters["properties"]["x"].is_object());
        assert_eq!(domain.parameters["required"][0], "x");
        assert_eq!(domain.parameters["required"][1], "y");
        assert_eq!(domain.parameters["required"][2], "z");
    }

    #[test]
    fn value_to_struct_and_back() {
        let original = json!({
            "string_val": "hello",
            "number_val": 42.5,
            "bool_val": true,
            "null_val": null,
            "array_val": [1, "two", false, null],
            "nested": {
                "inner": "value",
                "deep": {
                    "level": 3
                }
            }
        });

        let prost_struct = value_to_struct(original);
        let roundtripped = struct_to_value(prost_struct);

        assert_eq!(roundtripped["string_val"], "hello");
        assert!((roundtripped["number_val"].as_f64().unwrap() - 42.5).abs() < f64::EPSILON);
        assert_eq!(roundtripped["bool_val"], true);
        assert!(roundtripped["null_val"].is_null());
        assert_eq!(roundtripped["array_val"][0], 1.0);
        assert_eq!(roundtripped["array_val"][1], "two");
        assert_eq!(roundtripped["array_val"][2], false);
        assert!(roundtripped["array_val"][3].is_null());
        assert_eq!(roundtripped["nested"]["inner"], "value");
        assert_eq!(roundtripped["nested"]["deep"]["level"], 3.0);
    }

    #[test]
    fn empty_content_part_returns_error() {
        let proto = roz_v1::ContentPart { part: None };
        let result = DomainContentPart::try_from(proto);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no oneof variant"));
    }

    #[test]
    fn unknown_role_returns_error() {
        let proto = roz_v1::ConversationMessage {
            role: "tool".to_string(),
            parts: vec![],
        };
        let result = Message::try_from(proto);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown message role"));
    }

    #[test]
    fn all_message_roles_roundtrip() {
        for (role, role_str) in [
            (MessageRole::System, "system"),
            (MessageRole::User, "user"),
            (MessageRole::Assistant, "assistant"),
        ] {
            let domain = Message {
                role,
                parts: vec![DomainContentPart::Text {
                    text: "test".to_string(),
                }],
            };
            let proto = roz_v1::ConversationMessage::try_from(&domain).unwrap();
            assert_eq!(proto.role, role_str);
            let back = Message::try_from(proto).unwrap();
            assert_eq!(back.role, role);
        }
    }
}
