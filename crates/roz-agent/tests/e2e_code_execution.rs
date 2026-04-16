//! End-to-end tests for the `execute_code` agent tool contract.
//!
//! Run: cargo test -p roz-agent --test e2e_code_execution

use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode, PresenceSignal};
use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher, ToolExecutor};
use roz_agent::model::types::*;
use roz_agent::safety::SafetyStack;
use roz_agent::session_runtime::ApprovalRuntimeHandle;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_agent::tools::execute_code::{EXECUTE_CODE_TOOL_NAME, ExecuteCodeTool};
use roz_core::auth::{ApiKeyScope, AuthIdentity, TenantId};
use roz_core::tools::{ToolCall, ToolCategory, ToolResult, ToolSchema};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn test_auth_identity() -> AuthIdentity {
    AuthIdentity::ApiKey {
        key_id: uuid::Uuid::nil(),
        tenant_id: TenantId::new(uuid::Uuid::nil()),
        scopes: vec![ApiKeyScope::Admin],
    }
}

fn test_control_manifest() -> roz_core::embodiment::binding::ControlInterfaceManifest {
    let mut manifest = roz_core::embodiment::binding::ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: (0..6)
            .map(|index| roz_core::embodiment::binding::ControlChannelDef {
                name: format!("joint{index}/velocity"),
                interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: "base".into(),
            })
            .collect(),
        bindings: Vec::new(),
    };
    manifest.stamp_digest();
    manifest
}

/// Build extensions with a generic 6-joint canonical control manifest for execute_code tests.
fn test_extensions() -> Extensions {
    let mut ext = Extensions::new();
    ext.insert(test_control_manifest());
    ext.insert(test_auth_identity());
    ext
}

fn build_input(user_message: &str) -> AgentInput {
    build_input_with_prompt(
        vec!["You are a robot controller. Use execute_code to deploy control code.".to_string()],
        user_message,
    )
}

fn build_input_with_prompt(system_prompt: Vec<String>, user_message: &str) -> AgentInput {
    AgentInput {
        task_id: "e2e-test".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        seed: AgentInputSeed::new(system_prompt, Vec::new(), user_message.to_string()),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    }
}

struct UppercaseTool;

#[async_trait::async_trait]
impl ToolExecutor for UppercaseTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "uppercase_text".to_string(),
            description: "Uppercase text".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let text = params["text"].as_str().unwrap_or_default().to_uppercase();
        Ok(ToolResult::success(json!({ "text": text })))
    }
}

struct AppendTool;

#[async_trait::async_trait]
impl ToolExecutor for AppendTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "append_text".to_string(),
            description: "Append two strings".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "left": { "type": "string" },
                    "right": { "type": "string" }
                },
                "required": ["left", "right"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let left = params["left"].as_str().unwrap_or_default();
        let right = params["right"].as_str().unwrap_or_default();
        Ok(ToolResult::success(json!({ "text": format!("{left}{right}") })))
    }
}

struct LengthTool;

#[async_trait::async_trait]
impl ToolExecutor for LengthTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "text_length".to_string(),
            description: "Return text length".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let text = params["text"].as_str().unwrap_or_default();
        Ok(ToolResult::success(json!({ "length": text.chars().count() })))
    }
}

struct PhysicalTrapTool;

#[async_trait::async_trait]
impl ToolExecutor for PhysicalTrapTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "set_motors".to_string(),
            description: "Physical trap tool".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "speed": { "type": "number" }
                }
            }),
        }
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ToolResult::success(json!({ "status": "motors_set" })))
    }
}

struct IdentityEchoTool;

#[async_trait::async_trait]
impl ToolExecutor for IdentityEchoTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "identity_echo".to_string(),
            description: "Return the nested auth identity".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let identity = ctx
            .extensions
            .get::<AuthIdentity>()
            .cloned()
            .ok_or_else(|| std::io::Error::other("missing auth identity"))?;
        Ok(ToolResult::success(json!({
            "identity": serde_json::to_value(identity)?,
            "call_id": ctx.call_id.clone(),
        })))
    }
}

