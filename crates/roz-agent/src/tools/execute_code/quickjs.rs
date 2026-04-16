use std::collections::HashMap;
use std::panic::AssertUnwindSafe;

use quick_js::{Arguments, Context as JsContext, JsValue};

use super::bridge::{SandboxBridge, SandboxOutcome};

pub fn run(code: &str, bridge: SandboxBridge) -> SandboxOutcome {
    let context = match JsContext::new() {
        Ok(context) => context,
        Err(error) => return runtime_error_outcome(&bridge, format!("quickjs init failed: {error}")),
    };

    let print_bridge = AssertUnwindSafe(bridge.clone());
    if let Err(error) = context.add_callback("print", move |args: Arguments| {
        let rendered = args
            .into_vec()
            .into_iter()
            .map(|value| stringify_js_value(&value))
            .collect::<Vec<_>>()
            .join(" ");
        print_bridge.0.print(rendered);
        JsValue::Undefined
    }) {
        return runtime_error_outcome(&bridge, format!("quickjs print binding failed: {error}"));
    }

    let call_bridge = AssertUnwindSafe(bridge.clone());
    if let Err(error) = context.add_callback("call_tool", move |args: Arguments| -> Result<JsValue, String> {
        let mut args = args.into_vec().into_iter();
        let tool_name = match args.next() {
            Some(JsValue::String(value)) => value,
            Some(other) => {
                return Err(format!(
                    "call_tool expected first argument to be a string, got {other:?}"
                ));
            }
            None => return Err("call_tool expected a tool name".to_string()),
        };
        let params = args.next().unwrap_or(JsValue::Object(HashMap::new()));
        let params_json = js_to_json(params).map_err(|err| err.to_string())?;
        let output = call_bridge
            .0
            .call_tool_json(&tool_name, params_json)
            .map_err(|err| err.to_string())?;
        json_to_js(output).map_err(|err| err.to_string())
    }) {
        return runtime_error_outcome(&bridge, format!("quickjs call_tool binding failed: {error}"));
    }

    match context.eval(code) {
        Ok(_) => bridge.success_outcome(),
        Err(error) => runtime_error_outcome(&bridge, format!("quickjs runtime error: {error}")),
    }
}

fn runtime_error_outcome(bridge: &SandboxBridge, message: String) -> SandboxOutcome {
    bridge.write_stderr(&message);
    if message.contains("timed out") {
        bridge.timeout_outcome(message)
    } else {
        bridge.error_outcome(message)
    }
}

fn js_to_json(value: JsValue) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    Ok(match value {
        JsValue::Undefined | JsValue::Null => serde_json::Value::Null,
        JsValue::Bool(value) => serde_json::Value::Bool(value),
        JsValue::Int(value) => serde_json::json!(value),
        JsValue::Float(value) => serde_json::json!(value),
        JsValue::String(value) => serde_json::Value::String(value),
        JsValue::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(js_to_json).collect::<Result<Vec<_>, _>>()?)
        }
        JsValue::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| Ok((key, js_to_json(value)?)))
                .collect::<Result<serde_json::Map<_, _>, Box<dyn std::error::Error + Send + Sync>>>()?,
        ),
        other => return Err(format!("unsupported QuickJS value: {other:?}").into()),
    })
}

fn json_to_js(value: serde_json::Value) -> Result<JsValue, Box<dyn std::error::Error + Send + Sync>> {
    Ok(match value {
        serde_json::Value::Null => JsValue::Null,
        serde_json::Value::Bool(value) => JsValue::Bool(value),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                JsValue::Int(value.try_into()?)
            } else if let Some(value) = value.as_f64() {
                JsValue::Float(value)
            } else {
                return Err("unsupported JSON number".into());
            }
        }
        serde_json::Value::String(value) => JsValue::String(value),
        serde_json::Value::Array(values) => {
            JsValue::Array(values.into_iter().map(json_to_js).collect::<Result<Vec<_>, _>>()?)
        }
        serde_json::Value::Object(values) => JsValue::Object(
            values
                .into_iter()
                .map(|(key, value)| Ok((key, json_to_js(value)?)))
                .collect::<Result<HashMap<_, _>, Box<dyn std::error::Error + Send + Sync>>>()?,
        ),
    })
}

fn stringify_js_value(value: &JsValue) -> String {
    match value {
        JsValue::Undefined => "undefined".to_string(),
        JsValue::Null => "null".to_string(),
        JsValue::Bool(value) => value.to_string(),
        JsValue::Int(value) => value.to_string(),
        JsValue::Float(value) => value.to_string(),
        JsValue::String(value) => value.clone(),
        JsValue::Array(_) | JsValue::Object(_) => js_to_json(value.clone())
            .map(|value| value.to_string())
            .unwrap_or_else(|_| "<unprintable>".to_string()),
        other => format!("{other:?}"),
    }
}
