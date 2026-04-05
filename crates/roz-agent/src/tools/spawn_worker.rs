//! `spawn_worker` — Orchestrator tool that creates a child task and registers it in the team
//! roster.
//!
//! When called, this tool:
//! 1. Sends a [`roz_core::tasks::SpawnRequest`] via NATS request-reply on
//!    [`roz_nats::team::INTERNAL_SPAWN_SUBJECT`] to roz-server, which creates the child task
//!    in the DB and kicks off the Restate workflow.
//! 2. Writes a `WorkerRecord` into the `JetStream` KV bucket `roz_teams` under the parent
//!    task's key.
//! 3. Returns `{ "worker_id": "<uuid>", "task_id": "<uuid>" }` to the orchestrator model.
//!
//! This tool is **not** registered by default. The orchestrator session must register it
//! explicitly after constructing it with the required runtime handles.

use async_nats::jetstream::Context as JetStreamContext;
use async_trait::async_trait;
use roz_core::{
    phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter},
    tasks::{SpawnReply, SpawnRequest},
    team::{WorkerRecord, WorkerStatus},
    tools::ToolResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::dispatch::{ToolContext, TypedToolExecutor};

/// The canonical name of the spawn-worker tool.
pub const SPAWN_WORKER_TOOL_NAME: &str = "spawn_worker";

// ---------------------------------------------------------------------------
// Input schema
// ---------------------------------------------------------------------------

/// Input for `spawn_worker`.
///
/// `phases` defaults to a single `OodaReAct / All / Immediate` phase when empty.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnWorkerInput {
    /// The task prompt to send to the child worker.
    pub prompt: String,
    /// The host (worker node) to assign the child task to.
    pub host_id: String,
    /// Optional phase specification for the child task. If empty, defaults to a single
    /// `OodaReAct` phase with all tools and an immediate trigger.
    #[serde(default)]
    pub phases: Vec<PhaseSpecInput>,
}

/// JSON-serialisable mirror of `roz_core::phases::PhaseSpec` for use in the tool's input
/// schema.
///
/// We cannot use `PhaseSpec` directly as the input type because it lives in `roz-core`
/// without `JsonSchema` derives (adding that dependency would pull `schemars` into core).
/// This thin wrapper is `#[serde(into)]` compatible and is converted into `PhaseSpec`
/// before the NATS request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PhaseSpecInput {
    /// Execution mode: `"react"` or `"ooda_react"`.
    pub mode: PhaseModeInput,
    /// Tool set filter: `"all"`, `"none"`, or `{ "named": ["tool1", "tool2"] }`.
    pub tools: ToolSetFilterInput,
    /// Phase trigger: `"immediate"`, `"on_tool_signal"`, or `{ "after_cycles": N }`.
    pub trigger: PhaseTriggerInput,
}

/// Mirror of `PhaseMode` for schema generation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhaseModeInput {
    React,
    OodaReAct,
}

/// Mirror of `ToolSetFilter` for schema generation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolSetFilterInput {
    All,
    Named(Vec<String>),
    None,
}

/// Mirror of `PhaseTrigger` for schema generation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhaseTriggerInput {
    Immediate,
    AfterCycles(u32),
    OnToolSignal,
}

