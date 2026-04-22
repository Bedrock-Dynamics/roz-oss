//! Phase 26.3 SC5: trace_context roundtrip.
//!
//! Pins a known root `SpanContext`, drives one mock-provider agent turn,
//! finalizes the MCAP, and asserts every session-events message in that
//! turn carries the pinned `trace_id` bytes. The tool-calls channel is
//! intentionally NOT asserted here (see ROADMAP.md Phase 26.3 SC5 scope
//! narrow + CONTEXT.md Deferred Ideas — no production `ToolCallEvent`
//! producer exists today; session-event variants cover tool calls via
//! `SessionEventEnvelope` which is fully asserted).
//!
//! Reviewer HIGH #7: the test body uses `.instrument(root_span).await` to
//! keep the pinned span active across every `.await` boundary. A lexical
//! RAII guard (see `Span::enter()` docs) does NOT survive tokio `.await`
//! points and is incorrect for this integration test. Grep-verified in
//! Plan 07 acceptance criteria.
//!
//! Path B (direct `AgentLoop` + `SessionRuntime` drive) — forked from
//! `crates/roz-server/tests/mcap_agent_session_live.rs` (Phase 26.2 Plan 05).
//!
//! Runs under `cargo test -p roz-server --features test-helpers
//!   --test trace_context_roundtrip -- --ignored`.

#![cfg(feature = "test-helpers")]
#![allow(
    clippy::too_many_lines,
    reason = "integration test carries unavoidable fixture scaffolding"
)]

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
use roz_core::session::event::EventEnvelope;
use roz_core::tools::{ToolCategory, ToolResult};
use roz_db::{create_pool, run_migrations};
use roz_server::grpc::roz_v1;
use roz_server::observability::ingest_cloud::spawn_cloud_ingestors;
use roz_server::observability::mcap_archive::{FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::schema_registry::SchemaDescriptors;
use roz_server::observability::task_lifecycle::new_task_lifecycle_sink;
use roz_test::make_pinned_span_context;
use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Pinned trace / span byte values (CONTEXT.md §Specifics)
// ---------------------------------------------------------------------------

const PINNED_TRACE_ID: [u8; 16] = [0xFF; 16];
const PINNED_SPAN_ID: [u8; 8] = [0xFE; 8];

// ---------------------------------------------------------------------------
// Deterministic drain barrier (forked from mcap_agent_session_live.rs M3)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct DrainReport {
    saw_turn_started: bool,
    saw_turn_finished: bool,
    total_events: usize,
}

async fn await_turn_drain(mut rx: broadcast::Receiver<EventEnvelope>, timeout: Duration) -> DrainReport {
    use roz_core::session::event::SessionEvent;

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
            SessionEvent::TurnFinished { .. } => {
                report.saw_turn_finished = true;
                // SC5 only needs confirmation the turn completed; we do not
                // gate on the full D-10 set here (the session_live test owns
                // that regression fence).
                break;
            }
            _ => {}
        }
    }

    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    report
}

// ---------------------------------------------------------------------------
// Test-local StreamingTurnExecutor (clone of mcap_agent_session_live.rs)
// ---------------------------------------------------------------------------

struct TestStreamingExecutor {
    agent_loop: roz_agent::agent_loop::AgentLoop,
    agent_input: AgentInput,
}

