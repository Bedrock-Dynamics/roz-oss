//! Phase 26 OBS-03 SC5: 30-second scripted session fixture round-trip.
//!
//! Drives a per-session [`WriterActor`] end-to-end with:
//!   - 1500 telemetry frames (50 Hz × 30 s) → `/tf` + `/roz/telemetry/pose`
//!   - 60 tool-call events (20 triplets) → `/roz/tool/calls`
//!   - 10 approval events (5 pairs) → `/roz/session/events` + `/roz/log`
//!     (driven through [`emit_session_event_for_tests`] — the INTEGRATION
//!     PATH — so a revert of `encode_session_event_proto` to the
//!     iteration-2 `None`-stub regression class would make this test fail)
//!   - 20 task-lifecycle placeholder events → `/roz/task/lifecycle`
//!   - **Phase 26.2 Plan 06 (D-11)**: ONE scripted agent turn via the
//!     [`mock_provider_v1`] path — drives `AgentLoop → SessionRuntime →
//!     EventEmitter → broadcast → spawn_cloud_ingestors → emit_session_event
//!     → MCAP`, exercising EVERY production emit hop that Plan 04's wiring
//!     touches (ModelCallCompleted, in-process
//!     ToolCall{Requested,Started,Finished}). Plan 05's
//!     [`mcap_agent_session_live`] test is the payload-fidelity fence; this
//!     extension makes SC5 ALSO cover the agent-turn path so a revert of
//!     Plan 04's emit sites would fail here, not just the dedicated test.
//!
//! After [`WriteCommand::Finalize`], re-reads the MCAP via
//! [`mcap::MessageStream`] and asserts:
//!   - Total message count `>= min_expected` (lower-bound — the agent-turn
//!     block emits non-deterministic extras like `ActivityChanged`,
//!     `TextDelta`, `ThinkingDelta` that land above the D-10 minimum)
//!   - `/roz/session/events` count `>= APPROVAL_PAIRS*2 +
//!     AGENT_TURN_SESSION_EVENTS_MIN` (6 D-10 BLOCKING variants from the
//!     agent turn, per REVIEWS.md M1 — ReasoningTrace deferred to 26.3)
//!   - `/roz/log` count `>= min_expected_session_events`
//!   - `/tf` and `/roz/telemetry/pose` topics are present
//!   - First `/tf` message decodes to [`FrameTransform`] with non-identity
//!     quaternion components matching the fixture (90° about z)
//!   - `roz_session_mcap_archives` row transitions to `finalized` with
//!     non-null digest + size
//!   - Agent-turn post-assertion: decode `/roz/session/events` envelopes
//!     and verify the 6 D-10 BLOCKING variants + 2 SC5 baseline approval
//!     variants are present (ReasoningTrace tolerated absent per M1).
//!
//! Uses testcontainers for Postgres. NATS is NOT required — the fixture
//! writes telemetry directly via [`WriteCommand`] (bypassing the NATS
//! subscribe step, which is covered separately by cloud/edge parity
//! integration tests); approvals flow through the production converter
//! path via [`emit_session_event_for_tests`]; the agent turn flows through
//! the production cloud-ingestor path via [`spawn_cloud_ingestors`].
//!
//! Run with:
//!   ```text
//!   cargo test -p roz-server --features test-helpers \
//!       --test export_roundtrip -- --ignored --test-threads=1
//!   ```

