use rhai::{Dynamic, Engine, EvalAltResult, Position};

use super::bridge::{SandboxBridge, SandboxOutcome};

pub fn run(code: &str, bridge: SandboxBridge) -> SandboxOutcome {
    let mut engine = Engine::new();

    let print_bridge = bridge.clone();
    engine.on_print(move |text| {
        print_bridge.print(text);
    });

    let call_bridge = bridge.clone();
    engine.register_fn(
        "call_tool",
        move |tool_name: &str, params: Dynamic| -> Result<Dynamic, Box<EvalAltResult>> {
            let params_json = rhai::serde::from_dynamic::<serde_json::Value>(&params).map_err(|error| {
                Box::new(EvalAltResult::ErrorRuntime(
                    format!("invalid call_tool params: {error}").into(),
                    Position::NONE,
                ))
            })?;
            let output = call_bridge
                .call_tool_json(tool_name, params_json)
                .map_err(|error| Box::new(EvalAltResult::ErrorRuntime(error.to_string().into(), Position::NONE)))?;
            rhai::serde::to_dynamic(output).map_err(|error| {
                Box::new(EvalAltResult::ErrorRuntime(
                    format!("failed to encode tool output: {error}").into(),
                    Position::NONE,
                ))
            })
        },
    );

    match engine.run(code) {
        Ok(()) => bridge.success_outcome(),
        Err(error) => runtime_error_outcome(&bridge, format!("rhai runtime error: {error}")),
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
