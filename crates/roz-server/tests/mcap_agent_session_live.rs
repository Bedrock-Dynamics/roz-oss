//! Phase 26.2 SC3: integration test that drives a 1-turn mock-provider
//! agent session end-to-end into MCAP, then asserts the D-10 required
//! SessionEvent variants are present on /roz/session/events with the
//! correct mock-response payload values.
//!
//! This is the regression fence for Plan 04's wiring gap closures
//! (ModelCallCompleted, in-process ToolCall{Requested,Started,Finished}).
//! If any of those emit sites regress, this test fails. ReasoningTrace is
//! DEFERRED to Phase 26.3 (REVIEWS.md M1) — the test tolerates absence.
//!
//! Path B (direct AgentLoop + SessionRuntime drive) chosen over Path A
//! (gRPC AgentService) per RESEARCH.md §"Integration Test Scaffolding".
//!
//! Runs under `cargo test -p roz-server --features test-helpers
//!   --test mcap_agent_session_live -- --ignored --test-threads=1`.
//!
//! REVIEWS.md corrections embedded:
//!   * H1 — mock provider imported from `roz_agent::model::mock_provider_v1`
//!     (relocated from roz-test to avoid dev-dep cycle).
//!   * L1 — hook built via existing `runtime.event_emitter()` accessor
//!     (NOT a renamed `event_emitter_clone`).
//!   * M3 — `await_turn_drain` deterministic barrier replaces any
//!     fixed-duration sleep between turn completion and `Finalize`.
//!   * S1 — proto path resolved to `roz_server::grpc::roz_v1::SessionEventEnvelope`
//!     (matches `ingest_cloud::encode_session_event_proto`).

#![allow(
    clippy::too_many_lines,
    reason = "integration test carries unavoidable fixture scaffolding"
)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use mcap::MessageStream;
use prost::Message as _;
use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoopMode, PresenceSignal};
use roz_agent::dispatch::{MockToolExecutor, ToolDispatcher};
use roz_agent::model::mock_provider_v1;
use roz_agent::safety::SafetyStack;
use roz_agent::session_runtime::{
    PreparedTurn, SessionConfig, SessionRuntime, SessionRuntimeEventHook, StreamingTurnExecutor, StreamingTurnHandle,
    TurnExecutionFailure, TurnInput, TurnOutput,
};
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_core::agent_event_hook::AgentEventHook;
use roz_core::session::activity::RuntimeFailureKind;
use roz_core::session::control::{CognitionMode, SessionMode};
use roz_core::session::event::{EventEnvelope, SessionEvent};
use roz_core::tools::{ToolCategory, ToolResult};
use roz_db::{create_pool, run_migrations};
use roz_server::grpc::roz_v1;
use roz_server::observability::ingest_cloud::spawn_cloud_ingestors;
use roz_server::observability::mcap_archive::{FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::schema_registry::SchemaDescriptors;
use roz_server::observability::task_lifecycle::new_task_lifecycle_sink;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// REVIEWS.md M3 — deterministic drain barrier
// ---------------------------------------------------------------------------

/// Drain report produced by `await_turn_drain` (REVIEWS.md M3).
///
/// Captures which canonical event_type strings were observed on the
/// broadcast by a side-subscriber during the turn. Reported in panic
/// messages so failing assertions immediately surface which emit site
/// regressed (cloud ingestor cannot write what broadcast did not carry).
#[derive(Debug, Default)]
struct DrainReport {
    saw_turn_started: bool,
    saw_model_call: bool,
    saw_tool_call_requested: bool,
    saw_tool_call_started: bool,
    saw_tool_call_finished: bool,
    saw_turn_finished: bool,
    saw_reasoning_trace: bool,
    total_events: usize,
}

/// REVIEWS.md M3: deterministic drain barrier.
///
/// Subscribe a second receiver to the event broadcast BEFORE the turn,
/// then poll it after `run_turn_streaming` until `TurnFinished` arrives
/// AND all expected gap events are observed — or the timeout fires.
///
/// This guarantees the cloud ingestor (also subscribed to the same
/// broadcast) has had the chance to enqueue every event into the MCAP
/// writer before `Finalize` is sent. Replaces the flaky
/// `tokio::time::sleep(200ms)` used in earlier plan drafts.
///
/// The 8x `yield_now` tail gives the background ingestor task a few
/// scheduler ticks to drain its in-flight `WriteCommand::Event` mpsc
/// sends, since each ingestor enqueue is a separate `.await`.
async fn await_turn_drain(mut rx: broadcast::Receiver<EventEnvelope>, timeout: Duration) -> DrainReport {
    let mut report = DrainReport::default();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;

        let envelope = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(env)) => env,
            Ok(Err(_recv_err)) => break, // channel closed or lagged
            Err(_elapsed) => break,      // overall timeout
        };

        report.total_events += 1;

        match &envelope.event {
            SessionEvent::TurnStarted { .. } => report.saw_turn_started = true,
            SessionEvent::ModelCallCompleted { .. } => report.saw_model_call = true,
            SessionEvent::ToolCallRequested { .. } => report.saw_tool_call_requested = true,
            SessionEvent::ToolCallStarted { .. } => report.saw_tool_call_started = true,
            SessionEvent::ToolCallFinished { .. } => report.saw_tool_call_finished = true,
            SessionEvent::ReasoningTrace { .. } => report.saw_reasoning_trace = true,
            SessionEvent::TurnFinished { .. } => {
                report.saw_turn_finished = true;
                if report.saw_turn_started
                    && report.saw_model_call
                    && report.saw_tool_call_requested
                    && report.saw_tool_call_started
                    && report.saw_tool_call_finished
                {
                    break;
                }
            }
            _ => {}
        }
    }

    // Give the cloud ingestor's background task a few scheduler ticks to
    // enqueue the WriteCommand::Event it has already received on its own
    // broadcast subscriber.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    report
}