#![allow(
    clippy::too_many_lines,
    reason = "integration test carries unavoidable fixture scaffolding"
)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use mcap::MessageStream;
use prost::Message as _;
// Phase 26.2 Plan 06 (D-11) — agent-turn drive imports.
// REVIEWS.md H1: mock provider lives at `roz_agent::model::mock_provider_v1`
// (relocated from roz-test to break the dev-dep cycle). roz-server's
// `test-helpers` feature transitively activates `roz-agent/test-helpers`,
// making the `pub use` re-export reachable here.
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
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_core::session::feedback::ApprovalOutcome;
use roz_core::tools::{ToolCategory, ToolResult};
use roz_db::{create_pool, run_migrations};
use roz_server::grpc::roz_v1;
use roz_server::observability::ingest_cloud::{emit_session_event_for_tests, spawn_cloud_ingestors};
use roz_server::observability::mcap_archive::{ChannelKey, FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::projection::{
    FrameTransform, LogLevel, PoseInFrame, Vector3, copper_quat_to_foxglove, log_line, ns_to_proto_timestamp,
    pose_in_frame,
};
use roz_server::observability::schema_registry::SchemaDescriptors;
use roz_server::observability::task_lifecycle::new_task_lifecycle_sink;
use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture constants (D-10 + OBS-03 acceptance).
// ---------------------------------------------------------------------------

const SESSION_DURATION_SECS: u64 = 30;
const TELEMETRY_HZ: u64 = 50;
const TOTAL_TELEMETRY: u64 = SESSION_DURATION_SECS * TELEMETRY_HZ; // 1500
const TOOL_CALL_TRIPLETS: u64 = 20;
const APPROVAL_PAIRS: u64 = 5;
const TASK_LIFECYCLE_TRANSITIONS: u64 = 20;

/// Phase 26.2 Plan 06 (D-11): minimum session-event count delta contributed
/// by ONE run of `mock_provider_v1()` driven through `SessionRuntime`.
///
/// Measured empirically via Plan 05's integration test (see
/// `.planning/phases/26.2-.../26.2-05-SUMMARY.md`):
///
/// ```text
/// DrainReport {
///     saw_turn_started: true,
///     saw_model_call: true,
///     saw_tool_call_requested: true,
///     saw_tool_call_started: true,
///     saw_tool_call_finished: true,
///     saw_turn_finished: true,
///     saw_reasoning_trace: false,
///     total_events: 18,
/// }
/// ```
///
/// Minimum 6 = D-10 BLOCKING variants Plan 04 wires in scope:
///   `TurnStarted + ModelCallCompleted + ToolCallRequested + ToolCallStarted
///    + ToolCallFinished + TurnFinished`.
///
/// ReasoningTrace is DEFERRED per REVIEWS.md M1 — NOT counted. Extras
/// (`ActivityChanged`, `TextDelta`, `ThinkingDelta`, `SessionStarted`) land
/// above the floor and are not required. Stateful mock emits 2x
/// `ModelCallCompleted` (tool_use + end_turn) but the floor counts at least
/// one of each D-10 variant, so the lower-bound is safe.
const AGENT_TURN_SESSION_EVENTS_MIN: u64 = 6;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires testcontainers Postgres + --features test-helpers"]
async fn sc5_30s_fixture_roundtrips_via_mcap_message_stream() {
    // 1. Testcontainers Postgres + migrations + pool.
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    std::mem::forget(guard); // keep container alive past function scope
    let pool = create_pool(&url).await.expect("pool");
    run_migrations(&pool).await.expect("migrate");

    // 2. Create a tenant row (schema requires name + slug). Pin the tenant_id
    //    we want via a post-create UPDATE so the archive row's tenant_id
    //    matches what we pass to `spawn_writer`.
    let tenant_id = Uuid::new_v4();
    let slug = format!("sc5-{}", Uuid::new_v4());
    roz_db::tenant::create_tenant(&pool, "SC5 Test", &slug, "personal")
        .await
        .expect("create tenant");
    sqlx::query("UPDATE roz_tenants SET id = $1 WHERE slug = $2")
        .bind(tenant_id)
        .bind(&slug)
        .execute(&pool)
        .await
        .expect("update tenant id");

    // 3. Tempdir for MCAP output; load schema descriptors up-front.
    let tmp = TempDir::new().expect("tempdir");
    let mcap_dir = std::fs::canonicalize(tmp.path()).expect("canonicalize mcap dir");
    let descriptors = SchemaDescriptors::load().expect("descriptor load");

    // 4. Spawn a WriterActor for this session.
    let session_id = Uuid::new_v4();
    let tx = spawn_writer(mcap_dir.clone(), tenant_id, session_id, descriptors, pool.clone(), None)
        .await
        .expect("spawn writer");

    // 5. Drive fixture messages.
    let now_ns: u64 = 1_700_000_000_000_000_000; // fixed epoch for determinism

    // --- 1500 telemetry frames: /tf (FrameTransform) + /roz/telemetry/pose.
    //
    // Direct WriteCommand injection is acceptable here because the NATS
    // telemetry pipeline is not under test in this fixture (it is covered
    // separately by cloud/edge parity integration tests). Session events go
    // through the production converter (below).
    for i in 0..TOTAL_TELEMETRY {
        let ts = now_ns + i * 20_000_000; // 50 Hz = 20 ms spacing
        let ft = fixture_frame_transform(ts);
        let mut buf = Vec::new();
        ft.encode(&mut buf).expect("encode FrameTransform");
        tx.send(WriteCommand::Event {
            channel: ChannelKey::Tf,
            log_time_ns: ts,
            publish_time_ns: ts,
            bytes: buf,
        })
        .await
        .expect("send /tf");

        let pose = fixture_pose(ts);
        let mut buf = Vec::new();
        pose.encode(&mut buf).expect("encode PoseInFrame");
        tx.send(WriteCommand::Event {
            channel: ChannelKey::Pose,
            log_time_ns: ts,
            publish_time_ns: ts,
            bytes: buf,
        })
        .await
        .expect("send /roz/telemetry/pose");
    }

    // --- 60 tool-call events (3 per triplet: Started / Requested / Finished).
    //    The real `ToolCallEvent` envelope is exercised elsewhere; here we
    //    use a `log_line` stub as a message-count fallback because this
    //    fixture's contract is shape-correct (count + channel presence),
    //    not payload-correct, for tool calls.
    for i in 0..TOOL_CALL_TRIPLETS {
        let ts = now_ns + i * 1_500_000_000;
        for variant in 0..3u8 {
            let stub = log_line(LogLevel::Info, ts, "tool", &format!("tool_{i}_v{variant}"));
            let mut buf = Vec::new();
            stub.encode(&mut buf).expect("encode tool call stub");
            tx.send(WriteCommand::Event {
                channel: ChannelKey::ToolCalls,
                log_time_ns: ts,
                publish_time_ns: ts,
                bytes: buf,
            })
            .await
            .expect("send /roz/tool/calls");
        }
    }

    // --- 10 approval events (2 per pair) — DRIVEN THROUGH THE INTEGRATION
    //     PATH. This is the SC5 anti-regression guard. Each call flows:
    //
    //       EventEnvelope
    //         → emit_session_event_for_tests          (Plan 26-11 Task 1)
    //         → ingest_cloud::emit_session_event      (pub(crate))
    //         → ingest_cloud::encode_session_event_proto
    //         → event_mapper::event_envelope_to_session_response
    //         → WriteCommand::Event { channel: ChannelKey::SessionEvents,
    //                                 bytes: prost_bytes }
    //
    //     A regression that reverts `encode_session_event_proto` to
    //     `return None` would leave `/roz/session/events` empty, so
    //     `session_events_count == 0` and the assertion below would fail.
    for i in 0..APPROVAL_PAIRS {
        for variant in 0..2u8 {
            let envelope = EventEnvelope {
                event_id: EventId::new(),
                correlation_id: CorrelationId::new(),
                parent_event_id: None,
                timestamp: chrono::Utc::now(),
                event: if variant == 0 {
                    SessionEvent::ApprovalRequested {
                        approval_id: format!("approval_{i}"),
                        action: "tool:shell".into(),
                        reason: format!("approval request {i}"),
                        timeout_secs: 300,
                    }
                } else {
                    SessionEvent::ApprovalResolved {
                        approval_id: format!("approval_{i}"),
                        outcome: ApprovalOutcome::Approved,
                    }
                },
                trace_id: None,
                span_id: None,
            };
            emit_session_event_for_tests(&tx, &envelope).await;
        }
    }

    // -----------------------------------------------------------------
    // Phase 26.2 Plan 06 (D-11): scripted agent turn via mock_provider_v1
    // -----------------------------------------------------------------
    // Drives the SAME chain as the mcap_agent_session_live integration test:
    //   mock_provider_v1() → AgentLoop → SessionRuntime → EventEmitter
    //   → broadcast → spawn_cloud_ingestors → emit_session_event → MCAP.
    //
    // Exercises Plan 04's emit wiring (ModelCallCompleted, in-process
    // ToolCall{Requested,Started,Finished}) inside the SC5 regression
    // fence. ReasoningTrace is DEFERRED to Phase 26.3 per REVIEWS.md M1.
    //
    // D-13: this block accepts wall-clock timestamps on /roz/session/events
    // and /roz/log for the agent-turn portion — `emit_session_event` stamps
    // `log_time_ns` via `now_wall_clock_ns()` and the envelope's own
    // `Utc::now()` timestamp is captured at emission. The telemetry block
    // above retains its deterministic-epoch property (pinned to `now_ns`).
    //
    // Construction mirrors Plan 05's `mcap_agent_session_live.rs` verbatim.
    // If that test evolves, this block should too.
    {
        // (a) Build SessionRuntime with test config (mirror Plan 05).
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

        // (b) REVIEWS.md M3: subscribe a drain receiver BEFORE the turn.
        //     Side subscriber used ONLY as a deterministic barrier — the
        //     cloud ingestor below is the one that actually writes MCAP.
        let drain_rx = runtime.subscribe_events();

        // (c) Subscribe cloud ingestors to the SAME writer_tx — `tx` is
        //     the per-session writer sender the outer SC5 fixture already
        //     uses, so the ingestor writes agent-turn events INTO THE
        //     EXISTING MCAP file. This is exactly how production operates.
        let ingestor_rx = runtime.subscribe_events();
        let task_lifecycle_sink = new_task_lifecycle_sink();
        let task_lifecycle_rx = task_lifecycle_sink.subscribe();
        let _agent_turn_cancel = spawn_cloud_ingestors(
            session_id,
            None, // no worker bound
            &tx,
            ingestor_rx,
            task_lifecycle_rx,
            None, // no NATS
            None, // no signing_gate
        );

        // (d) Build ToolDispatcher with an in-process mock "hello_world" tool.
        //     Registered as Pure so dispatch takes the no-safety-gate Pure
        //     branch (agent_loop/dispatch.rs:93-154) — Plan 04's in-process
        //     emit site for ToolCall{Requested,Started,Finished}.
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(MockToolExecutor::new(
                "hello_world",
                ToolResult::success(serde_json::json!({"greeting": "hello, world"})),
            )),
            ToolCategory::Pure,
        );

        // (e) Build AgentLoop with SessionRuntimeEventHook wired in.
        //     REVIEWS.md L1: use existing `runtime.event_emitter()` accessor —
        //     NOT `event_emitter_clone` (which does not exist).
        let event_hook: Arc<dyn AgentEventHook> = Arc::new(SessionRuntimeEventHook::new(runtime.event_emitter()));
        let agent_loop = roz_agent::agent_loop::AgentLoop::new(
            mock_provider_v1(),
            dispatcher,
            SafetyStack::new(vec![]),
            Box::new(MockSpatialContextProvider::empty()),
        )
        .with_agent_event_hook(event_hook);

        // (f) Drive one turn.
        let agent_input = AgentInput::runtime_shell(
            "phase262-sc5-agent-turn",
            tenant_id.to_string(),
            "test-mock-v1", // MUST match mock
            CognitionMode::React,
            3,       // max_cycles
            4096,    // max_tokens
            200_000, // max_context_tokens
            true,    // streaming
            None,    // cancellation_token
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
        eprintln!("SC5 agent-turn turn_result: {turn_result:?}");

        // (g) REVIEWS.md M3: deterministic drain barrier — NOT a fixed sleep.
        //     Poll drain_rx until TurnFinished arrives AND all 5 gap events
        //     are observed, bounded by a 5s timeout. Guarantees the cloud
        //     ingestor (subscribed in (c)) has had a chance to enqueue every
        //     event to the MCAP writer before SC5's downstream Finalize.
        let drain = await_turn_drain(drain_rx, Duration::from_secs(5)).await;
        eprintln!("SC5 agent-turn drain report: {drain:?}");
        assert!(
            drain.saw_turn_finished,
            "SC5 agent-turn: TurnFinished did not arrive within drain timeout; drain={drain:?}"
        );
    }

    // --- 20 task lifecycle transitions (stubbed as log lines; see tool
    //     calls note above — shape-correct, not payload-correct).
    for i in 0..TASK_LIFECYCLE_TRANSITIONS {
        let ts = now_ns + i * 1_500_000_000;
        let stub = log_line(LogLevel::Info, ts, "task", &format!("task_transition_{i}"));
        let mut buf = Vec::new();
        stub.encode(&mut buf).expect("encode task lifecycle stub");
        tx.send(WriteCommand::Event {
            channel: ChannelKey::TaskLifecycle,
            log_time_ns: ts,
            publish_time_ns: ts,
            bytes: buf,
        })
        .await
        .expect("send /roz/task/lifecycle");
    }

    // 6. Finalize. Drop the sender so the WriterActor drain completes.
    tx.send(WriteCommand::Finalize {
        reason: FinalizeReason::SessionCompleted,
    })
    .await
    .expect("send Finalize");
    drop(tx);
    // Poll briefly for the writer task to finish + DB row transition.
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
            .await
            .expect("db lookup");
        if rows.iter().any(|r| r.status == "finalized") {
            break;
        }
    }

    // 7. Re-read the MCAP via MessageStream.
    let file_path = mcap_dir.join(tenant_id.to_string()).join(format!("{session_id}.mcap"));
    let data = std::fs::read(&file_path).expect("mcap file exists after finalize");

    let mut count: u64 = 0;
    let mut session_events_count: u64 = 0;
    let mut log_count: u64 = 0;
    let mut seen_tf = false;
    let mut seen_pose = false;
    let mut tf_non_identity: Option<FrameTransform> = None;
    for msg in MessageStream::new(&data).expect("valid mcap") {
        let msg = msg.expect("valid message");
        match msg.channel.topic.as_str() {
            "/tf" => {
                seen_tf = true;
                if tf_non_identity.is_none() {
                    tf_non_identity = FrameTransform::decode(&*msg.data).ok();
                }
            }
            "/roz/telemetry/pose" => {
                seen_pose = true;
            }
            "/roz/session/events" => {
                session_events_count += 1;
            }
            "/roz/log" => {
                log_count += 1;
            }
            _ => {}
        }
        count += 1;
    }

    // /roz/session/events anti-regression assertion — see approval-loop
    // comment. A revert of `encode_session_event_proto` to a None-stub would
    // make this fail.
    assert!(
        session_events_count > 0,
        "/roz/session/events must receive messages via the integration path; \
         got 0, which indicates encode_session_event_proto regressed to None \
         or the converter path broke (see Plan 26-05 Task 2)",
    );

    // Phase 26.2 Plan 06 (D-11, D-12): the agent-turn block above contributes
    // AGENT_TURN_SESSION_EVENTS_MIN (6) D-10 BLOCKING variants plus
    // non-deterministic extras (ActivityChanged, TextDelta, ThinkingDelta,
    // stateful mock's 2nd ModelCallCompleted, SessionStarted if the runtime
    // emits one). Assertions switch from exact-eq to lower-bound.
    //
    // Lower-bound floor:
    //   approval pairs × 2 (Requested + Resolved) = 10
    //   + agent-turn D-10 minimum = 6
    //   = 16
    let min_expected_session_events = APPROVAL_PAIRS * 2 + AGENT_TURN_SESSION_EVENTS_MIN;
    assert!(
        session_events_count >= min_expected_session_events,
        "/roz/session/events count {} should be at least {} \
         (approval pairs × 2 = {} + agent turn D-10 minimum = {})",
        session_events_count,
        min_expected_session_events,
        APPROVAL_PAIRS * 2,
        AGENT_TURN_SESSION_EVENTS_MIN
    );

    // /roz/log receives the summary line per session event. Lower-bound is
    // the same floor (extras on /roz/session/events → extras on /roz/log).
    assert!(
        log_count >= min_expected_session_events,
        "/roz/log count {log_count} must be at least {min_expected_session_events} \
         (one log line per session event, including agent-turn D-10 minimum)",
    );

    // Fixture totals (lower-bound per D-12):
    //   telemetry: 1500 * 2 = 3000                 (/tf + /roz/telemetry/pose)
    //   tool calls: 20 * 3 = 60                    (/roz/tool/calls)
    //   approvals: 5 * 2 = 10 session_events + 10 log = 20
    //   agent-turn D-10 min: 6 session_events + 6 log = 12
    //   task lifecycle: 20                         (/roz/task/lifecycle)
    //   MIN TOTAL: 3112
    let min_expected = TOTAL_TELEMETRY * 2
        + TOOL_CALL_TRIPLETS * 3
        + APPROVAL_PAIRS * 2 * 2               // approval_session_events + approval_log_lines
        + AGENT_TURN_SESSION_EVENTS_MIN * 2    // agent_turn_session_events + agent_turn_log_lines
        + TASK_LIFECYCLE_TRANSITIONS;
    assert!(
        count >= min_expected,
        "total MCAP message count {count} should be at least {min_expected} \
         (telemetry + tool calls + approvals + agent turn D-10 min + task lifecycle)",
    );
    assert!(seen_tf, "/tf channel present");
    assert!(seen_pose, "/roz/telemetry/pose channel present");

    // Phase 26.2 Plan 06 (D-11) post-assertion: verify the agent turn
    // actually landed on /roz/session/events. Decode each envelope and
    // check the canonical event_type strings from
    // `canonical_event_type_name` in crates/roz-core/src/session/event.rs.
    //
    // A regression that reverts ANY Plan 04 emit site would fail here with
    // a precise pointer to which emit gap returned.
    let mut agent_turn_seen: HashSet<String> = HashSet::new();
    for msg in MessageStream::new(&data).expect("valid mcap") {
        let msg = msg.expect("valid message");
        if msg.channel.topic == "/roz/session/events" {
            let envelope = roz_v1::SessionEventEnvelope::decode(msg.data.as_ref())
                .expect("decode SessionEventEnvelope on /roz/session/events");
            agent_turn_seen.insert(envelope.event_type.clone());
        }
    }
    assert!(
        agent_turn_seen.contains("turn_started"),
        "D-11 post: TurnStarted missing from /roz/session/events. seen={agent_turn_seen:?}"
    );
    assert!(
        agent_turn_seen.contains("model_call"),
        "D-11 post: ModelCallCompleted missing (Plan 04 Gap 1 regression?). seen={agent_turn_seen:?}"
    );
    assert!(
        agent_turn_seen.contains("tool_call_requested"),
        "D-11 post: ToolCallRequested missing (Plan 04 Gap 3 regression — in-process emit at dispatch.rs). \
         seen={agent_turn_seen:?}"
    );
    assert!(
        agent_turn_seen.contains("tool_call_started"),
        "D-11 post: ToolCallStarted missing (Plan 04 Gap 4 regression — should fire at execution site \
         post-safety Pure branch). seen={agent_turn_seen:?}"
    );
    assert!(
        agent_turn_seen.contains("tool_call_finished"),
        "D-11 post: ToolCallFinished missing (Plan 04 Gap 5 regression). seen={agent_turn_seen:?}"
    );
    assert!(
        agent_turn_seen.contains("turn_finished"),
        "D-11 post: TurnFinished missing. seen={agent_turn_seen:?}"
    );
    // SC5 baseline preserved — approval pairs still flow through.
    assert!(
        agent_turn_seen.contains("approval_requested"),
        "SC5 baseline: ApprovalRequested missing. seen={agent_turn_seen:?}"
    );
    assert!(
        agent_turn_seen.contains("approval_resolved"),
        "SC5 baseline: ApprovalResolved missing. seen={agent_turn_seen:?}"
    );
    // ReasoningTrace tolerantly handled (deferred per REVIEWS.md M1).
    if !agent_turn_seen.contains("reasoning_trace") {
        eprintln!(
            "WARN: ReasoningTrace not observed in SC5 — DEFERRED to Phase 26.3 per REVIEWS.md M1. \
             Not a failure."
        );
    }

    // Non-identity quaternion round-trip verification — RESEARCH Pitfall 2.
    // Fixture quat is 90° about z → copper [w,x,y,z] = [sqrt(1/2), 0, 0, sqrt(1/2)]
    // → foxglove {x, y, z, w} = {0, 0, sqrt(1/2), sqrt(1/2)}.
    let ft = tf_non_identity.expect("at least one /tf message decoded");
    let q = ft.rotation.expect("rotation present");
    let half = std::f64::consts::FRAC_1_SQRT_2;
    assert!((q.x - 0.0).abs() < 1e-9, "quaternion x component (got {})", q.x);
    assert!((q.y - 0.0).abs() < 1e-9, "quaternion y component (got {})", q.y);
    assert!((q.z - half).abs() < 1e-9, "quaternion z component (got {})", q.z);
    assert!((q.w - half).abs() < 1e-9, "quaternion w component (got {})", q.w);

    // DB row transitioned to finalized with digest + size populated.
    let rows = roz_db::mcap_archives::list_by_session(&pool, tenant_id, session_id)
        .await
        .expect("db lookup");
    assert_eq!(rows.len(), 1, "expected exactly one archive row");
    assert_eq!(
        rows[0].status, "finalized",
        "status must be finalized after SessionCompleted"
    );
    assert!(
        rows[0].digest_sha256.is_some(),
        "digest_sha256 must be populated on finalize"
    );
    assert!(rows[0].size_bytes > 0, "size_bytes must be positive on finalize");
}