fn runtime_dispatcher() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(Box::new(ExecuteCodeTool), ToolCategory::CodeSandbox);
    dispatcher.register_with_category(Box::new(UppercaseTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(AppendTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(LengthTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(IdentityEchoTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(PhysicalTrapTool), ToolCategory::Physical);
    dispatcher
}

fn runtime_context(dispatcher: &ToolDispatcher) -> ToolContext {
    let mut extensions = test_extensions();
    extensions.insert(Arc::new(dispatcher.clone()));
    ToolContext {
        task_id: "runtime-test".to_string(),
        tenant_id: "tenant-test".to_string(),
        call_id: String::new(),
        extensions,
    }
}

fn runtime_context_with_approval(
    dispatcher: &ToolDispatcher,
) -> (
    ToolContext,
    ApprovalRuntimeHandle,
    tokio::sync::mpsc::Receiver<PresenceSignal>,
) {
    let mut extensions = test_extensions();
    let approval_runtime = ApprovalRuntimeHandle::default();
    let (presence_tx, presence_rx) = tokio::sync::mpsc::channel(8);
    extensions.insert(Arc::new(dispatcher.clone()));
    extensions.insert(approval_runtime.clone());
    extensions.insert(presence_tx);
    (
        ToolContext {
            task_id: "runtime-test".to_string(),
            tenant_id: "tenant-test".to_string(),
            call_id: "call-runtime".to_string(),
            extensions,
        },
        approval_runtime,
        presence_rx,
    )
}

#[tokio::test]
async fn javascript_qjs_executes_pure_tool_chain_in_one_call() {
    let dispatcher = runtime_dispatcher();
    let ctx = runtime_context(&dispatcher);
    let call = ToolCall {
        id: "call-js".to_string(),
        tool: EXECUTE_CODE_TOOL_NAME.to_string(),
        params: json!({
            "language": "javascript_qjs",
            "code": r#"
                const upper = call_tool("uppercase_text", { text: "arm" });
                const ready = call_tool("append_text", { left: upper.text, right: "-ready" });
                const length = call_tool("text_length", { text: ready.text });
                print(`${ready.text}:${length.length}`);
            "#,
        }),
    };

    let result = dispatcher.dispatch(&call, &ctx).await;
    assert!(result.is_success(), "javascript_qjs should succeed: {result:?}");
    let output = result.output.to_string();
    assert!(output.contains("success"), "result should report success: {output}");
    assert!(
        output.contains("ARM-ready:9"),
        "script output should contain final chain result: {output}"
    );
    assert!(
        output.contains("\"tool_calls_made\":3"),
        "should record three nested tool calls: {output}"
    );
}

#[tokio::test]
async fn rhai_executes_pure_tool_chain_in_one_call() {
    let dispatcher = runtime_dispatcher();
    let ctx = runtime_context(&dispatcher);
    let call = ToolCall {
        id: "call-rhai".to_string(),
        tool: EXECUTE_CODE_TOOL_NAME.to_string(),
        params: json!({
            "language": "rhai",
            "code": r#"
                let upper = call_tool("uppercase_text", #{ text: "arm" });
                let ready = call_tool("append_text", #{ left: upper["text"], right: "-ready" });
                let length = call_tool("text_length", #{ text: ready["text"] });
                print(ready["text"] + ":" + length["length"].to_string());
            "#,
        }),
    };

    let result = dispatcher.dispatch(&call, &ctx).await;
    assert!(result.is_success(), "rhai should succeed: {result:?}");
    let output = result.output.to_string();
    assert!(output.contains("success"), "result should report success: {output}");
    assert!(
        output.contains("ARM-ready:9"),
        "script output should contain final chain result: {output}"
    );
    assert!(
        output.contains("\"tool_calls_made\":3"),
        "should record three nested tool calls: {output}"
    );
}

#[tokio::test]
async fn execute_code_requires_approval_runtime_for_nested_physical_tools() {
    let dispatcher = runtime_dispatcher();
    let ctx = runtime_context(&dispatcher);
    let call = ToolCall {
        id: "call-blocked".to_string(),
        tool: EXECUTE_CODE_TOOL_NAME.to_string(),
        params: json!({
            "language": "javascript_qjs",
            "code": r#"
                call_tool("set_motors", { speed: 0.5 });
            "#,
        }),
    };

    let result = dispatcher.dispatch(&call, &ctx).await;
    assert!(result.is_error(), "non-pure nested tool should fail");
    let output = result.output.to_string();
    assert!(output.contains("error"), "result should report error status: {output}");
    assert!(
        output.contains("ApprovalRuntimeHandle extension missing"),
        "bridge should fail closed when approval runtime is unavailable: {output}"
    );
}

#[tokio::test]
async fn execute_code_reuses_captured_auth_identity_for_nested_calls() {
    let dispatcher = runtime_dispatcher();
    let ctx = runtime_context(&dispatcher);
    let call = ToolCall {
        id: "call-identity".to_string(),
        tool: EXECUTE_CODE_TOOL_NAME.to_string(),
        params: json!({
            "language": "javascript_qjs",
            "code": r#"
                const who = call_tool("identity_echo", {});
                print(who.identity.ApiKey.key_id);
                print(who.call_id);
            "#,
        }),
    };

    let result = dispatcher.dispatch(&call, &ctx).await;
    assert!(result.is_success(), "identity roundtrip should succeed: {result:?}");
    let output = result.output.to_string();
    assert!(output.contains(&uuid::Uuid::nil().to_string()));
    assert!(output.contains("call-identity::nested::1::identity_echo"));
}

#[tokio::test]
async fn execute_code_nested_physical_tool_waits_for_approval_and_resumes() {
    let dispatcher = runtime_dispatcher();
    let (ctx, approval_runtime, mut presence_rx) = runtime_context_with_approval(&dispatcher);
    let call = ToolCall {
        id: "call-approved".to_string(),
        tool: EXECUTE_CODE_TOOL_NAME.to_string(),
        params: json!({
            "language": "javascript_qjs",
            "code": r#"
                const motors = call_tool("set_motors", { speed: 0.5 });
                print(motors.status);
            "#,
        }),
    };

    let approval_task = tokio::spawn(async move {
        let mut approval_id = None;
        let mut saw_resolved = false;
        while let Some(signal) = presence_rx.recv().await {
            match signal {
                PresenceSignal::ApprovalRequested {
                    approval_id: id,
                    action,
                    ..
                } => {
                    assert_eq!(action, "set_motors");
                    approval_runtime.resolve_approval(&id, true, Some(json!({"speed": 0.25})));
                    approval_id = Some(id);
                }
                PresenceSignal::ApprovalResolved { approval_id: id, .. } => {
                    if approval_id.as_deref() == Some(id.as_str()) {
                        saw_resolved = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(approval_id.is_some(), "expected ApprovalRequested signal");
        assert!(saw_resolved, "expected ApprovalResolved signal");
    });

    let result = dispatcher.dispatch(&call, &ctx).await;
    approval_task.await.expect("approval task should finish");
    assert!(result.is_success(), "approved physical tool should succeed: {result:?}");
    let output = result.output.to_string();
    assert!(output.contains("motors_set"));
    assert!(output.contains("\"tool_calls_made\":1"));
}

#[tokio::test]
async fn execute_code_nested_physical_tool_denial_returns_structured_error() {
    let dispatcher = runtime_dispatcher();
    let (ctx, approval_runtime, _presence_rx) = runtime_context_with_approval(&dispatcher);
    let call = ToolCall {
        id: "call-denied".to_string(),
        tool: EXECUTE_CODE_TOOL_NAME.to_string(),
        params: json!({
            "language": "rhai",
            "code": r#"
                let denied = call_tool("set_motors", #{ speed: 0.5 });
                print(denied);
            "#,
        }),
    };

    let approval_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        approval_runtime.resolve_approval("call-denied::nested::1::set_motors", false, None);
    });

    let result = dispatcher.dispatch(&call, &ctx).await;
    approval_task.await.expect("approval task should finish");
    assert!(result.is_error(), "denied physical tool should fail");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Permission denied by user"),
        "expected denial to surface through ToolResult.error: {result:?}"
    );
    let output = result.output.to_string();
    assert!(output.contains("\"status\":\"error\"") || output.contains("\"status\": \"error\""));
}

#[tokio::test]
async fn agent_handles_execute_code_contract_error() {
    // MockModel will:
    //   Turn 1: Call execute_code with the Phase 20 request shape
    //   Turn 2: Respond after seeing the structured tool result

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "call_1".to_string(),
                name: EXECUTE_CODE_TOOL_NAME.to_string(),
                input: json!({
                    "language": "javascript_qjs",
                    "code": "print('hello from sandbox');",
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "I attempted the sandbox run and got a structured execute_code result back.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 30,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(Box::new(ExecuteCodeTool), ToolCategory::CodeSandbox);
    assert_eq!(dispatcher.category(EXECUTE_CODE_TOOL_NAME), ToolCategory::CodeSandbox);

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(test_extensions());

    let input = build_input("Collapse a tool chain into one execute_code call");
    let output = agent.run(input).await.expect("agent loop should complete");

    assert_eq!(output.cycles, 2, "should take 2 cycles (tool call + final response)");

    let response = output.final_response.as_deref().expect("should have final response");
    assert!(
        response.contains("execute_code") || response.contains("structured"),
        "response should acknowledge the tool result, got: {response}"
    );

    assert_eq!(output.total_usage.input_tokens, 300, "100 + 200 input tokens");
    assert_eq!(output.total_usage.output_tokens, 80, "50 + 30 output tokens");
}

#[tokio::test]
async fn agent_handles_execute_code_input_validation_error() {
    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "call_1".to_string(),
                name: EXECUTE_CODE_TOOL_NAME.to_string(),
                input: json!({
                    "language": "rhai",
                    "code": "   ",
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "The execute_code request was invalid. Let me fix the input.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 30,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(Box::new(ExecuteCodeTool), ToolCategory::CodeSandbox);
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(test_extensions());

    let input = build_input("Write a controller");
    let output = agent
        .run(input)
        .await
        .expect("agent should handle compile error gracefully");

    assert_eq!(output.cycles, 2);
    assert!(output.final_response.is_some());
}

/// Test with a real LLM that generates WAT code and execute_code runs it.
///
/// Requires `ANTHROPIC_API_KEY` or `ROZ_API_KEY` environment variable.
///
/// Run: `ANTHROPIC_API_KEY=... cargo test -p roz-agent --test e2e_code_execution -- live_model --ignored --nocapture`
#[tokio::test]
#[ignore = "requires API key: ANTHROPIC_API_KEY or ROZ_API_KEY"]
async fn live_model_generates_and_executes_wasm() {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ROZ_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or ROZ_API_KEY");

    let model = roz_agent::model::create_model(
        "claude-sonnet-4-6",
        "",
        "",
        120,
        "anthropic",
        Some(&api_key),
        &roz_core::auth::TenantId::new(uuid::Uuid::nil()),
        std::sync::Arc::new(roz_core::model_endpoint::EndpointRegistry::empty()),
    )
    .expect("should create model");

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(Box::new(ExecuteCodeTool), ToolCategory::CodeSandbox);
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(test_extensions());

    let input = AgentInput {
        task_id: "live-test".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec![
                "You are a robot controller. You have access to the execute_code tool. \
                 When asked to control a robot, write a WAT (WebAssembly Text) module \
                 that exports a `process` function taking an i64 parameter (the tick count). \
                 Use the execute_code tool to compile and verify the code. \
                 Keep the WAT simple — a no-op function is fine for testing."
                    .to_string(),
            ],
            Vec::new(),
            "Write a simple controller that does nothing on each tick. Use execute_code to verify it works."
                .to_string(),
        ),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.expect("agent should complete");

    // The agent should have called execute_code and gotten a result back
    assert!(
        output.cycles >= 2,
        "should call tool + respond, got {} cycles",
        output.cycles
    );
    assert!(output.final_response.is_some(), "should have a final response");

    // Print response for manual inspection (--nocapture)
    let response = output.final_response.unwrap();
    eprintln!("Live model response: {response}");
    // Don't assert on specific text -- LLM output is non-deterministic.
    // The fact that the agent loop completed without error is the real test.
}

/// Verify that an agent run produces a complete, serialisable output matching
/// the shape that `--non-interactive` would emit to stdout.
///
/// Requires `ANTHROPIC_API_KEY` or `ROZ_API_KEY` environment variable.
#[tokio::test]
#[ignore = "requires API key: ANTHROPIC_API_KEY or ROZ_API_KEY"]
async fn non_interactive_produces_valid_response() {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ROZ_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or ROZ_API_KEY");

    let model = roz_agent::model::create_model(
        "claude-sonnet-4-6",
        "",
        "",
        120,
        "anthropic",
        Some(&api_key),
        &roz_core::auth::TenantId::new(uuid::Uuid::nil()),
        std::sync::Arc::new(roz_core::model_endpoint::EndpointRegistry::empty()),
    )
    .expect("should create model");

    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = build_input_with_prompt(
        vec!["You are a test agent. Be concise.".to_string()],
        "Say hello in exactly one word.",
    );
    let output = agent.run(input).await.expect("agent should complete");

    // Verify the output matches what --non-interactive would produce
    assert!(output.final_response.is_some(), "should have response");
    assert!(output.cycles >= 1, "should take at least 1 cycle");
    assert!(output.total_usage.input_tokens > 0, "should use input tokens");
    assert!(output.total_usage.output_tokens > 0, "should use output tokens");

    // Verify JSON serialization matches non_interactive format
    let json = serde_json::json!({
        "status": "success",
        "response": output.final_response,
        "usage": {
            "input_tokens": output.total_usage.input_tokens,
            "output_tokens": output.total_usage.output_tokens,
        },
        "cycles": output.cycles,
    });
    let serialized = serde_json::to_string_pretty(&json).unwrap();
    assert!(serialized.contains("\"status\": \"success\""));
    assert!(serialized.contains("\"response\""));
    eprintln!("Non-interactive output:\n{serialized}");
}

/// Test that a real Claude model can generate WAT code using the channel
/// interface (`command::set`, `math::sin`) and that the generated code
/// compiles and runs in the WASM sandbox without crashing.
///
/// The agent runs in React mode with NO tools -- we just want the text
/// response containing the WAT.  We then extract the WAT, compile it
/// with `CuWasmTask::from_source_with_host` using a UR5 manifest, tick
/// 100 times, and verify stability.
///
/// Requires `ANTHROPIC_API_KEY` environment variable.
///
/// Run:
/// ```text
/// ANTHROPIC_API_KEY=... cargo test -p roz-agent --test e2e_code_execution \
///     -- live_model_generates_channel --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn live_model_generates_channel_interface_wasm() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("set ANTHROPIC_API_KEY");

    let model = roz_agent::model::create_model(
        "claude-sonnet-4-6",
        "",
        "",
        120,
        "anthropic",
        Some(&api_key),
        &roz_core::auth::TenantId::new(uuid::Uuid::nil()),
        std::sync::Arc::new(roz_core::model_endpoint::EndpointRegistry::empty()),
    )
    .expect("should create model");

    // No tools -- we only want the text response containing WAT code.
    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let system_prompt = "\
You are a WebAssembly code generator for robot control.

You have access to the following host functions via WAT imports:

  (import \"command\" \"set\" (func $cmd_set (param i32 f64) (result i32)))
    Write a value to command channel `index`. Returns 0 on success, -1 on bad index, -2 if clamped.

  (import \"command\" \"count\" (func $cmd_count (result i32)))
    Returns the number of command channels.

  (import \"state\" \"get\" (func $state_get (param i32) (result f64)))
    Read the current value of state channel `index`.

  (import \"math\" \"sin\" (func $sin (param f64) (result f64)))
    Returns sin(x).

  (import \"math\" \"cos\" (func $cos (param f64) (result f64)))
    Returns cos(x).

The module MUST export a function: (func (export \"process\") (param i64))
The i64 parameter is the tick counter (increments each call).

Respond with ONLY the WAT code. No explanation, no markdown fences, just raw WAT starting with (module."
        .to_string();

    let input = AgentInput {
        task_id: "live-channel-test".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec![system_prompt],
            Vec::new(),
            "Write a WAT module that oscillates command channel 0 using sin. \
            Use the tick parameter to compute sin(tick * 0.05) * 0.5 and write it to channel 0."
                .to_string(),
        ),
        max_cycles: 1,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.expect("agent should complete");
    assert!(output.final_response.is_some(), "should have a response");

    let response = output.final_response.unwrap();
    eprintln!("Claude response:\n{response}");

    // Extract WAT: may be bare or inside a markdown code fence.
    let wat = extract_wat(&response);
    eprintln!("Extracted WAT:\n{wat}");
    assert!(wat.contains("(module"), "response must contain a WAT module");

    // Compile with a 6-joint canonical control manifest.
    let control_manifest = test_control_manifest();
    let host = roz_copper::wit_host::HostContext::with_control_manifest(&control_manifest);
    let mut task = roz_copper::wasm::CuWasmTask::from_source_with_host(wat.as_bytes(), host)
        .expect("Claude-generated WAT should compile");

    // Tick 100 times -- the real test is that it doesn't crash.
    for tick in 0..100_u64 {
        task.tick_with_contract(tick, None).expect("tick should not trap");
    }

    // Verify low rejection count (a few clamps are acceptable).
    let rejections = task
        .host_context()
        .rejection_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(rejections < 10, "rejection count should be low, got {rejections}");

    eprintln!("PASS: Claude generated channel-interface WAT, compiled, 100 ticks, {rejections} rejections");
}

/// Extract WAT source from a model response that may contain markdown fences.
fn extract_wat(response: &str) -> &str {
    // Try ```wat ... ``` or ```wasm ... ``` or ``` ... ```
    if let Some(start) = response.find("```") {
        let after_fence = &response[start + 3..];
        // Skip optional language tag on the same line.
        let code_start = after_fence.find('\n').map_or(0, |i| i + 1);
        let code = &after_fence[code_start..];
        if let Some(end) = code.find("```") {
            return code[..end].trim();
        }
    }
    // No fences -- assume the whole response is WAT.
    response.trim()
}
