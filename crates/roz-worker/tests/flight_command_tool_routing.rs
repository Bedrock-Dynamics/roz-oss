use std::sync::{Arc, Mutex};
use std::time::Duration;

use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher};
use roz_copper::io::{DiscreteCommandSink, FlightCommand, FlightCommandResponse, MavResult, MavlinkDispatchError};
use roz_core::tools::{ToolCall, ToolCategory, ToolResult};
use roz_worker::tools::flight_command::{FlightCommandSinkHandle, FlightCommandTool};
use serde_json::json;

#[derive(Clone)]
struct FakeFlightSink {
    commands: Arc<Mutex<Vec<FlightCommand>>>,
    response: Arc<Mutex<FlightCommandResponse>>,
}

impl FakeFlightSink {
    fn new(response: FlightCommandResponse) -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(response)),
        }
    }
}

impl DiscreteCommandSink<FlightCommand> for FakeFlightSink {
    type Error = MavlinkDispatchError;
    type Response = FlightCommandResponse;

    fn send_command(&self, cmd: FlightCommand) -> Result<Self::Response, Self::Error> {
        self.commands
            .lock()
            .expect("fake sink command mutex poisoned")
            .push(cmd);
        Ok(self.response.lock().expect("fake sink response mutex poisoned").clone())
    }
}

fn accepted() -> FlightCommandResponse {
    FlightCommandResponse {
        result: MavResult::Accepted,
        error: String::new(),
    }
}

fn dispatcher() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register_with_category(Box::new(FlightCommandTool), ToolCategory::Physical);
    dispatcher
}

fn context_with_sink(sink: Option<FlightCommandSinkHandle>) -> ToolContext {
    let mut extensions = Extensions::new();
    if let Some(sink) = sink {
        extensions.insert(sink);
    }
    ToolContext {
        task_id: "flight-command-routing".into(),
        tenant_id: "tenant-flight".into(),
        call_id: "call-flight".into(),
        extensions,
    }
}

async fn dispatch_call(dispatcher: &ToolDispatcher, ctx: &ToolContext, params: serde_json::Value) -> ToolResult {
    let call = ToolCall {
        id: "call-flight-command".into(),
        tool: "flight_command".into(),
        params,
    };
    dispatcher.dispatch(&call, &ctx).await
}

#[test]
fn schema_exposes_one_flight_command_tool_with_variant_arg() {
    let dispatcher = dispatcher();
    let schemas = dispatcher.schemas();
    let schema = schemas
        .iter()
        .find(|schema| schema.name == "flight_command")
        .expect("flight_command schema must be registered");
    let required = schema
        .parameters
        .get("required")
        .and_then(|value| value.as_array())
        .expect("schema must include required array");
    assert!(
        required.iter().any(|value| value.as_str() == Some("command")),
        "flight_command schema must require command: {schema:?}"
    );
}

#[tokio::test]
async fn missing_sink_returns_unavailable_tool_result() {
    let dispatcher = dispatcher();
    let ctx = context_with_sink(None);
    let result = dispatch_call(&dispatcher, &ctx, json!({ "command": "arm" })).await;
    assert!(result.is_error(), "missing sink must fail visibly");
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("flight_command unavailable")),
        "unexpected missing-sink error: {result:?}"
    );
}

#[tokio::test]
async fn arm_command_routes_to_discrete_sink() {
    let dispatcher = dispatcher();
    let fake = Arc::new(FakeFlightSink::new(accepted()));
    let recorded = Arc::clone(&fake.commands);
    let sink: Arc<
        dyn DiscreteCommandSink<FlightCommand, Response = FlightCommandResponse, Error = MavlinkDispatchError>
            + Send
            + Sync,
    > = fake;
    let ctx = context_with_sink(Some(FlightCommandSinkHandle(sink)));

    let result = dispatch_call(&dispatcher, &ctx, json!({ "command": "arm" })).await;

    assert!(result.is_success(), "arm command should succeed: {result:?}");
    assert_eq!(result.output["mav_result"], "accepted");
    let commands = recorded.lock().expect("fake sink command mutex poisoned");
    assert_eq!(commands.len(), 1, "expected exactly one routed command");
    assert!(
        matches!(commands.first(), Some(FlightCommand::Arm(_))),
        "expected FlightCommand::Arm, got {commands:?}"
    );
}

#[tokio::test]
async fn denied_ack_surfaces_failed_tool_result() {
    let dispatcher = dispatcher();
    let fake = Arc::new(FakeFlightSink::new(FlightCommandResponse {
        result: MavResult::Denied,
        error: "arming denied by test ACK".into(),
    }));
    let sink: Arc<
        dyn DiscreteCommandSink<FlightCommand, Response = FlightCommandResponse, Error = MavlinkDispatchError>
            + Send
            + Sync,
    > = fake;
    let ctx = context_with_sink(Some(FlightCommandSinkHandle(sink)));

    let result = dispatch_call(&dispatcher, &ctx, json!({ "command": "arm" })).await;

    assert!(result.is_error(), "denied ACK must be surfaced as a failed tool result");
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("denied by MAVLink ACK")),
        "unexpected denied ACK error: {result:?}"
    );
}