// ---------------------------------------------------------------------------
// Phase 26.2 Plan 06 (REVIEWS.md M3) — deterministic drain barrier.
//
// Inline copy of Plan 05's `await_turn_drain` from
// `crates/roz-server/tests/mcap_agent_session_live.rs`. Kept self-contained
// per Plan 05's convention — cross-test dependency would be awkward for
// isolated integration tests.
// ---------------------------------------------------------------------------

/// Drain report produced by [`await_turn_drain`] (REVIEWS.md M3).
///
/// Captures which canonical `event_type` strings were observed on the
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
// Test-local StreamingTurnExecutor (clone of grpc/agent.rs production pattern).
//
// Mirrors `TestStreamingExecutor` from
// `crates/roz-server/tests/mcap_agent_session_live.rs`. Delegates to
// `AgentLoop::run_streaming_seeded` and surfaces the resulting `TurnOutput`
// through `StreamingTurnHandle::completion`.
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
// Fixture helpers
// ---------------------------------------------------------------------------

/// Build a `FrameTransform` with a non-identity rotation (90° about z).
///
/// Copper's `[w, x, y, z]` convention: `[sqrt(1/2), 0, 0, sqrt(1/2)]`.
/// After `copper_quat_to_foxglove` reorder → foxglove `{x=0, y=0, z=sqrt(1/2), w=sqrt(1/2)}`.
fn fixture_frame_transform(timestamp_ns: u64) -> FrameTransform {
    let half = std::f64::consts::FRAC_1_SQRT_2;
    let copper_wxyz = [half, 0.0, 0.0, half];
    FrameTransform {
        timestamp: Some(ns_to_proto_timestamp(timestamp_ns)),
        parent_frame_id: "world".into(),
        child_frame_id: "base_link".into(),
        translation: Some(Vector3 { x: 1.0, y: 2.0, z: 3.0 }),
        rotation: Some(copper_quat_to_foxglove(copper_wxyz)),
    }
}

fn fixture_pose(timestamp_ns: u64) -> PoseInFrame {
    let half = std::f64::consts::FRAC_1_SQRT_2;
    pose_in_frame("base_link", [1.0, 2.0, 3.0], [half, 0.0, 0.0, half], timestamp_ns)
}