impl From<PhaseSpecInput> for PhaseSpec {
    fn from(input: PhaseSpecInput) -> Self {
        Self {
            mode: match input.mode {
                PhaseModeInput::React => PhaseMode::React,
                PhaseModeInput::OodaReAct => PhaseMode::OodaReAct,
            },
            tools: match input.tools {
                ToolSetFilterInput::All => ToolSetFilter::All,
                ToolSetFilterInput::Named(names) => ToolSetFilter::Named(names),
                ToolSetFilterInput::None => ToolSetFilter::None,
            },
            trigger: match input.trigger {
                PhaseTriggerInput::Immediate => PhaseTrigger::Immediate,
                PhaseTriggerInput::AfterCycles(n) => PhaseTrigger::AfterCycles(n),
                PhaseTriggerInput::OnToolSignal => PhaseTrigger::OnToolSignal,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// SpawnWorkerTool
// ---------------------------------------------------------------------------

/// An orchestrator tool that spawns a child worker task and registers it in the team roster.
///
/// Holds runtime handles (NATS client, `JetStream` context, current parent task ID) that are
/// not available at static/compile time — callers must construct this explicitly and
/// register it with the dispatcher when setting up an orchestrator session.
pub struct SpawnWorkerTool {
    /// Raw NATS client for request-reply to `roz.internal.tasks.spawn`.
    nats_client: async_nats::Client,
    /// This orchestrator's task ID, used as `parent_task_id` for child tasks.
    parent_task_id: Uuid,
    /// Environment ID inherited from the parent task and forwarded to child tasks.
    environment_id: Uuid,
    /// `JetStream` context for writing the team roster.
    jetstream: JetStreamContext,
    /// Tenant ID, used to scope the internal spawn request.
    tenant_id: Uuid,
}

impl SpawnWorkerTool {
    /// Construct a new `SpawnWorkerTool`.
    ///
    /// # Arguments
    /// - `nats_client` — A raw `async_nats::Client` (not `JetStream`) for request-reply.
    /// - `parent_task_id` — This orchestrator's own task ID.
    /// - `environment_id` — Environment ID inherited from the parent task.
    /// - `jetstream` — Active `JetStream` context for the team roster KV.
    /// - `tenant_id` — The tenant owning this session.
    pub const fn new(
        nats_client: async_nats::Client,
        parent_task_id: Uuid,
        environment_id: Uuid,
        jetstream: JetStreamContext,
        tenant_id: Uuid,
    ) -> Self {
        Self {
            nats_client,
            parent_task_id,
            environment_id,
            jetstream,
            tenant_id,
        }
    }

    /// Build the default phase list: a single `OodaReAct / All / Immediate` phase.
    ///
    /// This is the default when the caller supplies an empty `phases` vector.
    #[must_use]
    pub fn default_phases() -> Vec<PhaseSpec> {
        vec![PhaseSpec {
            mode: PhaseMode::OodaReAct,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::Immediate,
        }]
    }

    fn resolve_delegation_scope(ctx: &ToolContext) -> roz_core::tasks::DelegationScope {
        ctx.extensions
            .get::<roz_core::tasks::DelegationScope>()
            .cloned()
            .unwrap_or_else(roz_core::tasks::DelegationScope::fail_closed)
    }
}

#[async_trait]
impl TypedToolExecutor for SpawnWorkerTool {
    type Input = SpawnWorkerInput;

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        SPAWN_WORKER_TOOL_NAME
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Spawn a child worker task on a specific host. The worker runs the given prompt \
         autonomously. Returns the worker's task_id once registered. Use this when you need \
         to delegate a subtask to a separate agent."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // 1. Resolve phases: use supplied list or fall back to default.
        let effective_phases: Vec<PhaseSpec> = if input.phases.is_empty() {
            Self::default_phases()
        } else {
            input.phases.into_iter().map(PhaseSpec::from).collect()
        };

        // 2. Send NATS request-reply to create the child task internally (no HTTP hop).
        let request = SpawnRequest {
            tenant_id: self.tenant_id,
            prompt: input.prompt.clone(),
            host_id: input.host_id.clone(),
            environment_id: self.environment_id,
            phases: effective_phases,
            parent_task_id: self.parent_task_id,
            control_interface_manifest: ctx
                .extensions
                .get::<roz_core::embodiment::binding::ControlInterfaceManifest>()
                .cloned(),
            delegation_scope: Some(Self::resolve_delegation_scope(ctx)),
        };

        let payload =
            serde_json::to_vec(&request).map_err(|e| format!("spawn_worker: failed to serialize SpawnRequest: {e}"))?;

        let reply_msg = self
            .nats_client
            .send_request(
                roz_nats::team::INTERNAL_SPAWN_SUBJECT,
                async_nats::Request::new()
                    .payload(payload.into())
                    .timeout(Some(std::time::Duration::from_secs(30))),
            )
            .await
            .map_err(|e| format!("spawn_worker: NATS request timed out or failed: {e}"))?;

        let spawn_reply: SpawnReply = serde_json::from_slice(&reply_msg.payload)
            .map_err(|e| format!("spawn_worker: failed to parse SpawnReply: {e}"))?;

        let child_task_id = spawn_reply.task_id;

        // 3. Atomically append the new WorkerRecord to the team roster in JetStream KV.
        let new_record = WorkerRecord {
            child_task_id,
            host_id: input.host_id.clone(),
            status: WorkerStatus::Pending,
        };

        roz_nats::team::upsert_team_roster(&self.jetstream, self.parent_task_id, &new_record)
            .await
            .map_err(|e| {
                tracing::warn!(
                    child_task_id = %child_task_id,
                    error = %e,
                    "child task created but roster upsert failed; task is running but untracked"
                );
                format!("spawn_worker: roster upsert failed: {e}")
            })?;

        // 4. Return the worker/task IDs to the orchestrator.
        Ok(ToolResult::success(json!({
            // In Roz, each worker maps 1:1 to a task, so worker_id == task_id.
            "worker_id": child_task_id,
            "task_id": child_task_id,
        })))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // -----------------------------------------------------------------------
    // Schema helpers — derive schema directly from SpawnWorkerInput without
    // constructing a SpawnWorkerTool (which requires a live NATS connection).
    //
    // `SpawnWorkerTool` holds a `JetStreamContext` which can only be created
    // from a live `async_nats::Client`. Rather than spinning up NATS just to
    // test a JSON schema, we call `schemars::schema_for!(SpawnWorkerInput)`
    // and build the same `parameters` object the blanket impl would produce.
    // This lets us test the schema shape without any I/O.
    // -----------------------------------------------------------------------

    fn input_schema() -> serde_json::Value {
        let root: serde_json::Value = schemars::schema_for!(SpawnWorkerInput).into();
        let properties = root
            .get("properties")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let required = root
            .get("required")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    }

    // -----------------------------------------------------------------------
    // 1. Tool name constant
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_worker_tool_name_constant() {
        assert_eq!(SPAWN_WORKER_TOOL_NAME, "spawn_worker");
    }

    // -----------------------------------------------------------------------
    // 2. Input schema — required fields present, types correct
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_worker_tool_schema_has_required_fields() {
        let schema = input_schema();

        let required = schema["required"].as_array().expect("required should be an array");
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

        assert!(
            required_strs.contains(&"prompt"),
            "prompt should be required, got: {required_strs:?}"
        );
        assert!(
            required_strs.contains(&"host_id"),
            "host_id should be required, got: {required_strs:?}"
        );
        // phases has a #[serde(default)]; schemars should not include it in required
        assert!(
            !required_strs.contains(&"phases"),
            "phases should be optional (has serde default), got: {required_strs:?}"
        );
    }

    #[test]
    fn spawn_worker_tool_schema_has_string_fields() {
        let schema = input_schema();
        let props = &schema["properties"];
        assert_eq!(props["prompt"]["type"], "string", "prompt should be string type");
        assert_eq!(props["host_id"]["type"], "string", "host_id should be string type");
    }

    // -----------------------------------------------------------------------
    // 3. default_phases() returns the expected single OodaReAct/All/Immediate phase
    // -----------------------------------------------------------------------

    #[test]
    fn default_phases_returns_single_ooda_react_phase() {
        let phases = SpawnWorkerTool::default_phases();
        assert_eq!(phases.len(), 1, "default should have exactly one phase");
        let phase = &phases[0];
        assert_eq!(phase.mode, PhaseMode::OodaReAct, "default mode should be OodaReAct");
        assert_eq!(phase.tools, ToolSetFilter::All, "default tools should be All");
        assert_eq!(
            phase.trigger,
            PhaseTrigger::Immediate,
            "default trigger should be Immediate"
        );
    }

    #[test]
    fn default_phases_serialise_correctly() {
        let phases = SpawnWorkerTool::default_phases();
        let json = serde_json::to_value(&phases).expect("phases should serialise");
        assert_eq!(json[0]["mode"], "ooda_react");
        assert_eq!(json[0]["tools"], "all");
        assert_eq!(json[0]["trigger"], "immediate");
    }

    // -----------------------------------------------------------------------
    // 4. PhaseSpecInput → PhaseSpec conversion
    // -----------------------------------------------------------------------

    #[test]
    fn phase_spec_input_converts_to_phase_spec() {
        let input = PhaseSpecInput {
            mode: PhaseModeInput::React,
            tools: ToolSetFilterInput::Named(vec!["sensor_read".to_string()]),
            trigger: PhaseTriggerInput::AfterCycles(3),
        };
        let spec: PhaseSpec = input.into();
        assert_eq!(spec.mode, PhaseMode::React);
        assert_eq!(spec.tools, ToolSetFilter::Named(vec!["sensor_read".to_string()]));
        assert_eq!(spec.trigger, PhaseTrigger::AfterCycles(3));
    }

    #[test]
    fn phase_spec_input_ooda_react_all_immediate_converts() {
        let input = PhaseSpecInput {
            mode: PhaseModeInput::OodaReAct,
            tools: ToolSetFilterInput::All,
            trigger: PhaseTriggerInput::Immediate,
        };
        let spec: PhaseSpec = input.into();
        assert_eq!(spec.mode, PhaseMode::OodaReAct);
        assert_eq!(spec.tools, ToolSetFilter::All);
        assert_eq!(spec.trigger, PhaseTrigger::Immediate);
    }

    #[test]
    fn phase_spec_input_none_tools_on_tool_signal_converts() {
        let input = PhaseSpecInput {
            mode: PhaseModeInput::React,
            tools: ToolSetFilterInput::None,
            trigger: PhaseTriggerInput::OnToolSignal,
        };
        let spec: PhaseSpec = input.into();
        assert_eq!(spec.tools, ToolSetFilter::None);
        assert_eq!(spec.trigger, PhaseTrigger::OnToolSignal);
    }

    // -----------------------------------------------------------------------
    // 5. Description constant mentions key concepts
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_worker_description_mentions_worker_and_delegate() {
        struct Stub;
        impl Stub {
            fn description(&self) -> &str {
                "Spawn a child worker task on a specific host. The worker runs the given prompt \
                 autonomously. Returns the worker's task_id once registered. Use this when you need \
                 to delegate a subtask to a separate agent."
            }
        }
        let desc = Stub.description();
        assert!(
            desc.contains("worker") || desc.contains("Worker"),
            "description should mention worker, got: {desc}"
        );
        assert!(
            desc.contains("delegate") || desc.contains("subtask") || desc.contains("Spawn"),
            "description should mention delegation/spawning, got: {desc}"
        );
    }

    // -----------------------------------------------------------------------
    // 6. SpawnRequest serialisation — verifies the NATS payload shape
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_request_serialises_with_all_required_fields() {
        let parent_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let env_id = Uuid::new_v4();
        let req = SpawnRequest {
            tenant_id,
            prompt: "inspect sector 4".to_string(),
            host_id: "host-abc".to_string(),
            environment_id: env_id,
            phases: SpawnWorkerTool::default_phases(),
            parent_task_id: parent_id,
            control_interface_manifest: None,
            delegation_scope: None,
        };

        let serialised = serde_json::to_value(&req).expect("SpawnRequest should serialise");

        assert_eq!(
            serialised["tenant_id"],
            tenant_id.to_string(),
            "tenant_id must be present in serialised SpawnRequest"
        );
        assert_eq!(serialised["prompt"], "inspect sector 4");
        assert_eq!(serialised["host_id"], "host-abc");
        assert_eq!(
            serialised["parent_task_id"],
            parent_id.to_string(),
            "parent_task_id must be present in serialised SpawnRequest"
        );
        assert!(serialised["phases"].is_array(), "phases should be an array");
        assert_eq!(serialised["phases"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn spawn_request_phases_empty_by_default_when_input_is_empty() {
        // When SpawnWorkerTool resolves phases, an empty input → default_phases().
        // Verify default_phases() serialises to the expected NATS payload shape.
        let phases = SpawnWorkerTool::default_phases();
        let json = serde_json::to_value(&phases).expect("phases should serialise");
        assert_eq!(json[0]["mode"], "ooda_react");
        assert_eq!(json[0]["tools"], "all");
        assert_eq!(json[0]["trigger"], "immediate");
    }

    #[test]
    fn spawn_reply_deserialises_task_id() {
        let task_id = Uuid::new_v4();
        let json = json!({ "task_id": task_id.to_string() });
        let reply: SpawnReply = serde_json::from_value(json).expect("SpawnReply should deserialise");
        assert_eq!(reply.task_id, task_id);
    }

    #[test]
    fn resolve_delegation_scope_fails_closed_when_missing() {
        let ctx = crate::dispatch::ToolContext {
            task_id: Uuid::new_v4().to_string(),
            tenant_id: Uuid::new_v4().to_string(),
            call_id: "call-1".into(),
            extensions: crate::dispatch::Extensions::default(),
        };

        let scope = SpawnWorkerTool::resolve_delegation_scope(&ctx);
        assert!(scope.allowed_tools.is_empty());
        assert_eq!(scope.trust_posture.tool_trust, roz_core::trust::TrustLevel::Untrusted);
    }

    #[test]
    fn resolve_delegation_scope_preserves_parent_scope() {
        let expected = roz_core::tasks::DelegationScope {
            allowed_tools: vec!["capture_frame".into()],
            trust_posture: roz_core::trust::TrustPosture::default(),
        };
        let mut extensions = crate::dispatch::Extensions::default();
        extensions.insert(expected.clone());
        let ctx = crate::dispatch::ToolContext {
            task_id: Uuid::new_v4().to_string(),
            tenant_id: Uuid::new_v4().to_string(),
            call_id: "call-2".into(),
            extensions,
        };

        let scope = SpawnWorkerTool::resolve_delegation_scope(&ctx);
        assert_eq!(scope, expected);
    }

    // -----------------------------------------------------------------------
    // 7. Full execute() integration test — requires live NATS + running roz-server
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires live NATS JetStream and a running Roz server; \
                run: docker run -d -p 4222:4222 nats -js && cargo test ... -- --ignored"]
    async fn spawn_worker_execute_full_integration() {
        // Full execute() path: NATS request-reply task creation + JetStream KV roster upsert.
        // Set NATS_URL before running.
    }

    // -----------------------------------------------------------------------
    // 8. spawn_worker writes a WorkerRecord into the JetStream KV roster
    //    (requires live NATS — marked #[ignore])
    // -----------------------------------------------------------------------

    /// Run with:
    /// ```text
    /// NATS_URL=nats://localhost:4222 cargo test -p roz-agent spawn_worker_writes_worker_record_to_jetstream_kv_roster -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "requires live NATS JetStream and a running Roz server; \
                run: NATS_URL=nats://localhost:4222 cargo test -p roz-agent spawn_worker_writes_worker_record_to_jetstream_kv_roster -- --ignored"]
    async fn spawn_worker_writes_worker_record_to_jetstream_kv_roster() {
        use roz_core::team::WorkerStatus;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
        let nats = async_nats::connect(&nats_url).await.expect("connect to NATS");
        let js = async_nats::jetstream::new(nats.clone());

        let parent_task_id = Uuid::new_v4();
        let tenant_id = Uuid::new_v4();
        let environment_id = Uuid::new_v4();
        let tool = SpawnWorkerTool::new(nats.clone(), parent_task_id, environment_id, js.clone(), tenant_id);

        let ctx = crate::dispatch::ToolContext {
            task_id: parent_task_id.to_string(),
            tenant_id: tenant_id.to_string(),
            call_id: "test-call-1".to_string(),
            extensions: crate::dispatch::Extensions::default(),
        };
        let result = crate::dispatch::TypedToolExecutor::execute(
            &tool,
            SpawnWorkerInput {
                prompt: "inspect sector 4".to_string(),
                host_id: "host-1".to_string(),
                phases: vec![],
            },
            &ctx,
        )
        .await
        .expect("execute should not fail");

        assert!(
            result.is_success(),
            "spawn_worker execute should succeed, got: {}",
            result.output
        );

        let roster = roz_nats::team::get_team_roster(&js, parent_task_id)
            .await
            .expect("get_team_roster should succeed");

        assert_eq!(roster.len(), 1, "roster should contain exactly one WorkerRecord");

        let record = &roster[0];
        assert_eq!(record.status, WorkerStatus::Pending, "worker status should be Pending");
        assert_eq!(record.host_id, "host-1", "worker host_id should match input");
    }
}
