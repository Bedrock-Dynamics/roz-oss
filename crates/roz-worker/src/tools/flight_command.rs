//! `flight_command` tool — narrow Phase 27-aligned route to a MAVLink sink.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_copper::io::{
    DiscreteCommandSink, FlightCommand, FlightCommandParams, FlightCommandResponse, MavFrame, MavResult,
    MavlinkDispatchError,
};
use roz_core::tools::ToolResult;

/// Concrete extension key for the worker's flight-command sink.
#[derive(Clone)]
pub struct FlightCommandSinkHandle(
    pub  Arc<
        dyn DiscreteCommandSink<FlightCommand, Response = FlightCommandResponse, Error = MavlinkDispatchError>
            + Send
            + Sync,
    >,
);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FlightCommandInput {
    /// Command variant: arm, disarm, takeoff, land, rtl, set_mode, or goto.
    pub command: String,
    pub altitude_m: Option<f64>,
    pub mode: Option<String>,
    pub latitude_deg: Option<f64>,
    pub longitude_deg: Option<f64>,
    pub relative_altitude_m: Option<f64>,
}

/// Agent-visible flight-command route.
pub struct FlightCommandTool;

fn build_params(input: &FlightCommandInput) -> FlightCommandParams {
    FlightCommandParams {
        altitude_m: input.altitude_m.unwrap_or_default(),
        x: input.latitude_deg.unwrap_or_default(),
        y: input.longitude_deg.unwrap_or_default(),
        z: input.relative_altitude_m.or(input.altitude_m).unwrap_or_default(),
        mode: input.mode.clone().unwrap_or_default(),
        vehicle_index: 0,
        frame: Some(MavFrame::GlobalRelativeAltInt),
    }
}

fn build_command(input: &FlightCommandInput) -> Result<FlightCommand, String> {
    let params = build_params(input);
    match input.command.trim().to_ascii_lowercase().as_str() {
        "arm" => Ok(FlightCommand::Arm(params)),
        "disarm" => Ok(FlightCommand::Disarm(params)),
        "takeoff" => Ok(FlightCommand::Takeoff(params)),
        "land" => Ok(FlightCommand::Land(params)),
        "rtl" | "return_to_launch" => Ok(FlightCommand::ReturnToLaunch(params)),
        "set_mode" => Ok(FlightCommand::SetMode(params)),
        "goto" => Ok(FlightCommand::Goto(params)),
        other => Err(format!("unsupported flight_command command: {other}")),
    }
}

fn mav_result_name(result: MavResult) -> &'static str {
    match result {
        MavResult::Accepted => "accepted",
        MavResult::TemporarilyRejected => "temporarily_rejected",
        MavResult::Denied => "denied",
        MavResult::Unsupported => "unsupported",
        MavResult::Failed => "failed",
        MavResult::InProgress => "in_progress",
        MavResult::Cancelled => "cancelled",
    }
}

#[async_trait]
impl TypedToolExecutor for FlightCommandTool {
    type Input = FlightCommandInput;

    fn name(&self) -> &'static str {
        "flight_command"
    }

    fn description(&self) -> &'static str {
        "Dispatch a discrete MAVLink flight command: arm, disarm, takeoff, land, rtl, set_mode, or goto."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let Some(sink) = ctx.extensions.get::<FlightCommandSinkHandle>() else {
            return Ok(ToolResult::error(
                "flight_command unavailable: MAVLink sink missing".to_string(),
            ));
        };
        let command = match build_command(&input) {
            Ok(command) => command,
            Err(error) => return Ok(ToolResult::error(error)),
        };

        let response = match sink.0.send_command(command) {
            Ok(response) => response,
            Err(error) => return Ok(ToolResult::error(format!("flight_command dispatch failed: {error}"))),
        };

        if response.result != MavResult::Accepted {
            return Ok(ToolResult::error(format!(
                "flight_command denied by MAVLink ACK: {:?} {}",
                response.result, response.error
            )));
        }

        Ok(ToolResult::success(serde_json::json!({
            "status": "sent",
            "mav_result": mav_result_name(response.result),
            "error": response.error,
        })))
    }
}