impl StreamingTurnExecutor for TestStreamingExecutor {
    fn execute_turn_streaming(&mut self, prepared: PreparedTurn) -> StreamingTurnHandle<'_> {
        let prepared_mode: AgentLoopMode = prepared.cognition_mode();
        let (chunk_tx, chunk_rx) = mpsc::channel(64);
        let (presence_tx, presence_rx) = mpsc::channel::<PresenceSignal>(16);
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
async fn trace_context_roundtrip_stamps_pinned_trace_id_on_every_event() {
    // 1. Construct the pinned OTel context + root span.
    //    This is the only step OUTSIDE the instrumented future — it has no
    //    `.await` (no span context to lose).
    let cx = make_pinned_span_context(PINNED_TRACE_ID, PINNED_SPAN_ID);
    let root_span = tracing::info_span!("trace_context_roundtrip_root");
    root_span.set_parent(cx);

    // 2. Everything awaited runs INSIDE the instrumented future so
    //    `Span::current()` resolves to `root_span` on every `.await`.
    //
    //    Reviewer HIGH #7 (T-26.3-33): a lexical RAII guard from
    //    `Span::enter()` does NOT survive tokio `.await` boundaries; the
    //    span's OTel context goes out of scope and `emit_session_event`
    //    reads the default (invalid) context. Using `Instrument::instrument`
    //    on the future keeps the pinned context attached across every
    //    `.await` the agent turn performs.
    async move {
        // --- Postgres + migrations + pool ---
        let guard = roz_test::pg_container().await;
        let url: String = guard.url().to_string();
        std::mem::forget(guard);
        let pool = create_pool(&url).await.expect("pool");
        run_migrations(&pool).await.expect("migrate");

        // --- Tenant seed ---
        let tenant_id = Uuid::new_v4();
        let slug = format!("phase263-{}", Uuid::new_v4());
        roz_db::tenant::create_tenant(&pool, "Phase 26.3 Test", &slug, "personal")
            .await
            .expect("create tenant");
        sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
            .bind(tenant_id)
            .bind(&slug)
            .execute(&pool)
            .await
            .expect("update tenant id");

        // --- MCAP writer ---
        let tmp = TempDir::new().expect("tempdir");
        let mcap_dir = std::fs::canonicalize(tmp.path()).expect("canonicalize mcap dir");
        let descriptors = SchemaDescriptors::load().expect("descriptor load");
        let session_id = Uuid::new_v4();
        let writer_tx = spawn_writer(mcap_dir.clone(), tenant_id, session_id, descriptors, pool.clone(), None)
            .await
            .expect("spawn writer");

        // --- SessionRuntime + AgentLoop construction (Path B) ---
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

        // Subscribe drain barrier BEFORE the turn so no events can be missed.
        let drain_rx = runtime.subscribe_events();

        // Cloud ingestor subscriber — this is the one that writes to MCAP.
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

        // Build ToolDispatcher with an in-process mock tool so the turn
        // exercises the Pure branch (dispatch.rs:93-154). Plan 07 does not
        // assert on tool_call variants — the SC5 scope narrow per Plan 04
        // Task 2 covers only `/roz/session/events` — but including a tool
        // call keeps the turn realistic and exercises the in-process emit
        // sites that also funnel through `emit_session_event`.
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(MockToolExecutor::new(
                "hello_world",
                ToolResult::success(serde_json::json!({"greeting": "hello, world"})),
            )),
            ToolCategory::Pure,
        );

        let event_hook: Arc<dyn AgentEventHook> =
            Arc::new(SessionRuntimeEventHook::new(runtime.event_emitter()));
        let agent_loop = roz_agent::agent_loop::AgentLoop::new(
            mock_provider_v1(),
            dispatcher,
            SafetyStack::new(vec![]),
            Box::new(MockSpatialContextProvider::empty()),
        )
        .with_agent_event_hook(event_hook);

        // --- Drive one turn ---
        let agent_input = AgentInput::runtime_shell(
            "phase263-trace-roundtrip", // task_id
            tenant_id.to_string(),      // tenant_id
            "test-mock-v1",             // model_name — MUST match mock
            CognitionMode::React,       // mode
            3,                          // max_cycles
            4096,                       // max_tokens
            200_000,                    // max_context_tokens
            true,                       // streaming
            None,                       // cancellation_token
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

        // --- Deterministic drain barrier ---
        let drain = await_turn_drain(drain_rx, Duration::from_secs(5)).await;
        eprintln!("drain report: {drain:?}");
        assert!(
            drain.saw_turn_finished,
            "TurnFinished did not arrive within drain timeout; drain={drain:?}"
        );

        // --- Finalize MCAP + poll for finalized status ---
        writer_tx
            .send(WriteCommand::Finalize {
                reason: FinalizeReason::SessionCompleted,
            })
            .await
            .expect("send Finalize");
        drop(writer_tx);

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
                .await
                .expect("db lookup");
            if rows.iter().any(|r| r.status == "finalized") {
                break;
            }
        }

        // --- Read finalized MCAP + decode /roz/session/events ---
        //
        // The tool-calls channel decode is intentionally omitted per Plan 04
        // Task 2 SC5 scope narrow (reviewer HIGH #3 + MEDIUM M6): no
        // production `ToolCallEvent` producer exists today. Tool call
        // coverage in this test comes via the session-event variants
        // (`ToolCallRequested`/`Started`/`Finished`), which ride
        // `/roz/session/events` and ARE asserted below byte-for-byte.
        let file_path = mcap_dir.join(tenant_id.to_string()).join(format!("{session_id}.mcap"));
        let data = std::fs::read(&file_path).expect("mcap file exists after finalize");

        let mut session_envelopes: Vec<roz_v1::SessionEventEnvelope> = Vec::new();
        for msg in MessageStream::new(&data).expect("valid mcap") {
            let msg = msg.expect("valid message");
            if msg.channel.topic.as_str() == "/roz/session/events" {
                let env = roz_v1::SessionEventEnvelope::decode(msg.data.as_ref())
                    .expect("decode SessionEventEnvelope");
                session_envelopes.push(env);
            }
        }

        assert!(
            !session_envelopes.is_empty(),
            "expected at least one /roz/session/events message in the finalized MCAP"
        );

        // --- Byte-for-byte trace_id assertion (SC5 core) ---
        let expected_bytes = PINNED_TRACE_ID.to_vec();
        for env in &session_envelopes {
            assert_eq!(
                env.trace_id, expected_bytes,
                "envelope event_id={} event_type={} must carry pinned trace_id; got {:?}",
                env.event_id, env.event_type, env.trace_id
            );
            assert!(
                !env.span_id.is_empty(),
                "envelope event_id={} event_type={} span_id should not be empty",
                env.event_id,
                env.event_type
            );
        }

        eprintln!(
            "SC5 PASS: {} /roz/session/events envelopes carry pinned trace_id [0xFF; 16]",
            session_envelopes.len()
        );
    }
    .instrument(root_span)
    .await;
}