// ---------------------------------------------------------------------------
// Test-local StreamingTurnExecutor (clone of grpc/agent.rs:890-943 pattern)
// ---------------------------------------------------------------------------

/// Minimal `StreamingTurnExecutor` that delegates to `AgentLoop::run_streaming_seeded`.
/// Mirrors the production `ServerStreamingExecutor` in `crates/roz-server/src/grpc/agent.rs`.
struct TestStreamingExecutor {
    agent_loop: roz_agent::agent_loop::AgentLoop,
    agent_input: AgentInput,
}

impl StreamingTurnExecutor for TestStreamingExecutor {
    fn execute_turn_streaming(&mut self, prepared: PreparedTurn) -> StreamingTurnHandle<'_> {
        let prepared_mode: AgentLoopMode = prepared.cognition_mode();
        let (chunk_tx, chunk_rx) = mpsc::channel(64);
        let (presence_tx, presence_rx) = mpsc::channel::<PresenceSignal>(16);
        // No remote tools — test uses only in-process MockToolExecutor.
        let tool_call_rx = None;

        let system_prompt: Vec<String> = prepared.system_blocks.into_iter().map(|block| block.content).collect();
        let seed = AgentInputSeed::new(system_prompt, prepared.history, prepared.user_message);
        let mut agent_input = self.agent_input.clone();
        agent_input.mode = prepared_mode;

        StreamingTurnHandle {
            completion: Box::pin(async move {
                match self
                    .agent_loop
                    .run_streaming_seeded(agent_input, seed, chunk_tx, presence_tx)
                    .await
                {
                    Ok(output) => Ok(TurnOutput {
                        assistant_message: output.final_response.unwrap_or_default(),
                        tool_calls_made: output.cycles,
                        input_tokens: u64::from(output.total_usage.input_tokens),
                        output_tokens: u64::from(output.total_usage.output_tokens),
                        cache_read_tokens: u64::from(output.total_usage.cache_read_tokens),
                        cache_creation_tokens: u64::from(output.total_usage.cache_creation_tokens),
                        messages: output.messages,
                    }),
                    Err(error) => Err(Box::new(TurnExecutionFailure::new(
                        RuntimeFailureKind::ModelError,
                        error.to_string(),
                    )) as Box<dyn std::error::Error + Send + Sync>),
                }
            }),
            chunk_rx,
            presence_rx,
            tool_call_rx,
        }
    }
}

