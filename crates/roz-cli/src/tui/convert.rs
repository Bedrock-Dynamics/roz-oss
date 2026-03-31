//! Conversions between `serde_json::Value` and `prost_types::Struct`.
//!
//! Mirrors the helpers in `roz-server::grpc::convert` so the CLI can build
//! proto messages without depending on the server crate.

use std::collections::BTreeMap;

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip_simple_object() {
        let original = json!({
            "name": "move_to",
            "count": 42,
            "enabled": true,
        });
        let prost = value_to_struct(original);
        let back = struct_to_value(prost);
        assert_eq!(back["name"], "move_to");
        assert_eq!(back["count"], 42.0);
        assert_eq!(back["enabled"], true);
    }

    #[test]
    fn roundtrip_nested_object() {
        let original = json!({
            "properties": {
                "x": {"type": "number"},
                "y": {"type": "number"},
            },
            "required": ["x"],
        });
        let prost = value_to_struct(original);
        let back = struct_to_value(prost);
        assert_eq!(back["properties"]["x"]["type"], "number");
        assert_eq!(back["required"][0], "x");
    }

    #[test]
    fn non_object_wrapped() {
        let prost = value_to_struct(json!("hello"));
        let back = struct_to_value(prost);
        assert_eq!(back["value"], "hello");
    }
}
