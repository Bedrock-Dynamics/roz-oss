//! Criterion benchmark for the Phase 20 execute_code round-trip reduction claim.
//!
//! Compares a deterministic five-step pick-and-place tool chain executed as:
//! 1. five direct tool dispatches, and
//! 2. one `execute_code` call that performs the same chain in-process.
//!
//! Run: `cargo bench -p roz-bench --bench execute_code_roundtrip`

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher, ToolExecutor};
use roz_agent::tools::execute_code::{EXECUTE_CODE_TOOL_NAME, ExecuteCodeTool};
use roz_core::auth::{ApiKeyScope, AuthIdentity, TenantId};
use roz_core::tools::{ToolCall, ToolCategory, ToolResult, ToolSchema};
use serde_json::json;

struct PlanPickTool;
struct CheckGripperTool;
struct ApproachPoseTool;
struct CloseGripperTool;
struct PlaceObjectTool;

#[async_trait]
impl ToolExecutor for PlanPickTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "plan_pick".into(),
            description: "Plan a deterministic pick".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "object": { "type": "string" },
                    "destination": { "type": "string" }
                },
                "required": ["object", "destination"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let object = params["object"].as_str().unwrap_or_default();
        let destination = params["destination"].as_str().unwrap_or_default();
        Ok(ToolResult::success(json!({
            "object": object,
            "destination": destination,
            "pickup_pose": format!("pickup:{object}"),
        })))
    }
}

#[async_trait]
impl ToolExecutor for CheckGripperTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "check_gripper".into(),
            description: "Report the gripper state".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ToolResult::success(json!({ "gripper": "open" })))
    }
}

#[async_trait]
impl ToolExecutor for ApproachPoseTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "approach_pose".into(),
            description: "Approach the pickup pose".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pickup_pose": { "type": "string" },
                    "gripper": { "type": "string" }
                },
                "required": ["pickup_pose", "gripper"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let pickup_pose = params["pickup_pose"].as_str().unwrap_or_default();
        Ok(ToolResult::success(json!({ "at_pose": pickup_pose })))
    }
}

#[async_trait]
impl ToolExecutor for CloseGripperTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "close_gripper".into(),
            description: "Close the gripper around the target".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "object": { "type": "string" }
                },
                "required": ["object"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let object = params["object"].as_str().unwrap_or_default();
        Ok(ToolResult::success(json!({ "held": object })))
    }
}

#[async_trait]
impl ToolExecutor for PlaceObjectTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "place_object".into(),
            description: "Place the held object in the destination".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "held": { "type": "string" },
                    "destination": { "type": "string" }
                },
                "required": ["held", "destination"]
            }),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let held = params["held"].as_str().unwrap_or_default();
        let destination = params["destination"].as_str().unwrap_or_default();
        Ok(ToolResult::success(json!({
            "status": format!("placed:{held}:{destination}")
        })))
    }
}

fn benchmark_auth_identity() -> AuthIdentity {
    AuthIdentity::ApiKey {
        key_id: uuid::Uuid::nil(),
        tenant_id: TenantId::new(uuid::Uuid::nil()),
        scopes: vec![ApiKeyScope::Admin],
    }
}