// ---------------------------------------------------------------------------
// Integration test
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires testcontainers Postgres + --features test-helpers"]
async fn mcap_agent_session_live_emits_expected_variants() {
    // -------------------------------------------------------
    // 1. Testcontainers Postgres + migrations + pool
    // -------------------------------------------------------
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    std::mem::forget(guard);
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrate");

    // -------------------------------------------------------
    // 2. Tenant seeding (mirrors export_roundtrip.rs:82-93)
    // -------------------------------------------------------
    let tenant_id = Uuid::new_v4();
    let slug = format!("phase262-{}", Uuid::new_v4());
    roz_db::tenant::create_tenant(&pool, "Phase 26.2 Test", &slug, "personal")
        .await
        .expect("create tenant");
    sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
        .bind(tenant_id)
        .bind(&slug)
        .execute(&pool)
        .await
        .expect("update tenant id");

    // -------------------------------------------------------
    // 3. MCAP writer
    // -------------------------------------------------------
    let tmp = TempDir::new().expect("tempdir");
    let mcap_dir = std::fs::canonicalize(tmp.path()).expect("canonicalize mcap dir");
    let descriptors = SchemaDescriptors::load().expect("descriptor load");

    let session_id = Uuid::new_v4();
    let writer_tx = spawn_writer(mcap_dir.clone(), tenant_id, session_id, descriptors, pool.clone(), None)
        .await
        .expect("spawn writer");

    // -------------------------------------------------------
    // 4. SessionRuntime + AgentLoop construction (Path B)
    // -------------------------------------------------------
    let session_config = SessionConfig {
        session_id: format!("sess-{session_id}"),
        tenant_id: tenant_id.to_string(),
        mode: SessionMode::Local,
        cognition_mode: CognitionMode::React,
        constitution_text: String::new(),
        blueprint_toml: String::new(),
        model_name: Some("test-mock-v1".to_string()),
        permissions: Vec::new(),
        tool_schemas: Vec::new(),
        project_context: Vec::new(),
        initial_history: Vec::new(),
    };
    let mut runtime = SessionRuntime::new(&session_config);

    // REVIEWS.md M3: subscribe a drain receiver BEFORE the turn so no events
    // can be missed. This is a SECOND subscriber in addition to the cloud
    // ingestor's subscriber below. The cloud ingestor writes to MCAP; this
    // drain_rx is for test-side barrier + diagnostic reporting only.
    let drain_rx = runtime.subscribe_events();

    // Subscribe MCAP cloud ingestor to runtime's event broadcast.
    let ingestor_rx = runtime.subscribe_events();
    let task_lifecycle_sink = new_task_lifecycle_sink();
    let task_lifecycle_rx = task_lifecycle_sink.subscribe();
    let _cancel = spawn_cloud_ingestors(
        session_id,
        None, // no worker bound
        &writer_tx,
        ingestor_rx,
        task_lifecycle_rx,
        None, // no NATS
        None, // no signing_gate
    );

    // Build ToolDispatcher with an in-process "hello_world" mock tool.
    // Register as Pure so dispatch takes the no-safety-gate Pure branch
    // (agent_loop/dispatch.rs:93-154), which emits Requested + Started
    // + Finished in the in-process path that Plan 04 Task 3 added.
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register_with_category(
        Box::new(MockToolExecutor::new(
            "hello_world",
            ToolResult::success(serde_json::json!({"greeting": "hello, world"})),
        )),
        ToolCategory::Pure,
    );

    // Build AgentLoop with mock provider, dispatcher, empty safety stack,
    // empty spatial provider, and the SessionRuntimeEventHook from Plan 04.
    // REVIEWS.md L1: use existing `runtime.event_emitter()` accessor —
    // NOT `event_emitter_clone` (it does not exist; L1 explicitly removed
    // that rename from the plan).
    let event_hook: Arc<dyn AgentEventHook> = Arc::new(SessionRuntimeEventHook::new(runtime.event_emitter()));
    let agent_loop = roz_agent::agent_loop::AgentLoop::new(
        mock_provider_v1(),
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(MockSpatialContextProvider::empty()),
    )
    .with_agent_event_hook(event_hook);

    // -------------------------------------------------------
    // 5. Drive one turn
    // -------------------------------------------------------
    // The test tool is IN-PROCESS (Pure branch); ToolCallRequested is
    // emitted from crates/roz-agent/src/agent_loop/dispatch.rs (Plan 04
    // Task 3), not from session_runtime/mod.rs:1490 (that site is for
    // remote tools only).
    let agent_input = AgentInput::runtime_shell(
        "phase262-test-task",  // task_id
        tenant_id.to_string(), // tenant_id
        "test-mock-v1",        // model_name — MUST match mock
        CognitionMode::React,  // mode
        3,                     // max_cycles
        4096,                  // max_tokens
        200_000,               // max_context_tokens
        true,                  // streaming
        None,                  // cancellation_token
        roz_core::safety::ControlMode::default(),
    );

    let turn_input = TurnInput {
        user_message: "Please say hello.".to_string(),
        cognition_mode: CognitionMode::React,
        custom_context: Vec::new(),
        volatile_blocks: Vec::new(),
    };

    let mut executor = TestStreamingExecutor {
        agent_loop,
        agent_input,
    };
    let message_id = Uuid::new_v4().to_string();

    let turn_result = runtime
        .run_turn_streaming(turn_input, Some(message_id.clone()), &mut executor)
        .await
        .expect("run_turn_streaming");
    eprintln!("turn_result: {turn_result:?}");

    // -------------------------------------------------------
    // 6. REVIEWS.md M3 — deterministic drain barrier
    // -------------------------------------------------------
    let drain = await_turn_drain(drain_rx, Duration::from_secs(5)).await;
    eprintln!("drain report: {drain:?}");
    assert!(
        drain.saw_turn_finished,
        "TurnFinished did not arrive within drain timeout; drain={drain:?}"
    );

    // -------------------------------------------------------
    // 7. Finalize MCAP + poll for finalized status
    // -------------------------------------------------------
    writer_tx
        .send(WriteCommand::Finalize {
            reason: FinalizeReason::SessionCompleted,
        })
        .await
        .expect("send Finalize");
    drop(writer_tx);

    // NOTE: the per-status polling loop below uses tokio::time::sleep,
    // but that is NOT a drain barrier — it is a polling loop for an SQL
    // row transition AFTER Finalize was already sent. REVIEWS.md M3
    // prohibits sleep as a DRAIN barrier specifically; this polling
    // shape is fine.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
            .await
            .expect("db lookup");
        if rows.iter().any(|r| r.status == "finalized") {
            break;
        }
    }

    // -------------------------------------------------------
    // 8. Read + decode MCAP
    // -------------------------------------------------------
    let file_path = mcap_dir.join(tenant_id.to_string()).join(format!("{session_id}.mcap"));
    let data = std::fs::read(&file_path).expect("mcap file exists after finalize");

    let mut seen: HashSet<String> = HashSet::new();
    let mut payloads: Vec<roz_v1::SessionEventEnvelope> = Vec::new();

    for msg in MessageStream::new(&data).expect("valid mcap") {
        let msg = msg.expect("valid message");
        if msg.channel.topic == "/roz/session/events" {
            let envelope =
                roz_v1::SessionEventEnvelope::decode(msg.data.as_ref()).expect("decode SessionEventEnvelope");
            seen.insert(envelope.event_type.clone());
            payloads.push(envelope);
        }
    }

    // -------------------------------------------------------
    // 9. D-10 BLOCKING assertions (presence of 6 required variants)
    // -------------------------------------------------------
    assert!(
        seen.contains("turn_started"),
        "D-10 BLOCKING: TurnStarted missing from /roz/session/events. seen={seen:?} drain={drain:?}"
    );
    assert!(
        seen.contains("model_call"),
        "D-10 BLOCKING: ModelCallCompleted missing (Gap 1 regression?). seen={seen:?} drain={drain:?}"
    );
    assert!(
        seen.contains("tool_call_requested"),
        "D-10 BLOCKING: ToolCallRequested missing (Gap 3 regression — check dispatch.rs in-process emit). \
         seen={seen:?} drain={drain:?}"
    );
    assert!(
        seen.contains("tool_call_started"),
        "D-10 BLOCKING: ToolCallStarted missing (Gap 4 regression — check Plan 04 Task 3 Pure branch). \
         seen={seen:?} drain={drain:?}"
    );
    assert!(
        seen.contains("tool_call_finished"),
        "D-10 BLOCKING: ToolCallFinished missing (Gap 5 regression). seen={seen:?} drain={drain:?}"
    );
    assert!(
        seen.contains("turn_finished"),
        "D-10 BLOCKING: TurnFinished missing. seen={seen:?} drain={drain:?}"
    );

    // D-10 WARN-not-BLOCK — also DEFERRED per REVIEWS.md M1
    if !seen.contains("reasoning_trace") {
        eprintln!(
            "WARN: ReasoningTrace not emitted. Phase 26.2 DEFERRED this emit to Phase 26.3 \
             per REVIEWS.md M1 (ReasoningTraceBuilder::new requires turn_index that AgentLoop \
             does not yet carry). D-10 classifies this as WARN-not-BLOCK."
        );
    }

    // -------------------------------------------------------
    // 10. Payload fidelity — mock-response values round-trip through MCAP
    // -------------------------------------------------------
    // The stateful mock (Plan 05 Rule 1 correction in mock_provider.rs)
    // emits TWO ModelCallCompleted events per turn: call #1 with
    // stop_reason=tool_use, call #2 with stop_reason=end_turn. Assert
    // both shapes and pick the terminal one for the canonical stop_reason
    // fidelity check.
    let mcc_events: Vec<&roz_v1::SessionEventEnvelope> =
        payloads.iter().filter(|e| e.event_type == "model_call").collect();
    assert!(
        mcc_events.len() >= 2,
        "stateful mock should emit >=2 ModelCallCompleted events (tool_use + end_turn); got {}",
        mcc_events.len()
    );

    // All ModelCallCompleted events must carry the canonical mock payload
    // (same model_id, same per-call token usage).
    for mcc in &mcc_events {
        let typed = mcc
            .typed_event
            .as_ref()
            .expect("ModelCallCompleted typed_event populated");
        match typed {
            roz_v1::session_event_envelope::TypedEvent::ModelCallCompleted(p) => {
                assert_eq!(
                    p.model_id, "test-mock-v1",
                    "model_id should match AgentInput.model_name"
                );
                assert_eq!(p.input_tokens, 42, "input_tokens from mock TokenUsage");
                assert_eq!(p.output_tokens, 13, "output_tokens from mock TokenUsage");
            }
            other => panic!("expected ModelCallCompleted typed_event, got {other:?}"),
        }
    }

    // Exactly one terminal ModelCallCompleted with stop_reason=end_turn.
    let terminal_mcc = mcc_events
        .iter()
        .find(|e| {
            matches!(
                e.typed_event.as_ref(),
                Some(roz_v1::session_event_envelope::TypedEvent::ModelCallCompleted(p)) if p.stop_reason == "end_turn"
            )
        })
        .expect("terminal ModelCallCompleted with stop_reason=end_turn");
    // Also prove the tool-use call event exists (stop_reason=tool_use) so
    // regressions that collapse to a single model_call event surface here.
    let _tool_use_mcc = mcc_events
        .iter()
        .find(|e| {
            matches!(
                e.typed_event.as_ref(),
                Some(roz_v1::session_event_envelope::TypedEvent::ModelCallCompleted(p)) if p.stop_reason == "tool_use"
            )
        })
        .expect("ModelCallCompleted with stop_reason=tool_use from call #1");
    let _ = terminal_mcc;

    let tcr = payloads
        .iter()
        .find(|e| e.event_type == "tool_call_requested")
        .expect("ToolCallRequested envelope on /roz/session/events");
    let tcr_typed = tcr
        .typed_event
        .as_ref()
        .expect("ToolCallRequested typed_event populated");
    match tcr_typed {
        roz_v1::session_event_envelope::TypedEvent::ToolCallRequested(p) => {
            assert_eq!(p.tool_name, "hello_world", "tool_name from mock ContentPart::ToolUse");
            assert_eq!(p.call_id, "toolu_mock_1", "call_id from mock ContentPart::ToolUse");
        }
        other => panic!("expected ToolCallRequested typed_event, got {other:?}"),
    }

    // REVIEWS.md M2 — ToolCallStarted.category populated.
    let tcs = payloads
        .iter()
        .find(|e| e.event_type == "tool_call_started")
        .expect("ToolCallStarted envelope on /roz/session/events");
    let tcs_typed = tcs.typed_event.as_ref().expect("ToolCallStarted typed_event populated");
    match tcs_typed {
        roz_v1::session_event_envelope::TypedEvent::ToolCallStarted(p) => {
            assert_eq!(p.tool_name, "hello_world", "tool_name on Started event");
            assert!(
                !p.category.is_empty(),
                "ToolCallStarted.category must be populated per REVIEWS.md M2"
            );
            // Pure-branch Started emit in dispatch.rs:123 sets category="pure".
            assert_eq!(
                p.category, "pure",
                "Pure-branch Started category (dispatch.rs:123 category_str(ToolCategory::Pure))"
            );
        }
        other => panic!("expected ToolCallStarted typed_event, got {other:?}"),
    }

    // -------------------------------------------------------
    // 11. Archive row sanity
    // -------------------------------------------------------
    let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
        .await
        .expect("archive row lookup");
    assert!(
        rows.iter().any(|r| r.status == "finalized"),
        "archive row should transition to finalized after SessionCompleted"
    );
}