fn benchmark_dispatcher() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(10));
    dispatcher.register_with_category(Box::new(ExecuteCodeTool), ToolCategory::CodeSandbox);
    dispatcher.register_with_category(Box::new(PlanPickTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(CheckGripperTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(ApproachPoseTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(CloseGripperTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(PlaceObjectTool), ToolCategory::Pure);
    dispatcher
}

fn benchmark_context(dispatcher: &ToolDispatcher) -> ToolContext {
    let mut extensions = Extensions::new();
    extensions.insert(Arc::new(dispatcher.clone()));
    extensions.insert(benchmark_auth_identity());
    ToolContext {
        task_id: "bench-task".into(),
        tenant_id: uuid::Uuid::nil().to_string(),
        call_id: String::new(),
        extensions,
    }
}

fn extract_field<'a>(value: &'a serde_json::Value, field: &str) -> &'a str {
    value[field].as_str().unwrap_or_default()
}

async fn direct_five_roundtrip_chain(dispatcher: &ToolDispatcher, ctx: &ToolContext) -> String {
    let pick = dispatcher
        .dispatch(
            &ToolCall {
                id: "direct-1".into(),
                tool: "plan_pick".into(),
                params: json!({ "object": "red_block", "destination": "tray_a" }),
            },
            ctx,
        )
        .await;
    let grip = dispatcher
        .dispatch(
            &ToolCall {
                id: "direct-2".into(),
                tool: "check_gripper".into(),
                params: json!({}),
            },
            ctx,
        )
        .await;
    let _approach = dispatcher
        .dispatch(
            &ToolCall {
                id: "direct-3".into(),
                tool: "approach_pose".into(),
                params: json!({
                    "pickup_pose": extract_field(&pick.output, "pickup_pose"),
                    "gripper": extract_field(&grip.output, "gripper"),
                }),
            },
            ctx,
        )
        .await;
    let close = dispatcher
        .dispatch(
            &ToolCall {
                id: "direct-4".into(),
                tool: "close_gripper".into(),
                params: json!({
                    "object": extract_field(&pick.output, "object"),
                }),
            },
            ctx,
        )
        .await;
    let place = dispatcher
        .dispatch(
            &ToolCall {
                id: "direct-5".into(),
                tool: "place_object".into(),
                params: json!({
                    "held": extract_field(&close.output, "held"),
                    "destination": extract_field(&pick.output, "destination"),
                }),
            },
            ctx,
        )
        .await;

    extract_field(&place.output, "status").to_string()
}

async fn execute_code_single_roundtrip(dispatcher: &ToolDispatcher, ctx: &ToolContext) -> String {
    let result = dispatcher
        .dispatch(
            &ToolCall {
                id: "sandbox-chain".into(),
                tool: EXECUTE_CODE_TOOL_NAME.into(),
                params: json!({
                    "language": "javascript_qjs",
                    "code": r#"
                        const pick = call_tool("plan_pick", { object: "red_block", destination: "tray_a" });
                        const grip = call_tool("check_gripper", {});
                        call_tool("approach_pose", {
                            pickup_pose: pick.pickup_pose,
                            gripper: grip.gripper
                        });
                        const close = call_tool("close_gripper", { object: pick.object });
                        const place = call_tool("place_object", {
                            held: close.held,
                            destination: pick.destination
                        });
                        print(place.status);
                    "#,
                }),
            },
            ctx,
        )
        .await;

    result.output["output"].as_str().unwrap_or_default().to_string()
}

fn bench_execute_code_roundtrip(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let dispatcher = benchmark_dispatcher();
    let ctx = benchmark_context(&dispatcher);

    let mut group = c.benchmark_group("execute_code_roundtrip");
    group
        .warm_up_time(Duration::from_secs(3))
        .measurement_time(Duration::from_secs(10));

    group.throughput(Throughput::Elements(5));
    group.bench_with_input(
        BenchmarkId::new("pick_and_place_chain", "direct_5_roundtrips"),
        &(&dispatcher, &ctx),
        |b, (dispatcher, ctx)| {
            b.to_async(&runtime).iter(|| async {
                let result = direct_five_roundtrip_chain(black_box(dispatcher), black_box(ctx)).await;
                black_box(result);
            });
        },
    );

    group.throughput(Throughput::Elements(1));
    group.bench_with_input(
        BenchmarkId::new("pick_and_place_chain", "execute_code_1_roundtrip"),
        &(&dispatcher, &ctx),
        |b, (dispatcher, ctx)| {
            b.to_async(&runtime).iter(|| async {
                let result = execute_code_single_roundtrip(black_box(dispatcher), black_box(ctx)).await;
                black_box(result);
            });
        },
    );

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = bench_execute_code_roundtrip
}
criterion_main!(benches);
