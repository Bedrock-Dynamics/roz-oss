//! gRPC `AgentService` implementation — bidirectional streaming session state machine.
//!
//! The first message from the client must be `StartSession`. This triggers:
//! 1. Auth validation (via `grpc_auth_middleware` -- see `middleware/grpc_auth.rs`).
//! 2. Session metadata written to Postgres (`roz_agent_sessions`).
//! 3. `SessionStarted` acknowledgement sent back with session ID, resolved model, and permissions.
//!
//! After `StartSession`, `UserMessage` dispatches an `AgentLoop::run_streaming()` turn,
//! forwarding streaming deltas to the client. `ToolResult` messages resolve pending
//! remote tool calls. `CancelTurn` / `CancelSession` handle lifecycle cleanup.

#![allow(clippy::significant_drop_tightening)]

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::Stream;
use sqlx::PgPool;
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode};
use roz_agent::constitution::build_constitution;
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::dispatch::remote::{PendingResults, RemoteToolCall, RemoteToolExecutor, resolve_pending};
use roz_agent::model::types::StreamChunk;
use roz_agent::safety::SafetyStack;
use roz_agent::session_runtime::{
    SessionRuntime, StreamingTurnExecutor, StreamingTurnHandle, StreamingTurnResult, TurnExecutionFailure, TurnOutput,
};
use roz_agent::spatial_provider::{
    NullWorldStateProvider, bootstrap_runtime_world_state_provider, format_runtime_world_state_bootstrap_note,
};
use roz_core::auth::AuthIdentity;
use roz_core::edge_health::EdgeTransportHealth;
use roz_core::session::event::SessionPermissionRule;
use roz_core::session::event::{CanonicalSessionEventEnvelope, CorrelationId, EventEnvelope, SessionEvent};
use roz_core::tools::ToolCategory;

use roz_core::team::{SequencedTeamEvent, TeamEvent as CoreTeamEvent, WorkerFailReason};
use roz_nats::subjects::Subjects;
use roz_nats::team::{TEAM_STREAM, team_subject_pattern};

use super::convert::value_to_struct;
use super::roz_v1::agent_service_server::AgentService;
use super::roz_v1::{self, SessionRequest, SessionResponse, WatchTeamRequest, session_request, session_response};
use crate::grpc::auth_ext;

/// Keepalive interval while an agent turn is in progress.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Default tool timeout for remote tool execution.
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// In-flight session state held for the lifetime of a single `StreamSession` call.
struct Session {
    id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    environment_id: uuid::Uuid,
    model_name: String,
    max_context_tokens: u32,
    tools: Vec<roz_core::tools::ToolSchema>,
    /// Proto-declared categories per tool (for dispatcher registration).
    tool_categories: HashMap<String, ToolCategory>,
    /// Client-provided project context (AGENTS.md, .substrate/rules/*.md, etc.)
    /// sent once at session start. Included in the system prompt for every turn.
    project_context: Vec<String>,
    #[allow(dead_code)]
    cancel_token: tokio_util::sync::CancellationToken,
    #[allow(dead_code)]
    base_permissions: Vec<SessionPermissionRule>,
    #[allow(dead_code)]
    active_permissions: Vec<SessionPermissionRule>,
    pub host_id: Option<String>,
    /// Resolved worker name (host.name) corresponding to `host_id`.
    /// Cached at session start to avoid repeated DB lookups per message.
    pub worker_name: Option<String>,
    /// Whether this session runs on the edge worker (true) or cloud server (false).
    is_edge: bool,
    /// Canonical session lifecycle tracker for cloud sessions.
    runtime: Option<Arc<AsyncMutex<SessionRuntime>>>,
    /// Mirrored checkpoint cache for edge sessions.
    ///
    /// After handoff, the worker remains the active runtime authority. The
    /// server keeps only the latest exported checkpoint state so it can relay
    /// reconnects and expose session metadata without acting like a second
    /// runtime owner.
    edge_mirror: Option<Arc<AsyncMutex<EdgeSessionMirror>>>,
    /// Subscription to the canonical runtime event stream for cloud forwarding.
    event_rx: Option<broadcast::Receiver<EventEnvelope>>,
}

struct EdgeSessionMirror {
    bootstrap: roz_agent::session_runtime::SessionRuntimeBootstrap,
}

impl EdgeSessionMirror {
    const fn new(bootstrap: roz_agent::session_runtime::SessionRuntimeBootstrap) -> Self {
        Self { bootstrap }
    }

    fn export_bootstrap(&self) -> roz_agent::session_runtime::SessionRuntimeBootstrap {
        self.bootstrap.clone()
    }

    fn update_checkpoint(&mut self, bootstrap: roz_agent::session_runtime::SessionRuntimeBootstrap) {
        self.bootstrap = bootstrap;
    }

    const fn history_len(&self) -> usize {
        self.bootstrap.history.len()
    }

    fn model_name(&self) -> String {
        self.bootstrap.model_name.clone().unwrap_or_default()
    }
}

/// State for an active agent turn, shared between the session loop and relay tasks.
struct ActiveTurn {
    /// Resolves pending remote tool calls when the client sends `ToolResult`.
    pending: PendingResults,
    /// Canonical runtime authority for resolving approvals on the active turn.
    runtime: Arc<AsyncMutex<SessionRuntime>>,
    /// Cancel token scoped to this turn.
    cancel_token: tokio_util::sync::CancellationToken,
    /// Handle to the spawned agent task (dropped on turn completion).
    _handle: tokio::task::JoinHandle<()>,
}

enum TurnCompletion {
    Completed(TurnOutput),
    Cancelled,
    Failed(roz_core::session::activity::RuntimeFailureKind),
}

// ---------------------------------------------------------------------------
// AgentServiceImpl
// ---------------------------------------------------------------------------

/// gRPC implementation of the `AgentService` trait.
///
/// Holds its own dependencies rather than referencing the axum `AppState`,
/// since this module lives in the library crate while `AppState` is defined
/// in the binary crate.
pub struct AgentServiceImpl {
    pool: PgPool,
    #[allow(dead_code)]
    http_client: reqwest::Client,
    #[allow(dead_code)]
    restate_ingress_url: String,
    nats_client: Option<async_nats::Client>,
    default_model: String,
    gateway_url: String,
    api_key: String,
    model_timeout_secs: u64,
    anthropic_provider: String,
    direct_api_key: Option<String>,
    fallback_model_name: Option<String>,
    /// Usage metering — passed to each `AgentLoop` for budget checks and recording.
    meter: Arc<dyn roz_agent::meter::UsageMeter>,
}

impl AgentServiceImpl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        http_client: reqwest::Client,
        restate_ingress_url: String,
        nats_client: Option<async_nats::Client>,
        default_model: String,
        gateway_url: String,
        api_key: String,
        model_timeout_secs: u64,
        anthropic_provider: String,
        direct_api_key: Option<String>,
        fallback_model_name: Option<String>,
        meter: Arc<dyn roz_agent::meter::UsageMeter>,
    ) -> Self {
        Self {
            pool,
            http_client,
            restate_ingress_url,
            nats_client,
            default_model,
            gateway_url,
            api_key,
            model_timeout_secs,
            anthropic_provider,
            direct_api_key,
            fallback_model_name,
            meter,
        }
    }
}

#[tonic::async_trait]
impl AgentService for AgentServiceImpl {
    type StreamSessionStream = Pin<Box<dyn Stream<Item = Result<SessionResponse, Status>> + Send>>;

    async fn stream_session(
        &self,
        request: Request<Streaming<SessionRequest>>,
    ) -> Result<Response<Self::StreamSessionStream>, Status> {
        let (tx, rx) = mpsc::channel::<Result<SessionResponse, Status>>(32);

        // Clone deps for the spawned task.
        let pool = self.pool.clone();
        let default_model = self.default_model.clone();
        let gateway_url = self.gateway_url.clone();
        let api_key = self.api_key.clone();
        let model_timeout_secs = self.model_timeout_secs;
        let anthropic_provider = self.anthropic_provider.clone();
        let direct_api_key = self.direct_api_key.clone();
        let fallback_model_name = self.fallback_model_name.clone();
        let nats_client = self.nats_client.clone();
        let meter = self.meter.clone();

        // Extract AuthIdentity from request extensions (set by grpc_auth_middleware)
        // BEFORE consuming the request via into_inner().
        let auth_identity = request
            .extensions()
            .get::<AuthIdentity>()
            .cloned()
            .ok_or_else(|| Status::internal("auth identity missing from extensions"))?;

        let mut inbound = request.into_inner();

        tokio::spawn(async move {
            let model_config = ModelConfig {
                gateway_url,
                api_key,
                timeout_secs: model_timeout_secs,
                anthropic_provider,
                direct_api_key,
                fallback_model_name,
            };
            run_session_loop(
                &tx,
                &pool,
                &default_model,
                &model_config,
                auth_identity,
                &mut inbound,
                nats_client.as_ref(),
                meter,
            )
            .await;
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }

    type WatchTeamStream = Pin<Box<dyn Stream<Item = Result<roz_v1::TeamEvent, Status>> + Send>>;

    #[tracing::instrument(
        name = "agent_service.watch_team",
        skip(self, request),
        fields(task_id = tracing::field::Empty, tenant_id = tracing::field::Empty)
    )]
    async fn watch_team(&self, request: Request<WatchTeamRequest>) -> Result<Response<Self::WatchTeamStream>, Status> {
        // --- Auth (extracted from request extensions by grpc_auth_middleware) ---
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));

        // --- Parse task_id ---
        let body = request.into_inner();
        let task_id = uuid::Uuid::parse_str(&body.task_id)
            .map_err(|_| Status::invalid_argument("task_id is not a valid UUID"))?;

        tracing::Span::current().record("task_id", tracing::field::display(task_id));

        // --- Verify task belongs to this tenant ---
        let task_row = roz_db::tasks::get_by_id(&self.pool, task_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, %task_id, "failed to fetch task for WatchTeam");
                Status::internal("database error")
            })?
            .ok_or_else(|| Status::not_found("task not found"))?;

        // Explicit tenant_id check — defense-in-depth beyond RLS.
        if task_row.tenant_id != tenant_id {
            return Err(Status::not_found("task not found"));
        }

        tracing::debug!(task_id = %task_row.id, "WatchTeam: task ownership verified");

        // --- NATS JetStream consumer ---
        let nats_client = self
            .nats_client
            .clone()
            .ok_or_else(|| Status::unavailable("NATS is not configured"))?;

        let js = async_nats::jetstream::new(nats_client);
        let filter_subject = team_subject_pattern(task_id);

        // Open an ephemeral ordered consumer on the team event stream,
        // filtered to this task's workers.
        let stream = js.get_stream(TEAM_STREAM).await.map_err(|e| {
            tracing::warn!(error = %e, %task_id, "WatchTeam: team stream not found or unavailable");
            Status::unavailable("team event stream unavailable")
        })?;

        let consumer = stream
            .create_consumer(async_nats::jetstream::consumer::push::OrderedConfig {
                filter_subject,
                ..Default::default()
            })
            .await
            .map_err(|e| {
                tracing::error!(error = %e, %task_id, "WatchTeam: failed to create consumer");
                Status::internal("failed to create NATS consumer")
            })?;

        let mut messages = consumer.messages().await.map_err(|e| {
            tracing::error!(error = %e, %task_id, "WatchTeam: failed to subscribe");
            Status::internal("failed to subscribe to team events")
        })?;

        // --- Relay loop ---
        let (tx, rx) = mpsc::channel::<Result<roz_v1::TeamEvent, Status>>(64);

        // Spawns a background task that forwards NATS messages to the gRPC client stream.
        // Three termination conditions: (1) client disconnects (tx.send returns Err),
        // (2) NATS stream closes (messages.next() returns None), or
        // (3) NATS message error (messages.next() returns Some(Err)).
        tokio::spawn(async move {
            loop {
                match messages.next().await {
                    Some(Ok(msg)) => {
                        // Ack the message so it's not redelivered.
                        if let Err(e) = msg.ack().await {
                            tracing::warn!(error = %e, "WatchTeam: failed to ack message");
                        }

                        let Some(core_event) = decode_team_event_payload(&msg.payload) else {
                            tracing::warn!("WatchTeam: failed to decode TeamEvent payload, skipping");
                            continue;
                        };

                        let proto_event = core_team_event_to_proto(core_event);
                        if tx.send(Ok(proto_event)).await.is_err() {
                            // Client disconnected.
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "WatchTeam: NATS message error");
                        let _ = tx.send(Err(Status::internal("NATS stream error"))).await;
                        break;
                    }
                    None => {
                        // Stream closed.
                        break;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type AnalyzeMediaStream = Pin<Box<dyn Stream<Item = Result<roz_v1::AnalyzeMediaChunk, Status>> + Send>>;

    // Phase 16.1 placeholder. Downstream plans (MED-02..MED-04) wire in the
    // MediaBackend trait, SSRF-guarded fetcher, and Gemini routing. This stub
    // exists only so the generated tonic trait is satisfied and roz-server
    // continues to compile after MED-01 proto additions.
    async fn analyze_media(
        &self,
        _request: Request<roz_v1::AnalyzeMediaRequest>,
    ) -> Result<Response<Self::AnalyzeMediaStream>, Status> {
        Err(Status::unimplemented(
            "AnalyzeMedia not yet implemented (Phase 16.1 — MED-02/03/04)",
        ))
    }
}

// ---------------------------------------------------------------------------
// Model configuration passed to the session loop
// ---------------------------------------------------------------------------

struct ModelConfig {
    gateway_url: String,
    api_key: String,
    timeout_secs: u64,
    anthropic_provider: String,
    direct_api_key: Option<String>,
    fallback_model_name: Option<String>,
}

struct ServerStreamingExecutor {
    agent_loop: AgentLoop,
    agent_input: AgentInput,
    tool_request_rx: Option<mpsc::Receiver<RemoteToolCall>>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl StreamingTurnExecutor for ServerStreamingExecutor {
    fn execute_turn_streaming(
        &mut self,
        prepared: roz_agent::session_runtime::PreparedTurn,
    ) -> StreamingTurnHandle<'_> {
        let prepared_agent_mode: AgentLoopMode = prepared.cognition_mode();
        let (chunk_tx, chunk_rx) = mpsc::channel::<StreamChunk>(64);
        let (presence_tx, presence_rx) = mpsc::channel::<roz_agent::agent_loop::PresenceSignal>(16);
        let tool_call_rx = self.tool_request_rx.take();
        let cancellation = self.cancellation.clone();
        let system_prompt: Vec<String> = prepared.system_blocks.into_iter().map(|block| block.content).collect();
        let seed = AgentInputSeed::new(system_prompt, prepared.history, prepared.user_message);
        let mut agent_input = self.agent_input.clone();
        agent_input.mode = prepared_agent_mode;

        StreamingTurnHandle {
            completion: Box::pin(async move {
                let result = tokio::select! {
                    res = self.agent_loop.run_streaming_seeded(agent_input, seed, chunk_tx, presence_tx) => res,
                    () = cancellation.cancelled() => {
                        Err(roz_agent::error::AgentError::Cancelled {
                            partial_input_tokens: 0,
                            partial_output_tokens: 0,
                        })
                    }
                };

                match result {
                    Ok(output) => Ok(TurnOutput {
                        assistant_message: output.final_response.unwrap_or_default(),
                        tool_calls_made: output.cycles,
                        input_tokens: u64::from(output.total_usage.input_tokens),
                        output_tokens: u64::from(output.total_usage.output_tokens),
                        cache_read_tokens: u64::from(output.total_usage.cache_read_tokens),
                        cache_creation_tokens: u64::from(output.total_usage.cache_creation_tokens),
                        messages: output.messages,
                    }),
                    Err(error) => Err(Box::new(agent_error_to_turn_execution_failure(error))
                        as Box<dyn std::error::Error + Send + Sync>),
                }
            }),
            chunk_rx,
            presence_rx,
            tool_call_rx,
        }
    }
}

// ---------------------------------------------------------------------------
// Session message loop
// ---------------------------------------------------------------------------

#[tracing::instrument(
    name = "agent_session.stream",
    skip(tx, pool, model_config, inbound, nats_client, meter, auth_identity),
    fields(session_id = tracing::field::Empty, tenant_id = tracing::field::Empty, environment_id = tracing::field::Empty, model = %default_model)
)]
#[expect(
    clippy::too_many_lines,
    reason = "session loop is inherently sequential with many arms"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "session loop needs all its dependencies passed in"
)]
async fn run_session_loop(
    tx: &mpsc::Sender<Result<SessionResponse, Status>>,
    pool: &PgPool,
    default_model: &str,
    model_config: &ModelConfig,
    auth_identity: AuthIdentity,
    inbound: &mut Streaming<SessionRequest>,
    nats_client: Option<&async_nats::Client>,
    meter: Arc<dyn roz_agent::meter::UsageMeter>,
) {
    let mut session: Option<Session> = None;
    let mut cancelled = false;
    let mut active_turn: Option<ActiveTurn> = None;
    // When a turn completes, the agent task sends the output (or None on error)
    // so the session loop can update messages, run compaction, and allow the next turn.
    let (turn_done_tx, mut turn_done_rx) = mpsc::channel::<TurnCompletion>(1);
    // Cancellation token for telemetry and WebRTC signaling relay tasks.
    // Cancelled when the session loop exits to stop the infinite relay loops.
    let relay_cancel = tokio_util::sync::CancellationToken::new();

    // DEBT-03: session-scoped write-behind persistence.
    // The emitter is spawned when the session is established (post `handle_start`)
    // and cancelled alongside other relay tasks via `relay_cancel`.
    // Edge sessions (is_edge == true) are deferred — see TODO below.
    let mut turn_emitter: Option<roz_agent::agent_loop::TurnEmitter> = None;

    loop {
        // Wait for either the next inbound message or a turn-done signal.
        let req = tokio::select! {
            msg = inbound.next() => {
                match msg {
                    Some(Ok(r)) => r,
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "client stream error");
                        break;
                    }
                    None => break, // client closed stream
                }
            }
            turn_output = turn_done_rx.recv() => {
                if let Some(turn_output) = turn_output
                    && let Some(ref mut sess) = session
                {
                    match turn_output {
                        TurnCompletion::Completed(output) => {
                            tracing::info!(
                                session_id = %sess.id,
                                tenant_id = %sess.tenant_id,
                                environment_id = %sess.environment_id,
                                model = %sess.model_name,
                                input_tokens = output.input_tokens,
                                output_tokens = output.output_tokens,
                                tool_calls = output.tool_calls_made,
                                "agent_session.turn_complete"
                            );
                            if let Err(e) = roz_db::agent_sessions::update_session_usage(
                                pool,
                                sess.id,
                                i64::try_from(output.input_tokens).unwrap_or(i64::MAX),
                                i64::try_from(output.output_tokens).unwrap_or(i64::MAX),
                                1,
                            )
                            .await
                            {
                                tracing::warn!(session_id = %sess.id, error = %e, "failed to update session usage");
                            }
                            let runtime = sess
                                .runtime
                                .as_ref()
                                .expect("cloud sessions must retain runtime authority");
                            sess.event_rx = Some({
                                let runtime = runtime.lock().await;
                                runtime.subscribe_events()
                            });
                            {
                                let mut runtime = runtime.lock().await;
                                runtime.compact_context(sess.max_context_tokens).await;
                            }
                            if !drain_cloud_runtime_events(
                                tx,
                                sess.event_rx
                                    .as_mut()
                                    .expect("cloud sessions must retain event stream"),
                                &sess.model_name,
                                &sess.active_permissions,
                            )
                            .await
                            {
                                break;
                            }
                        }
                        TurnCompletion::Cancelled => {
                            let runtime = sess
                                .runtime
                                .as_ref()
                                .expect("cloud sessions must retain runtime authority");
                            sess.event_rx = Some({
                                let runtime = runtime.lock().await;
                                runtime.subscribe_events()
                            });
                        }
                        TurnCompletion::Failed(failure) => {
                            tracing::warn!(session_id = %sess.id, ?failure, "agent turn failed");
                            let runtime = sess
                                .runtime
                                .as_ref()
                                .expect("cloud sessions must retain runtime authority");
                            sess.event_rx = Some({
                                let runtime = runtime.lock().await;
                                runtime.subscribe_events()
                            });
                        }
                    }
                }
                active_turn = None;
                continue;
            }
        };

        match req.request {
            Some(session_request::Request::Start(start)) => {
                if !handle_start(tx, pool, default_model, &auth_identity, start, &mut session).await {
                    break;
                }
                if let Some(ref sess) = session {
                    let history_messages = {
                        if let Some(runtime) = &sess.runtime {
                            let runtime = runtime.lock().await;
                            runtime.history().len()
                        } else {
                            match sess.edge_mirror.as_ref() {
                                Some(mirror) => {
                                    let mirror = mirror.lock().await;
                                    mirror.history_len()
                                }
                                None => 0,
                            }
                        }
                    };
                    tracing::Span::current().record("session_id", tracing::field::display(sess.id));
                    tracing::Span::current().record("tenant_id", tracing::field::display(sess.tenant_id));
                    tracing::Span::current().record("environment_id", tracing::field::display(sess.environment_id));
                    tracing::info!(
                        session_id = %sess.id,
                        tenant_id = %sess.tenant_id,
                        environment_id = %sess.environment_id,
                        model = %sess.model_name,
                        tools_count = sess.tools.len(),
                        history_messages,
                        project_context_count = sess.project_context.len(),
                        "agent_session.started"
                    );

                    // Spawn telemetry relay: subscribe to NATS telemetry for the
                    // session's host and forward updates on the gRPC stream.
                    if let Some(ref worker_name) = sess.worker_name
                        && let Some(nats) = nats_client
                    {
                        let host_id_for_telem = sess.host_id.clone().unwrap_or_else(|| worker_name.clone());
                        spawn_telemetry_relay(nats, worker_name, &host_id_for_telem, tx, relay_cancel.clone()).await;
                        spawn_webrtc_signaling_relay(nats, worker_name, tx, relay_cancel.clone()).await;
                    }

                    // DEBT-03: spawn write-behind flush task for cloud (non-edge) sessions.
                    // TODO(phase 13+): edge session turn persistence deferred — the server
                    // would double-persist if the worker also emitted. See 13-01 plan.
                    if !sess.is_edge && turn_emitter.is_none() {
                        let (emitter, rx) = roz_agent::agent_loop::TurnEmitter::new();
                        turn_emitter = Some(emitter);
                        let flush_cancel = relay_cancel.child_token();
                        let flush_pool = pool.clone();
                        tokio::spawn(async move {
                            roz_agent::agent_loop::run_flush_task(rx, flush_pool, flush_cancel).await;
                        });
                    }

                    // Edge mode: relay the entire session to the worker via NATS
                    // instead of running the agent loop on the server.
                    if sess.is_edge {
                        let (Some(host_id_str), Some(nats)) = (&sess.host_id, nats_client) else {
                            send_error(
                                tx,
                                "invalid_argument",
                                "edge placement requires host_id and NATS",
                                false,
                            )
                            .await;
                            relay_cancel.cancel();
                            return;
                        };
                        let Ok(host_uuid) = uuid::Uuid::parse_str(host_id_str) else {
                            send_error(tx, "invalid_argument", "host_id is not a valid UUID", false).await;
                            relay_cancel.cancel();
                            return;
                        };
                        match roz_db::hosts::get_by_id(pool, host_uuid).await {
                            Ok(Some(host)) => {
                                tracing::info!(
                                    session_id = %sess.id,
                                    host_name = %host.name,
                                    "routing session to edge worker"
                                );
                                let bootstrap = sess
                                    .edge_mirror
                                    .clone()
                                    .expect("edge sessions must retain mirrored checkpoint state");
                                run_edge_relay(tx, nats, &host.name, &sess.id.to_string(), bootstrap, inbound).await;
                                relay_cancel.cancel();
                                return; // session done
                            }
                            Ok(None) => {
                                send_error(tx, "not_found", "host not found for edge placement", false).await;
                                relay_cancel.cancel();
                                return;
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "failed to look up host for edge relay");
                                send_error(tx, "internal", "failed to resolve edge host", true).await;
                                relay_cancel.cancel();
                                return;
                            }
                        }
                    }
                }
            }
            Some(session_request::Request::UserMessage(msg)) => {
                let Some(ref mut sess) = session else {
                    send_error(tx, "no_session", "send StartSession first", false).await;
                    continue;
                };

                if active_turn.is_some() {
                    send_error(tx, "turn_in_progress", "a turn is already in progress", false).await;
                    continue;
                }

                // Primary always routes through the gateway (PAIG or equivalent).
                // Never pass direct_api_key to the primary — that would bypass the
                // gateway entirely, defeating the FallbackModel's purpose.
                let primary = match roz_agent::model::create_model(
                    &sess.model_name,
                    &model_config.gateway_url,
                    &model_config.api_key,
                    model_config.timeout_secs,
                    &model_config.anthropic_provider,
                    None, // primary always via gateway
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        send_error(tx, "model_error", &format!("failed to create model: {e}"), true).await;
                        continue;
                    }
                };

                // Build fallback: prefer a named fallback model; otherwise
                // auto-fall back to the same model via direct Anthropic when
                // ROZ_ANTHROPIC_API_KEY is set (gateway degradation path).
                let model: Box<dyn roz_agent::model::types::Model> = {
                    let fallback_result = if let Some(ref fallback_name) = model_config.fallback_model_name {
                        // Explicit fallback model — use direct_api_key for it.
                        roz_agent::model::create_model(
                            fallback_name,
                            &model_config.gateway_url,
                            &model_config.api_key,
                            model_config.timeout_secs,
                            &model_config.anthropic_provider,
                            model_config.direct_api_key.as_deref(),
                        )
                        .map(|m| (fallback_name.as_str(), m))
                    } else if let Some(ref direct_key) = model_config.direct_api_key {
                        // No named fallback but direct key present: auto-create
                        // a same-model fallback that bypasses the gateway.
                        roz_agent::model::create_model(
                            &sess.model_name,
                            &model_config.gateway_url,
                            &model_config.api_key,
                            model_config.timeout_secs,
                            &model_config.anthropic_provider,
                            Some(direct_key.as_str()),
                        )
                        .map(|m| (sess.model_name.as_str(), m))
                    } else {
                        Err(roz_agent::error::AgentError::UnsupportedModel { name: "(none)".into() })
                    };

                    match fallback_result {
                        Ok((name, fallback)) => {
                            tracing::info!(fallback_model = %name, "model fallback configured");
                            Box::new(roz_agent::model::FallbackModel::new(primary, fallback))
                        }
                        Err(_) => primary,
                    }
                };

                // Set up remote tool channels.
                let (tool_request_tx, tool_request_rx) = mpsc::channel::<RemoteToolCall>(16);
                let pending: PendingResults = Arc::new(Mutex::new(HashMap::new()));

                let turn_runtime = sess
                    .runtime
                    .clone()
                    .expect("cloud sessions must retain runtime authority");
                let current_mode = {
                    let runtime = turn_runtime.lock().await;
                    runtime.cognition_mode()
                };
                let tools_changed = if msg.tools.is_empty() {
                    false
                } else {
                    sess.tool_categories = msg
                        .tools
                        .iter()
                        .map(|tool| (tool.name.clone(), proto_tool_category(tool)))
                        .collect();
                    sess.tools = msg
                        .tools
                        .iter()
                        .cloned()
                        .map(roz_core::tools::ToolSchema::from)
                        .collect();
                    sess.active_permissions = derive_permissions(&sess.tools);
                    true
                };

                let mode = resolved_user_message_mode(msg.ai_mode.as_deref(), current_mode);
                let session_project_context = sess.project_context.clone();
                let active_permissions = sess.active_permissions.clone();
                if tools_changed || mode != current_mode {
                    let mut runtime = turn_runtime.lock().await;
                    sync_cloud_runtime_surface(
                        &mut runtime,
                        mode,
                        &sess.tools,
                        &session_project_context,
                        &active_permissions,
                    );
                }

                // Register remote tool executors for the current session-owned tool inventory.
                let mut dispatcher = ToolDispatcher::new(DEFAULT_TOOL_TIMEOUT);
                let turn_tools = sess.tools.clone();
                let turn_categories = sess.tool_categories.clone();
                for tool in &turn_tools {
                    let category = turn_categories
                        .get(&tool.name)
                        .copied()
                        .unwrap_or(ToolCategory::Physical);
                    dispatcher.register_with_category(
                        Box::new(RemoteToolExecutor::new(
                            &tool.name,
                            &tool.description,
                            tool.parameters.clone(),
                            tool_request_tx.clone(),
                            pending.clone(),
                            DEFAULT_TOOL_TIMEOUT,
                        )),
                        category,
                    );
                }

                // Build AgentInput from session state + user message.
                // The agent loop will add the user message to its messages internally,
                // and the returned AgentOutput.messages will include it.
                let user_content = msg.content;
                let volatile_blocks = prompt_context_blocks(&msg.context);
                let inline_system_context = msg.system_context.filter(|ctx| !ctx.trim().is_empty());
                let approval_runtime = {
                    let runtime = turn_runtime.lock().await;
                    runtime.approval_handle()
                };

                let agent_input = AgentInput::runtime_shell(
                    sess.id.to_string(),
                    sess.tenant_id.to_string(),
                    sess.model_name.clone(),
                    mode,
                    200, // safety ceiling, not behavioral limit
                    8192,
                    sess.max_context_tokens,
                    true,
                    None,
                    roz_core::safety::ControlMode::default(),
                );

                let safety = SafetyStack::new(vec![]);
                let spatial_bootstrap =
                    bootstrap_runtime_world_state_provider(Box::new(NullWorldStateProvider), &sess.id.to_string())
                        .await;
                let runtime_spatial_note = {
                    let base = format_runtime_world_state_bootstrap_note(
                        "server_runtime",
                        spatial_bootstrap.runtime_spatial_context(),
                        "no server-side spatial provider is bound for this turn",
                    );
                    if matches!(mode, AgentLoopMode::React) {
                        format!(
                            "{base} Current turn mode is React, so the runtime did not bind spatial state as active observation."
                        )
                    } else {
                        base
                    }
                };
                let runtime_spatial_context = if matches!(mode, AgentLoopMode::OodaReAct) {
                    spatial_bootstrap.runtime_spatial_context().cloned()
                } else {
                    None
                };
                let spatial = spatial_bootstrap.provider;
                let agent_loop = AgentLoop::new(model, dispatcher, safety, spatial)
                    .with_approval_runtime(approval_runtime.clone())
                    .with_meter(meter.clone())
                    .with_turn_emitter_opt(turn_emitter.clone());

                let turn_cancel = tokio_util::sync::CancellationToken::new();
                let message_id = msg
                    .message_id
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let mut turn_event_rx = {
                    let runtime = turn_runtime.lock().await;
                    runtime.subscribe_events()
                };

                tracing::info!(
                    session_id = %sess.id,
                    message_id = %message_id,
                    "agent_session.turn_started"
                );

                let relay_tx = tx.clone();
                let relay_model_name = sess.model_name.clone();
                let relay_permissions = sess.active_permissions.clone();
                let turn_cancel_relay = turn_cancel.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            recv = turn_event_rx.recv() => {
                                match recv {
                                    Ok(envelope) => {
                                        let terminal = matches!(
                                            envelope.event,
                                            SessionEvent::TurnFinished { .. } | SessionEvent::SessionFailed { .. }
                                        );
                                        let response = cloud_event_envelope_to_response(
                                            &envelope,
                                            &relay_model_name,
                                            &relay_permissions,
                                        );
                                        if relay_tx
                                            .send(Ok(SessionResponse { response: Some(response) }))
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                        if terminal {
                                            break;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                        tracing::warn!(skipped, "cloud turn event stream lagged");
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                }
                            }
                            () = turn_cancel_relay.cancelled() => {
                                loop {
                                    match turn_event_rx.try_recv() {
                                        Ok(envelope) => {
                                            let response = cloud_event_envelope_to_response(
                                                &envelope,
                                                &relay_model_name,
                                                &relay_permissions,
                                            );
                                            if relay_tx
                                                .send(Ok(SessionResponse { response: Some(response) }))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => break,
                                        Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                                            tracing::warn!(skipped, "cloud turn event stream lagged");
                                        }
                                    }
                                }
                                break;
                            }
                        }
                    }
                });

                let mut executor = ServerStreamingExecutor {
                    agent_loop,
                    agent_input,
                    tool_request_rx: Some(tool_request_rx),
                    cancellation: turn_cancel.clone(),
                };
                let agent_tx = tx.clone();
                let turn_done = turn_done_tx.clone();
                let turn_cancel_task = turn_cancel.clone();
                let runtime_for_turn = turn_runtime.clone();
                let handle = tokio::spawn(async move {
                    let result = {
                        let mut runtime = runtime_for_turn.lock().await;
                        let custom_context = runtime
                            .turn_prompt_staging()
                            .take_turn_custom_context(inline_system_context);
                        runtime.sync_world_state_with_note(runtime_spatial_context, Some(runtime_spatial_note));
                        let turn_input = roz_agent::session_runtime::TurnInput {
                            user_message: user_content,
                            cognition_mode: mode,
                            custom_context,
                            volatile_blocks,
                        };
                        runtime
                            .run_turn_streaming(turn_input, Some(message_id), &mut executor)
                            .await
                    };
                    turn_cancel_task.cancel();

                    match result {
                        Ok(StreamingTurnResult::Completed(output)) => {
                            let _ = turn_done.send(TurnCompletion::Completed(output)).await;
                        }
                        Ok(StreamingTurnResult::Cancelled) => {
                            let _ = turn_done.send(TurnCompletion::Cancelled).await;
                        }
                        Err(roz_agent::session_runtime::SessionRuntimeError::SessionFailed(failure)) => {
                            let _ = turn_done.send(TurnCompletion::Failed(failure)).await;
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "streaming turn rejected before execution");
                            let _ = agent_tx
                                .send(Ok(SessionResponse {
                                    response: Some(super::event_mapper::canonical_session_event_to_response(
                                        SessionEvent::SessionRejected {
                                            code: "turn_rejected".into(),
                                            message: error.to_string(),
                                            retryable: false,
                                        },
                                        CorrelationId::new(),
                                    )),
                                }))
                                .await;
                            let _ = turn_done
                                .send(TurnCompletion::Failed(
                                    roz_core::session::activity::RuntimeFailureKind::ModelError,
                                ))
                                .await;
                        }
                    }
                });

                active_turn = Some(ActiveTurn {
                    pending: pending.clone(),
                    runtime: turn_runtime.clone(),
                    cancel_token: turn_cancel.clone(),
                    _handle: handle,
                });

                // Spawn keepalive task.
                let keepalive_tx = tx.clone();
                let keepalive_cancel = turn_cancel.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(KEEPALIVE_INTERVAL);
                    interval.tick().await; // skip initial tick
                    loop {
                        tokio::select! {
                            _ = interval.tick() => {
                                let resp = SessionResponse {
                                    response: Some(session_response::Response::Keepalive(roz_v1::Keepalive {
                                        tokens_used: None,
                                        tokens_max: None,
                                    })),
                                };
                                if keepalive_tx.send(Ok(resp)).await.is_err() {
                                    break;
                                }
                            }
                            () = keepalive_cancel.cancelled() => break,
                        }
                    }
                });
            }
            Some(session_request::Request::ToolResult(result)) => {
                let Some(ref sess) = session else {
                    send_error(tx, "no_session", "send StartSession first", false).await;
                    continue;
                };
                let _ = sess; // suppress unused warning (we validate session exists)

                let Some(ref turn) = active_turn else {
                    send_error(
                        tx,
                        "no_active_turn",
                        "no turn in progress to receive tool results",
                        false,
                    )
                    .await;
                    continue;
                };

                // Build a domain ToolResult from the proto ToolResult.
                // Try parsing the result as JSON; fall back to wrapping as a string.
                // D1: also forward structured metadata (exit_code, truncated,
                // duration_ms) so they appear in model conversation history.
                let tool_result = if result.success {
                    let value =
                        serde_json::from_str(&result.result).unwrap_or(serde_json::Value::String(result.result));
                    roz_core::tools::ToolResult {
                        output: value,
                        error: None,
                        exit_code: result.exit_code,
                        truncated: result.truncated,
                        duration_ms: result.duration_ms.and_then(|d| {
                            u64::try_from(d)
                                .map_err(|_| {
                                    tracing::warn!(duration_ms = d, "ToolResult duration_ms is negative, dropping");
                                })
                                .ok()
                        }),
                    }
                } else {
                    roz_core::tools::ToolResult {
                        output: serde_json::Value::Null,
                        error: Some(result.result),
                        exit_code: result.exit_code,
                        truncated: result.truncated,
                        duration_ms: result.duration_ms.and_then(|d| {
                            u64::try_from(d)
                                .map_err(|_| {
                                    tracing::warn!(duration_ms = d, "ToolResult duration_ms is negative, dropping");
                                })
                                .ok()
                        }),
                    }
                };

                let resolved = resolve_pending(&turn.pending, &result.tool_call_id, tool_result);
                if !resolved {
                    tracing::warn!(
                        tool_call_id = %result.tool_call_id,
                        "ToolResult for unknown or already-resolved call"
                    );
                }
            }
            Some(session_request::Request::CancelTurn(_)) => {
                if let Some(ref turn) = active_turn {
                    turn.cancel_token.cancel();
                    // active_turn is cleared when turn_done_rx fires,
                    // preventing new turns from starting before the cancelled
                    // turn fully drains.
                }
            }
            Some(session_request::Request::CancelSession(_)) => {
                if let Some(ref turn) = active_turn {
                    turn.cancel_token.cancel();
                }
                cancelled = true;
                break;
            }
            Some(session_request::Request::Ping(_)) => {
                let _ = tx
                    .send(Ok(SessionResponse {
                        response: Some(session_response::Response::Pong(roz_v1::Pong {})),
                    }))
                    .await;
            }
            Some(session_request::Request::Feedback(fb)) => {
                if let Some(ref sess) = session {
                    let session_uuid = uuid::Uuid::parse_str(&fb.session_id).unwrap_or(sess.id);
                    if let Err(e) = roz_db::message_feedback::upsert_feedback(
                        pool,
                        sess.tenant_id,
                        session_uuid,
                        &fb.message_id,
                        &fb.rating,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "Failed to store message feedback");
                    }
                }
            }
            // D2: IDE sends user's Deny/Allow decision for a Roz-authoritative approval.
            Some(session_request::Request::PermissionDecision(decision)) => {
                if let Some(ref turn) = active_turn {
                    let modifier = decision.modifier.map(super::tasks::prost_struct_to_json);
                    let resolved = {
                        let runtime = turn.runtime.lock().await;
                        runtime.resolve_approval(&decision.approval_id, decision.approved, modifier)
                    };
                    if !resolved {
                        tracing::warn!(
                            approval_id = %decision.approval_id,
                            approved = decision.approved,
                            "PermissionDecision for unknown or already-resolved approval"
                        );
                    }
                } else {
                    tracing::warn!(
                        approval_id = %decision.approval_id,
                        "PermissionDecision received but no active turn"
                    );
                }
            }
            // D3: Hot-swap a named set of tools without restarting the session.
            Some(session_request::Request::RegisterTools(reg)) => {
                let Some(ref mut sess) = session else {
                    send_error(tx, "no_session", "send StartSession first", false).await;
                    continue;
                };

                let source = reg.source.as_deref().unwrap_or("").to_string();

                // Remove all tools previously registered under this source.
                if !source.is_empty() {
                    let prefix = format!("{source}__");
                    sess.tools.retain(|t| !t.name.starts_with(prefix.as_str()));
                    sess.tool_categories
                        .retain(|name, _| !name.starts_with(prefix.as_str()));
                }

                // Extract categories before converting proto tools.
                let new_categories: HashMap<String, ToolCategory> = reg
                    .tools
                    .iter()
                    .map(|tool| (tool.name.clone(), proto_tool_category(tool)))
                    .collect();

                // Append new tools (empty list = unregister only).
                let new_tools: Vec<roz_core::tools::ToolSchema> =
                    reg.tools.into_iter().map(roz_core::tools::ToolSchema::from).collect();

                let n_added = new_tools.len();
                sess.tools.extend(new_tools);
                sess.tool_categories.extend(new_categories);
                sess.active_permissions = derive_permissions(&sess.tools);

                let runtime = sess.runtime.clone();
                let session_id = sess.id;
                let total_tools = sess.tools.len();
                let session_project_context = sess.project_context.clone();
                let active_permissions = sess.active_permissions.clone();

                if let Some(runtime) = runtime {
                    let mut runtime = runtime.lock().await;
                    let mode: AgentLoopMode = runtime.cognition_mode();
                    sync_cloud_runtime_surface(
                        &mut runtime,
                        mode,
                        &sess.tools,
                        &session_project_context,
                        &active_permissions,
                    );
                    if let Some(ctx) = reg.system_context.clone().filter(|ctx| !ctx.trim().is_empty()) {
                        runtime.turn_prompt_staging().stage_system_context(Some(ctx));
                    }
                }

                tracing::info!(
                    session_id = %session_id,
                    source = %source,
                    tools_added = n_added,
                    total_tools = total_tools,
                    "agent_session.register_tools"
                );
            }
            // WebRTC answer: serialize and publish to NATS for the worker.
            Some(session_request::Request::WebrtcAnswer(answer)) => {
                if let (Some(sess), Some(nats)) = (&session, nats_client) {
                    if let Some(worker_id) = sess.worker_name.as_deref() {
                        relay_webrtc_answer(nats, worker_id, &answer).await;
                    } else {
                        tracing::debug!("WebRTC answer received but no worker_name on session");
                    }
                } else {
                    tracing::debug!("WebRTC answer received but session or NATS unavailable");
                }
            }
            // ICE candidate: serialize and publish to NATS for the worker.
            Some(session_request::Request::IceCandidate(candidate)) => {
                if let (Some(sess), Some(nats)) = (&session, nats_client) {
                    if let Some(worker_id) = sess.worker_name.as_deref() {
                        relay_ice_candidate(nats, worker_id, &candidate).await;
                    } else {
                        tracing::debug!("ICE candidate received but no worker_name on session");
                    }
                } else {
                    tracing::debug!("ICE candidate received but session or NATS unavailable");
                }
            }
            // Camera request: serialize and publish to NATS for the worker.
            Some(session_request::Request::CameraRequest(cam_req)) => {
                if let (Some(sess), Some(nats)) = (&session, nats_client) {
                    if let Some(worker_id) = sess.worker_name.as_deref() {
                        relay_camera_request(nats, worker_id, &cam_req).await;
                    } else {
                        tracing::debug!("camera request received but no worker_name on session");
                    }
                } else {
                    tracing::debug!("camera request received but session or NATS unavailable");
                }
            }
            None => {}
        }
    }

    // Cancel telemetry and WebRTC signaling relay tasks.
    relay_cancel.cancel();

    // Cancel any in-flight turn.
    if let Some(ref turn) = active_turn {
        turn.cancel_token.cancel();
    }

    if let Some(ref mut s) = session {
        let should_complete = if let Some(runtime) = &s.runtime {
            let runtime = runtime.lock().await;
            !runtime.has_failed() && !runtime.is_completed()
        } else {
            false
        };
        if should_complete {
            let summary = if cancelled {
                "cancelled by client"
            } else {
                "session completed"
            };
            let completed = {
                let mut runtime = s
                    .runtime
                    .as_ref()
                    .expect("cloud sessions must retain runtime authority")
                    .lock()
                    .await;
                runtime.complete_session(summary).await.is_ok()
            };
            if completed {
                let _ = drain_cloud_runtime_events(
                    tx,
                    s.event_rx.as_mut().expect("cloud sessions must retain event stream"),
                    &s.model_name,
                    &s.active_permissions,
                )
                .await;
            }
        }
    }

    // Cleanup: mark session with the appropriate terminal status.
    let status = if cancelled { "cancelled" } else { "completed" };
    if let Some(ref s) = session {
        tracing::info!(
            session_id = %s.id,
            tenant_id = %s.tenant_id,
            environment_id = %s.environment_id,
            model = %s.model_name,
            status = status,
            "agent_session.ended"
        );
    }
    if let Some(ref s) = session
        && let Err(e) = roz_db::agent_sessions::complete_session(pool, s.id, status).await
    {
        tracing::warn!(session_id = %s.id, error = %e, "failed to complete session");
    }
}

/// Subscribe to NATS telemetry for a worker and relay `TelemetryUpdate` messages
/// on the gRPC response stream.
///
/// Subscribes to `telemetry.{worker_name}.>`. Each received NATS message is
/// converted to a `TelemetryUpdate` proto and forwarded on `tx`.
/// The loop exits when `cancel` is cancelled (session ended) or the client disconnects.
async fn spawn_telemetry_relay(
    nats: &async_nats::Client,
    worker_name: &str,
    host_id: &str,
    tx: &mpsc::Sender<Result<SessionResponse, Status>>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let telem_subject = match roz_nats::subjects::Subjects::telemetry_wildcard(worker_name) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(worker_name = %worker_name, error = %e, "invalid worker name for telemetry subject");
            return;
        }
    };

    let telem_sub = match nats.subscribe(telem_subject.clone()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(subject = %telem_subject, error = %e, "failed to subscribe to telemetry");
            return;
        }
    };

    tracing::info!(subject = %telem_subject, worker_name = %worker_name, "telemetry relay started");

    let telem_tx = tx.clone();
    let host_id_owned = host_id.to_string();
    tokio::spawn(async move {
        let mut sub = telem_sub;
        loop {
            let msg = tokio::select! {
                () = cancel.cancelled() => break,
                msg = sub.next() => match msg {
                    Some(m) => m,
                    None => break,
                },
            };
            if let Ok(data) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                // Parse joint states from the worker telemetry JSON.
                let joint_states: Vec<roz_v1::JointState> = data["joints"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|j| {
                                Some(roz_v1::JointState {
                                    name: j["name"].as_str()?.to_string(),
                                    position: j["position"].as_f64().unwrap_or(0.0),
                                    velocity: j["velocity"].as_f64().unwrap_or(0.0),
                                    effort: j["effort"].as_f64().unwrap_or(0.0),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Parse sensor readings from the worker telemetry JSON.
                let sensor_readings: std::collections::BTreeMap<String, f64> = data["sensors"]
                    .as_object()
                    .map(|obj| obj.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect())
                    .unwrap_or_default();

                let update = roz_v1::TelemetryUpdate {
                    host_id: host_id_owned.clone(),
                    timestamp: data["timestamp"].as_f64().unwrap_or(0.0),
                    joint_states,
                    end_effector_pose: None,
                    sensor_readings,
                };
                let resp = SessionResponse {
                    response: Some(session_response::Response::Telemetry(update)),
                };
                if telem_tx.send(Ok(resp)).await.is_err() {
                    break; // client disconnected
                }
            }
        }
    });
}

/// Resolve `host_id` (UUID string) to the host's `name` (= `worker_id`) via Postgres.
///
/// Returns `None` if the UUID is invalid or the host is not found.
async fn resolve_worker_id(pool: &PgPool, host_id_str: &str) -> Option<String> {
    let host_uuid = uuid::Uuid::parse_str(host_id_str).ok()?;
    let host = roz_db::hosts::get_by_id(pool, host_uuid).await.ok()??;
    Some(host.name)
}

/// Relay a `WebRtcAnswer` from the gRPC client to the worker via NATS.
///
/// Publishes a JSON payload matching the worker's `SignalingRelay` format
/// on `webrtc.{worker_id}.{peer_id}.answer`.
async fn relay_webrtc_answer(nats: &async_nats::Client, worker_id: &str, answer: &roz_v1::WebRtcAnswer) {
    let subject = match Subjects::webrtc_answer(worker_id, &answer.peer_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "invalid worker/peer id for WebRTC answer subject");
            return;
        }
    };

    let payload = serde_json::json!({
        "sdp": answer.sdp,
        "ice_candidates": answer.ice_candidates,
    });
    let bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize WebRTC answer");
            return;
        }
    };

    if let Err(e) = nats.publish(subject.clone(), bytes.into()).await {
        tracing::warn!(subject = %subject, error = %e, "failed to publish WebRTC answer to NATS");
    } else {
        tracing::debug!(subject = %subject, "relayed WebRTC answer to worker");
    }
}

/// Relay an `IceCandidate` from the gRPC client to the worker via NATS.
///
/// Publishes a JSON payload on `webrtc.{worker_id}.{peer_id}.ice.remote`.
async fn relay_ice_candidate(nats: &async_nats::Client, worker_id: &str, candidate: &roz_v1::IceCandidate) {
    let subject = match Subjects::webrtc_ice_remote(worker_id, &candidate.peer_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "invalid worker/peer id for ICE candidate subject");
            return;
        }
    };

    let payload = serde_json::json!({
        "candidate": candidate.candidate,
        "sdp_mid": candidate.sdp_mid,
        "sdp_m_line_index": candidate.sdp_m_line_index,
    });
    let bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize ICE candidate");
            return;
        }
    };

    if let Err(e) = nats.publish(subject.clone(), bytes.into()).await {
        tracing::warn!(subject = %subject, error = %e, "failed to publish ICE candidate to NATS");
    } else {
        tracing::debug!(subject = %subject, "relayed ICE candidate to worker");
    }
}

/// Relay a `CameraRequest` from the gRPC client to the worker via NATS.
///
/// Publishes a JSON payload on `camera.{worker_id}.request`.
async fn relay_camera_request(nats: &async_nats::Client, worker_id: &str, cam_req: &roz_v1::CameraRequest) {
    let subject = match Subjects::camera_request(worker_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "invalid worker id for camera request subject");
            return;
        }
    };

    let payload = serde_json::json!({
        "host_id": cam_req.host_id,
        "camera_ids": cam_req.camera_ids,
    });
    let bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize camera request");
            return;
        }
    };

    if let Err(e) = nats.publish(subject.clone(), bytes.into()).await {
        tracing::warn!(subject = %subject, error = %e, "failed to publish camera request to NATS");
    } else {
        tracing::debug!(subject = %subject, "relayed camera request to worker");
    }
}

/// Subscribe to NATS WebRTC offer and ICE candidate subjects for a worker and
/// relay them as `SessionResponse` messages on the gRPC stream.
///
/// Subscribes to:
/// - `webrtc.{worker_name}.*.offer` -> `SessionResponse::WebrtcOffer`
/// - `webrtc.{worker_name}.*.ice.local` -> `SessionResponse::IceCandidate`
///
/// The loop exits when `cancel` is cancelled (session ended) or the client disconnects.
async fn spawn_webrtc_signaling_relay(
    nats: &async_nats::Client,
    worker_name: &str,
    tx: &mpsc::Sender<Result<SessionResponse, Status>>,
    cancel: tokio_util::sync::CancellationToken,
) {
    // Subscribe to all WebRTC subjects for this worker.
    let wildcard_subject = match Subjects::webrtc_wildcard(worker_name) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(worker_name = %worker_name, error = %e, "invalid worker name for WebRTC wildcard subject");
            return;
        }
    };

    let webrtc_sub = match nats.subscribe(wildcard_subject.clone()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(subject = %wildcard_subject, error = %e, "failed to subscribe to WebRTC signaling");
            return;
        }
    };

    tracing::info!(subject = %wildcard_subject, worker_name = %worker_name, "WebRTC signaling relay started");

    let relay_tx = tx.clone();
    let worker_name_owned = worker_name.to_string();
    tokio::spawn(async move {
        let mut sub = webrtc_sub;
        loop {
            let msg = tokio::select! {
                () = cancel.cancelled() => break,
                msg = sub.next() => match msg {
                    Some(m) => m,
                    None => break,
                },
            };
            let subject_str = msg.subject.as_str();
            // Parse NATS subject segments: webrtc.{worker_id}.{peer_id}.{type}[.{subtype}]
            let segments: Vec<&str> = subject_str.split('.').collect();
            let peer_id = segments.get(2).unwrap_or(&"").to_string();
            let sig_type = segments.get(3).copied().unwrap_or("");

            let resp = match sig_type {
                "offer" => {
                    // Worker -> client: WebRTC offer (JSON format from SignalingRelay).
                    match serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                        Ok(data) => {
                            let sdp = data["sdp"].as_str().unwrap_or_default().to_string();
                            let camera_ids: Vec<String> = data["camera_ids"]
                                .as_array()
                                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                .unwrap_or_default();
                            Some(session_response::Response::WebrtcOffer(roz_v1::WebRtcOffer {
                                host_id: worker_name_owned.clone(),
                                sdp,
                                ice_candidates: vec![],
                                peer_id: peer_id.clone(),
                                camera_ids,
                            }))
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, subject = %subject_str, "failed to decode WebRTC offer");
                            None
                        }
                    }
                }
                // webrtc.{worker_id}.{peer_id}.ice.local -> relay as IceCandidate
                "ice" if segments.get(4).copied() == Some("local") => {
                    match serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                        Ok(data) => {
                            let candidate = data["candidate"].as_str().unwrap_or_default().to_string();
                            let sdp_mid = data["sdp_mid"].as_str().unwrap_or_default().to_string();
                            #[expect(
                                clippy::cast_possible_truncation,
                                reason = "sdp_m_line_index values are small integers"
                            )]
                            let sdp_m_line_index = data["sdp_m_line_index"].as_u64().unwrap_or(0) as u32;
                            Some(session_response::Response::IceCandidate(roz_v1::IceCandidate {
                                host_id: worker_name_owned.clone(),
                                peer_id: peer_id.clone(),
                                candidate,
                                sdp_mid,
                                sdp_m_line_index,
                            }))
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, subject = %subject_str, "failed to decode ICE candidate");
                            None
                        }
                    }
                }
                // answer / ice.remote are client->worker direction, not relayed back.
                _ => None,
            };

            if let Some(response) = resp {
                let session_resp = SessionResponse {
                    response: Some(response),
                };
                if relay_tx.send(Ok(session_resp)).await.is_err() {
                    break; // client disconnected
                }
            }
        }
    });
}

/// Handle the `StartSession` message. Returns `true` to continue the loop,
/// `false` to break (fatal error or auth failure).
#[expect(
    clippy::too_many_lines,
    reason = "sequential session initialization with auth + DB + placement"
)]
async fn handle_start(
    tx: &mpsc::Sender<Result<SessionResponse, Status>>,
    pool: &PgPool,
    default_model: &str,
    auth_identity: &AuthIdentity,
    start: roz_v1::StartSession,
    session: &mut Option<Session>,
) -> bool {
    if session.is_some() {
        send_error(tx, "already_started", "session already started", false).await;
        return true; // non-fatal, just ignore
    }

    // Auth was performed by grpc_auth_middleware before this handler runs.
    let tenant_id = auth_identity.tenant_id().0;

    // Auto-resolve environment: if empty, use tenant's first environment or create "default".
    let env_id = if start.environment_id.is_empty() {
        match roz_db::environments::list(pool, tenant_id, 1, 0).await {
            Ok(envs) if !envs.is_empty() => envs[0].id,
            _ => match roz_db::environments::create(pool, tenant_id, "default", "development", &serde_json::json!({}))
                .await
            {
                Ok(env) => env.id,
                Err(e) => {
                    tracing::error!(error = %e, "failed to create default environment");
                    send_error(tx, "internal", "failed to create default environment", true).await;
                    return false;
                }
            },
        }
    } else if let Ok(id) = uuid::Uuid::parse_str(&start.environment_id) {
        id
    } else {
        send_error(tx, "invalid_argument", "invalid environment_id", false).await;
        return false;
    };

    // Resolve model name (client may omit or leave empty).
    let max_context_tokens = start.max_context_tokens.unwrap_or(200_000);
    let raw_model = start
        .model
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| default_model.to_string());

    // Map tier names (fast/standard/max) to actual model names.
    // The IDE frontend sends tier names; resolve them here.
    let model_name = match raw_model.as_str() {
        "fast" => "claude-haiku-4-5-20251001".to_string(),
        "standard" => "claude-sonnet-4-6".to_string(),
        "max" => "claude-opus-4-6".to_string(),
        _ => raw_model,
    };

    // Write session metadata to Postgres.
    let session_row = match roz_db::agent_sessions::create_session(pool, tenant_id, env_id, &model_name).await {
        Ok(row) => row,
        Err(e) => {
            tracing::error!(error = %e, "failed to create agent session");
            send_error(tx, "internal", "failed to create session", true).await;
            return false;
        }
    };

    let session_id = session_row.id;

    // Extract per-tool categories before consuming the proto types.
    let tool_categories: HashMap<String, ToolCategory> = start
        .tools
        .iter()
        .map(|t| {
            let cat = match t.category() {
                roz_v1::ToolCategoryHint::ToolCategoryPure => ToolCategory::Pure,
                _ => ToolCategory::Physical,
            };
            (t.name.clone(), cat)
        })
        .collect();

    // Convert client tools to domain types.
    let tools: Vec<roz_core::tools::ToolSchema> =
        start.tools.into_iter().map(roz_core::tools::ToolSchema::from).collect();

    // Convert history to domain messages.
    let messages: Vec<roz_agent::model::types::Message> = start
        .history
        .into_iter()
        .filter_map(|m| roz_agent::model::types::Message::try_from(m).ok())
        .collect();

    // Derive permission rules from tool schemas.
    let base_permissions = derive_permissions(&tools);

    let is_edge = resolve_placement(start.agent_placement.unwrap_or(0), start.host_id.is_some());

    // Resolve host_id to worker_name once at session start to avoid per-message DB lookups.
    let worker_name = if let Some(ref hid) = start.host_id {
        resolve_worker_id(pool, hid).await
    } else {
        None
    };

    let session_config = roz_agent::session_runtime::SessionConfig {
        session_id: session_id.to_string(),
        tenant_id: tenant_id.to_string(),
        mode: if is_edge {
            roz_core::session::control::SessionMode::Edge
        } else {
            roz_core::session::control::SessionMode::Server
        },
        cognition_mode: roz_core::session::control::CognitionMode::React,
        constitution_text: build_constitution_for_tools(AgentLoopMode::React, &tools),
        blueprint_toml: String::new(),
        model_name: Some(model_name.clone()),
        permissions: base_permissions.clone(),
        tool_schemas: prompt_tool_schemas(&tools),
        project_context: start.project_context.clone(),
        initial_history: messages,
    };
    let (runtime, edge_mirror, event_rx) = if is_edge {
        (
            None,
            Some(Arc::new(AsyncMutex::new(EdgeSessionMirror::new(
                roz_agent::session_runtime::SessionRuntimeBootstrap::from_config(&session_config),
            )))),
            None,
        )
    } else {
        let mut session_rt = roz_agent::session_runtime::SessionRuntime::new(&session_config);
        let mut event_rx = session_rt.subscribe_events();
        if let Err(error) = session_rt.start_session().await {
            tracing::error!(error = %error, "failed to start cloud session runtime");
            send_error(tx, "internal", "failed to start session runtime", true).await;
            return false;
        }
        if !drain_cloud_runtime_events(tx, &mut event_rx, &model_name, &base_permissions).await {
            return false;
        }
        (Some(Arc::new(AsyncMutex::new(session_rt))), None, Some(event_rx))
    };

    *session = Some(Session {
        id: session_id,
        tenant_id,
        environment_id: env_id,
        model_name,
        max_context_tokens,
        tools,
        tool_categories,
        project_context: start.project_context,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        base_permissions: base_permissions.clone(),
        active_permissions: base_permissions,
        host_id: start.host_id,
        worker_name,
        is_edge,
        runtime,
        edge_mirror,
        event_rx,
    });

    true
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Assemble system prompt as separate blocks for prompt prefix caching.
///
/// Returns `Vec<String>` where each element becomes an independent `SystemBlock`
/// in the Anthropic API, enabling cache reuse of stable prefix blocks across turns.
///
/// Block layout:
/// 0. Constitution (mode-aware) — platform-level (stable per-session)
/// 1. Project context — from `StartSession` (AGENTS.md, rules) (stable per-session)
/// 2. Pending context — from the most recent `RegisterTools.system_context` (semi-stable)
/// 3. Per-message context — from `UserMessage.context` (volatile per-turn)
#[cfg_attr(not(test), allow(dead_code))]
fn build_system_prompt_blocks(
    mode: AgentLoopMode,
    project_context: &[String],
    per_message_context: &[roz_v1::ContentBlock],
    pending_system_context: Option<String>,
) -> Vec<String> {
    let mut blocks = vec![build_constitution(mode, &[])];

    // Consolidate all project context into one block (session-stable).
    let mut project_parts = Vec::new();
    for ctx in project_context {
        let trimmed = ctx.trim();
        if !trimmed.is_empty() {
            project_parts.push(trimmed.to_string());
        }
    }
    if !project_parts.is_empty() {
        blocks.push(project_parts.join("\n\n"));
    }

    // Workflow context from the most recent RegisterTools (semi-stable).
    if let Some(ctx) = pending_system_context {
        let trimmed = ctx.trim().to_string();
        if !trimmed.is_empty() {
            blocks.push(trimmed);
        }
    }

    // Per-message context as one volatile block (changes each turn).
    let mut volatile_parts = Vec::new();
    for block in per_message_context {
        if let Some(roz_v1::content_block::Block::Text(t)) = &block.block {
            let label = block.label.as_deref().unwrap_or("Context");
            volatile_parts.push(format!("[{label}]\n{t}"));
        }
    }
    if !volatile_parts.is_empty() {
        blocks.push(volatile_parts.join("\n\n"));
    }

    blocks
}

fn prompt_tool_schemas(tools: &[roz_core::tools::ToolSchema]) -> Vec<roz_agent::prompt_assembler::ToolSchema> {
    tools
        .iter()
        .map(|tool| roz_agent::prompt_assembler::ToolSchema {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters_json: serde_json::to_string(&tool.parameters).unwrap_or_else(|_| "{}".to_string()),
        })
        .collect()
}

fn proto_tool_category(tool: &roz_v1::ToolSchema) -> ToolCategory {
    match tool.category() {
        roz_v1::ToolCategoryHint::ToolCategoryPure => ToolCategory::Pure,
        _ => ToolCategory::Physical,
    }
}

fn build_constitution_for_tools(mode: AgentLoopMode, tools: &[roz_core::tools::ToolSchema]) -> String {
    let tool_names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    build_constitution(mode, &tool_names)
}

fn resolved_user_message_mode(ai_mode: Option<&str>, current_mode: AgentLoopMode) -> AgentLoopMode {
    match ai_mode {
        Some("react") => AgentLoopMode::React,
        Some("ooda_react" | "ooda_re_act" | "ooda") => AgentLoopMode::OodaReAct,
        _ => current_mode,
    }
}

fn sync_cloud_runtime_surface(
    runtime: &mut SessionRuntime,
    mode: AgentLoopMode,
    tools: &[roz_core::tools::ToolSchema],
    project_context: &[String],
    permissions: &[SessionPermissionRule],
) {
    runtime.sync_cognition_mode(mode);
    runtime.sync_prompt_surface(
        build_constitution_for_tools(mode, tools),
        prompt_tool_schemas(tools),
        project_context.to_vec(),
    );
    runtime.sync_permissions(permissions.to_vec());
}

fn prompt_context_blocks(blocks: &[roz_v1::ContentBlock]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|block| {
            let roz_v1::content_block::Block::Text(text) = block.block.as_ref()? else {
                return None;
            };
            let label = block.label.as_deref().unwrap_or("Context");
            Some(format!("[{label}]\n{text}"))
        })
        .collect()
}

fn edge_tool_category_name(tool: &roz_v1::ToolSchema) -> &'static str {
    match tool.category() {
        roz_v1::ToolCategoryHint::ToolCategoryPure => "pure",
        _ => "physical",
    }
}

fn build_edge_relay_user_message_envelope(message: &roz_v1::UserMessage) -> serde_json::Value {
    serde_json::json!({
        "type": "user_message",
        "text": message.content,
        "message_id": message.message_id,
        "ai_mode": message.ai_mode,
        "system_context": message.system_context,
        "volatile_blocks": prompt_context_blocks(&message.context),
        "tools": message.tools.iter().map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": super::tasks::prost_struct_to_json(tool.parameters_schema.clone().unwrap_or_default()),
                "category": edge_tool_category_name(tool),
            })
        }).collect::<Vec<_>>(),
    })
}

fn build_edge_relay_register_tools_envelope(register_tools: &roz_v1::RegisterTools) -> serde_json::Value {
    serde_json::json!({
        "type": "register_tools",
        "source": register_tools.source,
        "system_context": register_tools.system_context,
        "tools": register_tools.tools.iter().map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": super::tasks::prost_struct_to_json(tool.parameters_schema.clone().unwrap_or_default()),
                "category": edge_tool_category_name(tool),
            })
        }).collect::<Vec<_>>(),
    })
}

fn build_edge_relay_start_envelope(
    bootstrap: &roz_agent::session_runtime::SessionRuntimeBootstrap,
) -> serde_json::Value {
    serde_json::json!({
        "type": "start_session",
        "model": bootstrap.model_name.clone(),
        "bootstrap": bootstrap,
    })
}

fn build_edge_relay_tool_result_envelope(result: &roz_v1::ToolResult) -> serde_json::Value {
    serde_json::json!({
        "type": "tool_result",
        "tool_call_id": result.tool_call_id,
        "success": result.success,
        "result": result.result,
        "exit_code": result.exit_code,
        "truncated": result.truncated,
        "duration_ms": result.duration_ms,
    })
}

fn build_edge_relay_permission_decision_envelope(decision: &roz_v1::PermissionDecision) -> serde_json::Value {
    serde_json::json!({
        "type": "permission_decision",
        "approval_id": decision.approval_id,
        "approved": decision.approved,
        "modifier": decision.modifier.clone().map(super::tasks::prost_struct_to_json),
    })
}

fn agent_error_to_turn_execution_failure(error: roz_agent::error::AgentError) -> TurnExecutionFailure {
    match error {
        roz_agent::error::AgentError::Safety(message) => {
            TurnExecutionFailure::new(roz_core::session::activity::RuntimeFailureKind::SafetyBlocked, message)
                .with_client_error("safety_violation", false)
        }
        roz_agent::error::AgentError::ToolDispatch { message, .. } => {
            TurnExecutionFailure::new(roz_core::session::activity::RuntimeFailureKind::ToolError, message)
                .with_client_error("agent_error", false)
        }
        roz_agent::error::AgentError::CircuitBreakerTripped {
            consecutive_error_turns,
        } => TurnExecutionFailure::new(
            roz_core::session::activity::RuntimeFailureKind::CircuitBreakerTripped,
            format!("circuit breaker tripped after {consecutive_error_turns} consecutive all-error turns"),
        )
        .with_client_error("agent_error", false),
        roz_agent::error::AgentError::Cancelled { .. } => TurnExecutionFailure::new(
            roz_core::session::activity::RuntimeFailureKind::OperatorAbort,
            "turn cancelled",
        ),
        roz_agent::error::AgentError::BudgetExceeded { plan, period_end } => TurnExecutionFailure::new(
            roz_core::session::activity::RuntimeFailureKind::ModelError,
            format!("usage limit reached on plan '{plan}', resets {period_end}"),
        )
        .with_client_error("budget_exceeded", false),
        other => {
            let retryable = other.is_retryable();
            let message = if retryable {
                "Rate limited by provider. Please try again shortly.".to_string()
            } else {
                "Model request failed. Please try again.".to_string()
            };
            TurnExecutionFailure::new(roz_core::session::activity::RuntimeFailureKind::ModelError, message)
                .with_client_error(if retryable { "rate_limited" } else { "agent_error" }, retryable)
        }
    }
}

fn canonicalize_session_started_envelope(
    envelope: &EventEnvelope,
    model_name: &str,
    permissions: &[SessionPermissionRule],
) -> EventEnvelope {
    let SessionEvent::SessionStarted {
        session_id,
        mode,
        blueprint_version,
        ..
    } = &envelope.event
    else {
        return envelope.clone();
    };

    EventEnvelope {
        event_id: envelope.event_id.clone(),
        correlation_id: envelope.correlation_id.clone(),
        parent_event_id: envelope.parent_event_id.clone(),
        timestamp: envelope.timestamp,
        event: SessionEvent::SessionStarted {
            session_id: session_id.clone(),
            mode: *mode,
            blueprint_version: blueprint_version.clone(),
            model_name: Some(model_name.to_string()),
            permissions: permissions.to_vec(),
        },
    }
}

#[allow(clippy::unnecessary_wraps)]
fn edge_event_envelope_to_response(envelope: &EventEnvelope, model_name: &str) -> Option<session_response::Response> {
    let _ = model_name;
    Some(super::event_mapper::canonical_event_envelope_to_session_response(
        envelope,
    ))
}

fn edge_canonical_json_envelope_to_response(
    envelope: &CanonicalSessionEventEnvelope,
    model_name: &str,
) -> Option<session_response::Response> {
    match envelope.clone().into_event_envelope() {
        Ok(envelope) => edge_event_envelope_to_response(&envelope, model_name),
        Err(error) => {
            tracing::warn!(%error, event_type = %envelope.event_type, "failed to decode canonical edge session event payload");
            Some(super::event_mapper::canonical_session_event_to_response(
                SessionEvent::EdgeTransportDegraded {
                    transport: "nats".to_string(),
                    health: EdgeTransportHealth::Degraded {
                        affected: vec!["session_event_decode".to_string()],
                    },
                    affected_capabilities: vec!["session_events".to_string()],
                },
                CorrelationId(envelope.correlation_id.clone()),
            ))
        }
    }
}

fn cloud_event_envelope_to_response(
    envelope: &EventEnvelope,
    model_name: &str,
    permissions: &[SessionPermissionRule],
) -> session_response::Response {
    let envelope = canonicalize_session_started_envelope(envelope, model_name, permissions);
    super::event_mapper::canonical_event_envelope_to_session_response(&envelope)
}

async fn drain_cloud_runtime_events(
    tx: &mpsc::Sender<Result<SessionResponse, Status>>,
    event_rx: &mut broadcast::Receiver<EventEnvelope>,
    model_name: &str,
    permissions: &[SessionPermissionRule],
) -> bool {
    loop {
        match event_rx.try_recv() {
            Ok(envelope) => {
                let response = cloud_event_envelope_to_response(&envelope, model_name, permissions);
                if tx
                    .send(Ok(SessionResponse {
                        response: Some(response),
                    }))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => return true,
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "cloud session event stream lagged");
            }
        }
    }
}

/// Send a `SessionError` response on the outbound channel.
async fn send_error(tx: &mpsc::Sender<Result<SessionResponse, Status>>, code: &str, message: &str, retryable: bool) {
    let _ = tx
        .send(Ok(SessionResponse {
            response: Some(super::event_mapper::canonical_session_event_to_response(
                SessionEvent::SessionRejected {
                    code: code.into(),
                    message: message.into(),
                    retryable,
                },
                CorrelationId::new(),
            )),
        }))
        .await;
}

// ---------------------------------------------------------------------------
// TeamEvent conversion
// ---------------------------------------------------------------------------

/// Convert a `roz_core::team::TeamEvent` into the protobuf `TeamEvent` message.
///
/// This is a pure function (no IO) so it can be unit-tested without a NATS connection.
fn core_team_event_to_proto(event: CoreTeamEvent) -> roz_v1::TeamEvent {
    use roz_v1::team_event::Event;

    let inner = match event {
        CoreTeamEvent::WorkerStarted { worker_id, host_id } => Event::WorkerStarted(roz_v1::WorkerStarted {
            worker_id: worker_id.to_string(),
            host_id,
        }),
        CoreTeamEvent::WorkerPhase { worker_id, phase, mode } => Event::WorkerPhase(roz_v1::WorkerPhase {
            worker_id: worker_id.to_string(),
            phase,
            mode: match mode {
                roz_core::phases::PhaseMode::React => "react".to_string(),
                roz_core::phases::PhaseMode::OodaReAct => "ooda_react".to_string(),
            },
        }),
        CoreTeamEvent::WorkerToolCall { worker_id, tool } => Event::WorkerToolCall(roz_v1::WorkerToolCall {
            worker_id: worker_id.to_string(),
            tool,
        }),
        CoreTeamEvent::WorkerApprovalRequested {
            worker_id,
            task_id,
            approval_id,
            tool_name,
            reason,
            timeout_secs,
        } => Event::WorkerApprovalRequested(roz_v1::WorkerApprovalRequested {
            worker_id: worker_id.to_string(),
            task_id: task_id.to_string(),
            approval_id,
            tool_name,
            reason,
            timeout_secs,
        }),
        CoreTeamEvent::WorkerApprovalResolved {
            worker_id,
            task_id,
            approval_id,
            approved,
            modifier,
        } => Event::WorkerApprovalResolved(roz_v1::WorkerApprovalResolved {
            worker_id: worker_id.to_string(),
            task_id: task_id.to_string(),
            approval_id,
            approved,
            modifier: modifier.map(value_to_struct),
        }),
        CoreTeamEvent::WorkerCompleted { worker_id, result } => Event::WorkerCompleted(roz_v1::WorkerCompleted {
            worker_id: worker_id.to_string(),
            result,
        }),
        CoreTeamEvent::WorkerFailed { worker_id, reason } => Event::WorkerFailed(roz_v1::WorkerFailed {
            worker_id: worker_id.to_string(),
            reason: match reason {
                WorkerFailReason::EStop => "e_stop".to_string(),
                WorkerFailReason::Timeout => "timeout".to_string(),
                WorkerFailReason::ModelError => "model_error".to_string(),
                WorkerFailReason::SafetyViolation => "safety_violation".to_string(),
            },
        }),
        CoreTeamEvent::WorkerExited {
            worker_id,
            parent_task_id,
        } => Event::WorkerExited(roz_v1::WorkerExited {
            worker_id: worker_id.to_string(),
            parent_task_id: parent_task_id.to_string(),
        }),
    };

    roz_v1::TeamEvent { event: Some(inner) }
}

fn decode_team_event_payload(payload: &[u8]) -> Option<CoreTeamEvent> {
    if let Ok(sequenced) = serde_json::from_slice::<SequencedTeamEvent>(payload) {
        return Some(sequenced.event);
    }

    serde_json::from_slice::<CoreTeamEvent>(payload).ok()
}

/// Substrings that indicate a tool is "pure" (read-only / observational).
const PURE_VERBS: &[&str] = &["read", "get", "list", "search", "glob", "grep", "find"];

/// Derive default permission rules from tool schemas.
///
/// Physical tools default to `require_confirmation`; tools whose names contain
/// read-like verbs (get, list, search, etc.) default to `allow`.
fn derive_permissions(tools: &[roz_core::tools::ToolSchema]) -> Vec<SessionPermissionRule> {
    if tools.is_empty() {
        // Sensible defaults when no tools are declared.
        return vec![
            SessionPermissionRule {
                tool_pattern: "*".into(),
                policy: "require_confirmation".into(),
                category: Some("physical".into()),
                reason: Some("default: physical tools require confirmation".into()),
            },
            SessionPermissionRule {
                tool_pattern: "*".into(),
                policy: "allow".into(),
                category: Some("pure".into()),
                reason: Some("default: pure tools auto-allowed".into()),
            },
        ];
    }

    tools
        .iter()
        .map(|tool| {
            let is_pure = PURE_VERBS.iter().any(|verb| tool.name.starts_with(verb));

            SessionPermissionRule {
                tool_pattern: tool.name.clone(),
                policy: if is_pure {
                    "allow".into()
                } else {
                    "require_confirmation".into()
                },
                category: Some(if is_pure { "pure".into() } else { "physical".into() }),
                reason: None,
            }
        })
        .collect()
}

/// Override permissions for plan mode: physical tools are blocked, pure tools
/// remain allowed.
#[allow(dead_code)]
fn plan_mode_permissions(base: &[SessionPermissionRule]) -> Vec<SessionPermissionRule> {
    base.iter()
        .map(|rule| {
            if rule.category.as_deref() == Some("physical") {
                SessionPermissionRule {
                    policy: "block".into(),
                    reason: Some("plan mode: observation only".into()),
                    ..rule.clone()
                }
            } else {
                rule.clone()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Edge agent relay
// ---------------------------------------------------------------------------

/// Re-export from roz-core — shared with worker session relay.
use roz_core::edge::resolve_placement;

/// Relays a gRPC session to an edge worker via NATS.
///
/// Subscribes to the worker's response subject and forwards responses back
/// on the gRPC stream. Forwards all subsequent gRPC requests to NATS.
/// Returns when the client disconnects or the worker session ends.
#[expect(clippy::too_many_lines, reason = "bidirectional relay with message type mapping")]
async fn run_edge_relay(
    tx: &mpsc::Sender<Result<SessionResponse, Status>>,
    nats: &async_nats::Client,
    worker_id: &str,
    session_id: &str,
    edge_mirror: Arc<AsyncMutex<EdgeSessionMirror>>,
    stream: &mut Streaming<SessionRequest>,
) {
    let req_subject = match roz_nats::subjects::Subjects::session_request(worker_id, session_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to build session request subject");
            return;
        }
    };
    let resp_subject = match roz_nats::subjects::Subjects::session_response(worker_id, session_id) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to build session response subject");
            return;
        }
    };

    // Subscribe to worker responses.
    let mut worker_resp = match nats.subscribe(resp_subject).await {
        Ok(sub) => sub,
        Err(e) => {
            tracing::error!(error = %e, "failed to subscribe to worker responses");
            return;
        }
    };

    // Publish StartSession to the worker, including the resolved model name
    // so the worker can use the client's model selection.
    let start_envelope = {
        let bootstrap = edge_mirror.lock().await.export_bootstrap();
        build_edge_relay_start_envelope(&bootstrap)
    };
    if let Ok(payload) = serde_json::to_vec(&start_envelope)
        && let Err(e) = nats.publish(req_subject.clone(), payload.into()).await
    {
        tracing::error!(error = %e, "failed to publish start_session to worker");
        return;
    }

    // Spawn response relay: worker NATS -> gRPC client.
    // Timeout after 30s of silence from worker — handles worker crash.
    let tx_clone = tx.clone();
    let relay_session_id = session_id.to_string();
    let relay_model_name = { edge_mirror.lock().await.model_name() };
    let relay_edge_mirror = Arc::clone(&edge_mirror);
    let resp_relay = tokio::spawn(async move {
        loop {
            let msg = match tokio::time::timeout(Duration::from_secs(30), worker_resp.next()).await {
                Ok(Some(msg)) => msg,
                Ok(None) => break, // subscription closed
                Err(_) => {
                    // 30s with no message from worker — assume worker crash.
                    tracing::error!(session_id = %relay_session_id, "edge relay timeout — no response from worker in 30s");
                    let err_resp = SessionResponse {
                        response: Some(super::event_mapper::canonical_session_event_to_response(
                            SessionEvent::SessionRejected {
                                code: "worker_timeout".to_string(),
                                message: "No response from edge worker within 30 seconds — worker may have crashed"
                                    .to_string(),
                                retryable: true,
                            },
                            CorrelationId::new(),
                        )),
                    };
                    let _ = tx_clone.send(Ok(err_resp)).await;
                    break;
                }
            };
            if let Ok(envelope) = serde_json::from_slice::<CanonicalSessionEventEnvelope>(&msg.payload) {
                if let Some(response) = edge_canonical_json_envelope_to_response(&envelope, &relay_model_name) {
                    let session_response = SessionResponse {
                        response: Some(response),
                    };
                    if tx_clone.send(Ok(session_response)).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            if let Ok(envelope) = serde_json::from_slice::<EventEnvelope>(&msg.payload) {
                if let Some(response) = edge_event_envelope_to_response(&envelope, &relay_model_name) {
                    let session_response = SessionResponse {
                        response: Some(response),
                    };
                    if tx_clone.send(Ok(session_response)).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(&msg.payload) else {
                continue;
            };

            let msg_type = envelope["type"].as_str().unwrap_or("");
            if msg_type == "runtime_checkpoint" {
                match serde_json::from_value::<roz_agent::session_runtime::SessionRuntimeBootstrap>(
                    envelope.get("bootstrap").cloned().unwrap_or(serde_json::Value::Null),
                ) {
                    Ok(bootstrap) => {
                        relay_edge_mirror.lock().await.update_checkpoint(bootstrap);
                    }
                    Err(error) => {
                        tracing::warn!(session_id = %relay_session_id, %error, "failed to parse edge runtime checkpoint");
                    }
                }
                continue;
            }
            if msg_type == "keepalive" {
                // Worker is alive but busy. Reset timeout by continuing the loop.
                continue;
            }

            tracing::warn!(msg_type, session_id = %relay_session_id, "ignoring unexpected legacy edge relay message");
        }
    });

    // Forward gRPC requests -> NATS (runs until client disconnects).
    while let Ok(Some(req)) = stream.message().await {
        if let Some(request) = req.request {
            let envelope = match request {
                session_request::Request::UserMessage(um) => build_edge_relay_user_message_envelope(&um),
                session_request::Request::RegisterTools(reg) => build_edge_relay_register_tools_envelope(&reg),
                session_request::Request::ToolResult(result) => build_edge_relay_tool_result_envelope(&result),
                session_request::Request::PermissionDecision(decision) => {
                    build_edge_relay_permission_decision_envelope(&decision)
                }
                session_request::Request::CancelSession(_) => {
                    serde_json::json!({"type": "cancel_session"})
                }
                session_request::Request::CancelTurn(_) => {
                    serde_json::json!({"type": "cancel_turn"})
                }
                _ => continue,
            };

            if let Ok(payload) = serde_json::to_vec(&envelope) {
                let _ = nats.publish(req_subject.clone(), payload.into()).await;
            }
        }
    }

    resp_relay.abort();
    tracing::info!(session_id, worker_id, "edge relay ended");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::edge_health::EdgeTransportHealth;
    use roz_core::session::activity::RuntimeActivity;
    use roz_core::session::control::SessionMode;
    use serde_json::json;

    #[test]
    fn derive_permissions_no_tools_returns_defaults() {
        let perms = derive_permissions(&[]);
        assert_eq!(perms.len(), 2);
        assert_eq!(perms[0].tool_pattern, "*");
        assert_eq!(perms[0].policy, "require_confirmation");
        assert_eq!(perms[0].category.as_deref(), Some("physical"));
        assert_eq!(perms[1].tool_pattern, "*");
        assert_eq!(perms[1].policy, "allow");
        assert_eq!(perms[1].category.as_deref(), Some("pure"));
    }

    #[test]
    fn derive_permissions_classifies_tools() {
        let tools = vec![
            roz_core::tools::ToolSchema {
                name: "move_arm".into(),
                description: "Move the robot arm".into(),
                parameters: json!({}),
            },
            roz_core::tools::ToolSchema {
                name: "get_sensor_data".into(),
                description: "Read sensor data".into(),
                parameters: json!({}),
            },
            roz_core::tools::ToolSchema {
                name: "list_files".into(),
                description: "List files in a directory".into(),
                parameters: json!({}),
            },
            roz_core::tools::ToolSchema {
                name: "gripper_close".into(),
                description: "Close the gripper".into(),
                parameters: json!({}),
            },
        ];

        let perms = derive_permissions(&tools);
        assert_eq!(perms.len(), 4);

        // move_arm -> physical -> require_confirmation
        assert_eq!(perms[0].tool_pattern, "move_arm");
        assert_eq!(perms[0].policy, "require_confirmation");
        assert_eq!(perms[0].category.as_deref(), Some("physical"));

        // get_sensor_data -> pure (contains "get") -> allow
        assert_eq!(perms[1].tool_pattern, "get_sensor_data");
        assert_eq!(perms[1].policy, "allow");
        assert_eq!(perms[1].category.as_deref(), Some("pure"));

        // list_files -> pure (contains "list") -> allow
        assert_eq!(perms[2].tool_pattern, "list_files");
        assert_eq!(perms[2].policy, "allow");
        assert_eq!(perms[2].category.as_deref(), Some("pure"));

        // gripper_close -> physical -> require_confirmation
        assert_eq!(perms[3].tool_pattern, "gripper_close");
        assert_eq!(perms[3].policy, "require_confirmation");
        assert_eq!(perms[3].category.as_deref(), Some("physical"));
    }

    #[test]
    fn resolved_user_message_mode_preserves_runtime_mode_when_shell_omits_it() {
        assert_eq!(
            resolved_user_message_mode(None, AgentLoopMode::OodaReAct),
            AgentLoopMode::OodaReAct
        );
        assert_eq!(
            resolved_user_message_mode(Some("react"), AgentLoopMode::OodaReAct),
            AgentLoopMode::React
        );
        assert_eq!(
            resolved_user_message_mode(Some("ooda"), AgentLoopMode::React),
            AgentLoopMode::OodaReAct
        );
    }

    #[test]
    fn edge_event_envelope_maps_session_started() {
        let envelope = EventEnvelope {
            event_id: roz_core::session::event::EventId::new(),
            correlation_id: roz_core::session::event::CorrelationId::new(),
            parent_event_id: None,
            timestamp: chrono::Utc::now(),
            event: SessionEvent::SessionStarted {
                session_id: "sess-edge-1".into(),
                mode: SessionMode::Edge,
                blueprint_version: "1.0".into(),
                model_name: Some("claude-sonnet-4-6".into()),
                permissions: vec![SessionPermissionRule {
                    tool_pattern: "capture_frame".into(),
                    policy: "allow".into(),
                    category: Some("pure".into()),
                    reason: None,
                }],
            },
        };

        let response =
            edge_event_envelope_to_response(&envelope, "claude-sonnet-4-6").expect("session started should map");

        match response {
            session_response::Response::SessionEvent(event) => {
                assert_eq!(event.event_type, "session_started");
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::SessionStarted(payload)) => {
                        assert_eq!(payload.session_id, "sess-edge-1");
                        assert_eq!(payload.model_name.as_deref(), Some("claude-sonnet-4-6"));
                        assert_eq!(payload.permissions[0].tool_pattern, "capture_frame");
                        assert_eq!(payload.permissions[0].policy, "allow");
                    }
                    other => panic!("expected typed session_started payload, got {other:?}"),
                }
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn edge_event_envelope_prefers_canonical_runtime_event() {
        let envelope = EventEnvelope {
            event_id: roz_core::session::event::EventId::new(),
            correlation_id: roz_core::session::event::CorrelationId::new(),
            parent_event_id: None,
            timestamp: chrono::Utc::now(),
            event: SessionEvent::ActivityChanged {
                state: RuntimeActivity::Planning,
                reason: "turn 1 started".into(),
                robot_safe: true,
                unblock_event: None,
            },
        };

        let response = edge_event_envelope_to_response(&envelope, "ignored").expect("activity event should map");

        match response {
            session_response::Response::SessionEvent(event) => {
                assert_eq!(event.event_type, "activity_changed");
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::ActivityChanged(payload)) => {
                        assert_eq!(payload.state, "planning");
                        assert_eq!(payload.reason, "turn 1 started");
                    }
                    other => panic!("expected typed activity_changed payload, got {other:?}"),
                }
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn edge_event_envelope_preserves_unmapped_runtime_event() {
        let envelope = EventEnvelope {
            event_id: roz_core::session::event::EventId("evt-1".into()),
            correlation_id: roz_core::session::event::CorrelationId("corr-1".into()),
            parent_event_id: None,
            timestamp: chrono::Utc::now(),
            event: SessionEvent::TurnStarted { turn_index: 3 },
        };

        let response = edge_event_envelope_to_response(&envelope, "ignored").expect("turn-started event should map");

        match response {
            session_response::Response::SessionEvent(event) => {
                assert_eq!(event.event_id, "evt-1");
                assert_eq!(event.correlation_id, "corr-1");
                assert_eq!(event.event_type, "turn_started");
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::TurnStarted(payload)) => {
                        assert_eq!(payload.turn_index, 3);
                    }
                    other => panic!("expected typed turn_started payload, got {other:?}"),
                }
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn malformed_edge_canonical_json_becomes_typed_degradation_event() {
        let envelope = CanonicalSessionEventEnvelope {
            event_id: "evt-bad".into(),
            correlation_id: "corr-bad".into(),
            parent_event_id: None,
            timestamp: chrono::Utc::now(),
            event_type: "text_delta".into(),
            event_payload: serde_json::json!({"not": "a tagged session event"}),
        };

        let response = edge_canonical_json_envelope_to_response(&envelope, "ignored")
            .expect("decode failure should still map to a typed response");

        match response {
            session_response::Response::SessionEvent(event) => {
                assert_eq!(event.event_type, "edge_degraded");
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::EdgeTransportDegraded(payload)) => {
                        assert_eq!(payload.transport, "nats");
                        assert_eq!(payload.affected_capabilities, vec!["session_events"]);
                        let health = payload.health.expect("health should be present");
                        let value = super::super::convert::struct_to_value(health);
                        let parsed: EdgeTransportHealth = serde_json::from_value(value).unwrap();
                        assert_eq!(
                            parsed,
                            EdgeTransportHealth::Degraded {
                                affected: vec!["session_event_decode".into()]
                            }
                        );
                    }
                    other => panic!("expected typed edge degradation event, got {other:?}"),
                }
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn cloud_event_envelope_maps_session_started_with_permissions() {
        let envelope = EventEnvelope {
            event_id: roz_core::session::event::EventId::new(),
            correlation_id: roz_core::session::event::CorrelationId::new(),
            parent_event_id: None,
            timestamp: chrono::Utc::now(),
            event: SessionEvent::SessionStarted {
                session_id: "sess-cloud-1".into(),
                mode: SessionMode::Server,
                blueprint_version: "1.0".into(),
                model_name: None,
                permissions: vec![],
            },
        };
        let permissions = vec![SessionPermissionRule {
            tool_pattern: "capture_frame".into(),
            policy: "allow".into(),
            category: Some("pure".into()),
            reason: Some("observation only".into()),
        }];

        let response = cloud_event_envelope_to_response(&envelope, "claude-sonnet-4-6", &permissions);

        match response {
            session_response::Response::SessionEvent(event) => {
                assert_eq!(event.event_type, "session_started");
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::SessionStarted(payload)) => {
                        assert_eq!(payload.session_id, "sess-cloud-1");
                        assert_eq!(payload.model_name.as_deref(), Some("claude-sonnet-4-6"));
                        assert_eq!(payload.permissions[0].tool_pattern, "capture_frame");
                        assert_eq!(payload.permissions[0].policy, "allow");
                    }
                    other => panic!("expected typed session_started payload, got {other:?}"),
                }
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn cloud_event_envelope_prefers_canonical_runtime_event() {
        let envelope = EventEnvelope {
            event_id: roz_core::session::event::EventId("evt-cloud-1".into()),
            correlation_id: roz_core::session::event::CorrelationId("corr-cloud-1".into()),
            parent_event_id: None,
            timestamp: chrono::Utc::now(),
            event: SessionEvent::ActivityChanged {
                state: RuntimeActivity::Planning,
                reason: "turn 1 started".into(),
                robot_safe: true,
                unblock_event: None,
            },
        };

        let response = cloud_event_envelope_to_response(&envelope, "ignored", &[]);
        match response {
            session_response::Response::SessionEvent(event) => {
                assert_eq!(event.event_id, "evt-cloud-1");
                assert_eq!(event.correlation_id, "corr-cloud-1");
                assert_eq!(event.event_type, "activity_changed");
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::ActivityChanged(payload)) => {
                        assert_eq!(payload.state, "planning");
                        assert_eq!(payload.reason, "turn 1 started");
                    }
                    other => panic!("expected typed activity_changed payload, got {other:?}"),
                }
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn build_edge_relay_user_message_envelope_forwards_runtime_prompt_inputs() {
        let envelope = build_edge_relay_user_message_envelope(&roz_v1::UserMessage {
            content: "move to target".into(),
            context: vec![roz_v1::ContentBlock {
                label: Some("Editor".into()),
                block: Some(roz_v1::content_block::Block::Text("fn main() {}".into())),
            }],
            ai_mode: Some("ooda_react".into()),
            message_id: Some("msg-1".into()),
            tools: vec![roz_v1::ToolSchema {
                name: "scan_area".into(),
                description: "Scan the area".into(),
                parameters_schema: Some(crate::grpc::convert::value_to_struct(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "radius_m": { "type": "number" }
                    }
                }))),
                timeout_ms: 1_500,
                category: roz_v1::ToolCategoryHint::ToolCategoryPure as i32,
            }],
            system_context: Some("workflow context".into()),
        });

        assert_eq!(envelope["type"], "user_message");
        assert_eq!(envelope["text"], "move to target");
        assert_eq!(envelope["message_id"], "msg-1");
        assert_eq!(envelope["ai_mode"], "ooda_react");
        assert_eq!(envelope["system_context"], "workflow context");
        assert_eq!(envelope["volatile_blocks"][0], "[Editor]\nfn main() {}");
        assert_eq!(envelope["tools"][0]["name"], "scan_area");
        assert_eq!(envelope["tools"][0]["category"], "pure");
        assert_eq!(
            envelope["tools"][0]["parameters"]["properties"]["radius_m"]["type"],
            "number"
        );
    }

    #[test]
    fn build_edge_relay_register_tools_envelope_forwards_pending_system_context() {
        let envelope = build_edge_relay_register_tools_envelope(&roz_v1::RegisterTools {
            source: Some("sim".into()),
            tools: vec![roz_v1::ToolSchema {
                name: "scan_area".into(),
                description: "Scan".into(),
                parameters_schema: Some(crate::grpc::convert::value_to_struct(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "radius_m": { "type": "number" }
                    }
                }))),
                timeout_ms: 30_000,
                category: roz_v1::ToolCategoryHint::ToolCategoryPure as i32,
            }],
            system_context: Some("sim workflow".into()),
        });

        assert_eq!(envelope["type"], "register_tools");
        assert_eq!(envelope["source"], "sim");
        assert_eq!(envelope["system_context"], "sim workflow");
        assert_eq!(envelope["tools"][0]["name"], "scan_area");
        assert_eq!(envelope["tools"][0]["category"], "pure");
    }

    #[test]
    fn build_edge_relay_start_envelope_carries_only_canonical_bootstrap() {
        let bootstrap = roz_agent::session_runtime::SessionRuntimeBootstrap::from_config(
            &roz_agent::session_runtime::SessionConfig {
                session_id: "sess-edge-1".into(),
                tenant_id: "tenant-edge".into(),
                mode: SessionMode::Edge,
                cognition_mode: roz_core::session::control::CognitionMode::React,
                constitution_text: String::new(),
                blueprint_toml: String::new(),
                model_name: Some("claude-sonnet-4-6".into()),
                permissions: vec![SessionPermissionRule {
                    tool_pattern: "capture_frame".into(),
                    policy: "allow".into(),
                    category: Some("pure".into()),
                    reason: None,
                }],
                tool_schemas: Vec::new(),
                project_context: vec!["# AGENTS.md\nEdge bootstrap".into()],
                initial_history: vec![roz_agent::model::types::Message::user("hello edge")],
            },
        );

        let envelope = build_edge_relay_start_envelope(&bootstrap);

        assert_eq!(envelope["type"], "start_session");
        assert_eq!(envelope["model"], "claude-sonnet-4-6");
        assert!(envelope.get("tenant_id").is_none());
        assert!(envelope.get("project_context").is_none());
        assert!(envelope.get("permissions").is_none());
        assert!(envelope.get("history").is_none());
        assert_eq!(envelope["bootstrap"]["session_id"], "sess-edge-1");
        assert_eq!(envelope["bootstrap"]["tenant_id"], "tenant-edge");
        assert_eq!(
            envelope["bootstrap"]["project_context"][0],
            "# AGENTS.md\nEdge bootstrap"
        );
        assert_eq!(envelope["bootstrap"]["permissions"][0]["tool_pattern"], "capture_frame");
        let bootstrap_history: Vec<roz_agent::model::types::Message> =
            serde_json::from_value(envelope["bootstrap"]["history"].clone())
                .expect("bootstrap history should deserialize");
        assert_eq!(bootstrap_history[0].text().as_deref(), Some("hello edge"));
    }

    #[test]
    fn build_edge_relay_tool_result_envelope_forwards_structured_fields() {
        let envelope = build_edge_relay_tool_result_envelope(&roz_v1::ToolResult {
            tool_call_id: "toolu_123".into(),
            success: true,
            result: "{\"ok\":true}".into(),
            exit_code: Some(0),
            truncated: true,
            duration_ms: Some(42),
        });

        assert_eq!(envelope["type"], "tool_result");
        assert_eq!(envelope["tool_call_id"], "toolu_123");
        assert_eq!(envelope["success"], true);
        assert_eq!(envelope["result"], "{\"ok\":true}");
        assert_eq!(envelope["exit_code"], 0);
        assert_eq!(envelope["truncated"], true);
        assert_eq!(envelope["duration_ms"], 42);
    }

    #[test]
    fn build_edge_relay_permission_decision_envelope_forwards_modifier() {
        let envelope = build_edge_relay_permission_decision_envelope(&roz_v1::PermissionDecision {
            approval_id: "apr_approve".into(),
            approved: true,
            modifier: Some(crate::grpc::convert::value_to_struct(serde_json::json!({
                "position": { "x": 1.0 }
            }))),
        });

        assert_eq!(envelope["type"], "permission_decision");
        assert_eq!(envelope["approval_id"], "apr_approve");
        assert_eq!(envelope["approved"], true);
        assert_eq!(envelope["modifier"]["position"]["x"], 1.0);
    }

    #[test]
    fn build_system_prompt_blocks_base_only() {
        let blocks = build_system_prompt_blocks(AgentLoopMode::React, &[], &[], None);
        assert_eq!(blocks.len(), 1);
        assert!(
            blocks[0].starts_with("SAFETY-CRITICAL RULES"),
            "block 0 should be the constitution"
        );
        assert!(blocks[0].contains("MODE: Pure Reasoning (ReAct)"));
    }

    #[test]
    fn build_system_prompt_blocks_mode_aware() {
        let react = build_system_prompt_blocks(AgentLoopMode::React, &[], &[], None);
        assert!(react[0].contains("MODE: Pure Reasoning (ReAct)"));
        assert!(!react[0].contains("MODE: Physical Execution"));

        let ooda = build_system_prompt_blocks(AgentLoopMode::OodaReAct, &[], &[], None);
        assert!(ooda[0].contains("MODE: Physical Execution (OODA-ReAct)"));
        assert!(!ooda[0].contains("MODE: Pure Reasoning"));
    }

    #[test]
    fn build_system_prompt_blocks_with_project_context() {
        let project_ctx = vec![
            "# AGENTS.md\nYou are an IDE assistant.".to_string(),
            "# rules/safety.md\nNever delete files.".to_string(),
        ];
        let blocks = build_system_prompt_blocks(AgentLoopMode::React, &project_ctx, &[], None);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("SAFETY-CRITICAL RULES"));
        assert!(blocks[1].contains("# AGENTS.md\nYou are an IDE assistant."));
        assert!(blocks[1].contains("# rules/safety.md\nNever delete files."));
    }

    #[test]
    fn build_system_prompt_blocks_with_per_message_context() {
        let context_blocks = vec![roz_v1::ContentBlock {
            label: Some("Active File".into()),
            block: Some(roz_v1::content_block::Block::Text("fn main() {}".into())),
        }];
        let blocks = build_system_prompt_blocks(AgentLoopMode::React, &[], &context_blocks, None);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("SAFETY-CRITICAL RULES"));
        assert!(blocks[1].contains("[Active File]\nfn main() {}"));
    }

    #[test]
    fn build_system_prompt_blocks_combined() {
        let project_ctx = vec!["# AGENTS.md\nBe helpful.".to_string()];
        let context_blocks = vec![roz_v1::ContentBlock {
            label: None,
            block: Some(roz_v1::content_block::Block::Text("open file content".into())),
        }];
        let blocks = build_system_prompt_blocks(AgentLoopMode::React, &project_ctx, &context_blocks, None);

        // 3 blocks: constitution, project context, per-message context.
        assert_eq!(blocks.len(), 3);
        assert!(blocks[0].starts_with("SAFETY-CRITICAL RULES"));
        assert!(blocks[1].contains("# AGENTS.md\nBe helpful."));
        assert!(blocks[2].contains("[Context]\nopen file content"));
    }

    #[test]
    fn build_system_prompt_blocks_skips_empty_project_context() {
        let project_ctx = vec!["  ".to_string(), String::new(), "real content".to_string()];
        let blocks = build_system_prompt_blocks(AgentLoopMode::React, &project_ctx, &[], None);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("SAFETY-CRITICAL RULES"));
        assert!(blocks[1].contains("real content"));
        // Empty/whitespace-only entries should be filtered out.
        assert!(!blocks[1].contains("  "));
    }

    #[test]
    fn plan_mode_permissions_blocks_physical() {
        let base = vec![
            SessionPermissionRule {
                tool_pattern: "move_arm".into(),
                policy: "require_confirmation".into(),
                category: Some("physical".into()),
                reason: None,
            },
            SessionPermissionRule {
                tool_pattern: "get_sensor".into(),
                policy: "allow".into(),
                category: Some("pure".into()),
                reason: None,
            },
        ];

        let plan = plan_mode_permissions(&base);
        assert_eq!(plan.len(), 2);

        // Physical tool -> blocked in plan mode
        assert_eq!(plan[0].tool_pattern, "move_arm");
        assert_eq!(plan[0].policy, "block");
        assert_eq!(plan[0].reason.as_deref(), Some("plan mode: observation only"));

        // Pure tool -> unchanged
        assert_eq!(plan[1].tool_pattern, "get_sensor");
        assert_eq!(plan[1].policy, "allow");
        assert!(plan[1].reason.is_none());
    }

    // -----------------------------------------------------------------------
    // core_team_event_to_proto conversion
    // -----------------------------------------------------------------------

    #[test]
    fn team_event_worker_started_to_proto() {
        use roz_core::team::TeamEvent as CoreTeamEvent;
        use roz_v1::team_event::Event;
        use uuid::Uuid;

        let worker_id = Uuid::nil();
        let event = CoreTeamEvent::WorkerStarted {
            worker_id,
            host_id: "host-001".to_string(),
        };

        let proto = core_team_event_to_proto(event);
        match proto.event {
            Some(Event::WorkerStarted(ws)) => {
                assert_eq!(ws.worker_id, worker_id.to_string());
                assert_eq!(ws.host_id, "host-001");
            }
            other => panic!("expected WorkerStarted, got {other:?}"),
        }
    }

    #[test]
    fn team_event_worker_failed_to_proto_all_reasons() {
        use roz_core::team::{TeamEvent as CoreTeamEvent, WorkerFailReason};
        use roz_v1::team_event::Event;
        use uuid::Uuid;

        let worker_id = Uuid::max();
        let cases = [
            (WorkerFailReason::EStop, "e_stop"),
            (WorkerFailReason::Timeout, "timeout"),
            (WorkerFailReason::ModelError, "model_error"),
            (WorkerFailReason::SafetyViolation, "safety_violation"),
        ];

        for (reason, expected_str) in cases {
            let event = CoreTeamEvent::WorkerFailed { worker_id, reason };
            let proto = core_team_event_to_proto(event);
            match proto.event {
                Some(Event::WorkerFailed(wf)) => {
                    assert_eq!(wf.worker_id, worker_id.to_string());
                    assert_eq!(wf.reason, expected_str, "reason string mismatch for {expected_str}");
                }
                other => panic!("expected WorkerFailed, got {other:?}"),
            }
        }
    }

    #[test]
    fn team_event_worker_phase_mode_strings() {
        use roz_core::phases::PhaseMode;
        use roz_core::team::TeamEvent as CoreTeamEvent;
        use roz_v1::team_event::Event;
        use uuid::Uuid;

        let worker_id = Uuid::nil();

        let react_event = CoreTeamEvent::WorkerPhase {
            worker_id,
            phase: 0,
            mode: PhaseMode::React,
        };
        let proto = core_team_event_to_proto(react_event);
        match proto.event {
            Some(Event::WorkerPhase(wp)) => {
                assert_eq!(wp.mode, "react");
                assert_eq!(wp.phase, 0);
            }
            other => panic!("expected WorkerPhase(react), got {other:?}"),
        }

        let ooda_event = CoreTeamEvent::WorkerPhase {
            worker_id,
            phase: 2,
            mode: PhaseMode::OodaReAct,
        };
        let proto = core_team_event_to_proto(ooda_event);
        match proto.event {
            Some(Event::WorkerPhase(wp)) => {
                assert_eq!(wp.mode, "ooda_react");
                assert_eq!(wp.phase, 2);
            }
            other => panic!("expected WorkerPhase(ooda_react), got {other:?}"),
        }
    }

    #[test]
    fn team_event_worker_tool_call_to_proto() {
        use roz_core::team::TeamEvent as CoreTeamEvent;
        use roz_v1::team_event::Event;
        use uuid::Uuid;

        let worker_id = Uuid::nil();
        let event = CoreTeamEvent::WorkerToolCall {
            worker_id,
            tool: "move_arm".to_string(),
        };

        let proto = core_team_event_to_proto(event);
        match proto.event {
            Some(Event::WorkerToolCall(wtc)) => {
                assert_eq!(wtc.worker_id, worker_id.to_string());
                assert_eq!(wtc.tool, "move_arm");
            }
            other => panic!("expected WorkerToolCall, got {other:?}"),
        }
    }

    #[test]
    fn team_event_worker_completed_to_proto() {
        use roz_core::team::TeamEvent as CoreTeamEvent;
        use roz_v1::team_event::Event;
        use uuid::Uuid;

        let worker_id = Uuid::max();
        let event = CoreTeamEvent::WorkerCompleted {
            worker_id,
            result: "task finished successfully".to_string(),
        };

        let proto = core_team_event_to_proto(event);
        match proto.event {
            Some(Event::WorkerCompleted(wc)) => {
                assert_eq!(wc.worker_id, worker_id.to_string());
                assert_eq!(wc.result, "task finished successfully");
            }
            other => panic!("expected WorkerCompleted, got {other:?}"),
        }
    }

    #[test]
    fn team_event_worker_approval_requested_to_proto() {
        use roz_core::team::TeamEvent as CoreTeamEvent;
        use roz_v1::team_event::Event;
        use uuid::Uuid;

        let worker_id = Uuid::nil();
        let task_id = Uuid::max();
        let event = CoreTeamEvent::WorkerApprovalRequested {
            worker_id,
            task_id,
            approval_id: "apr-123".to_string(),
            tool_name: "exec_command".to_string(),
            reason: "requires human approval".to_string(),
            timeout_secs: 45,
        };

        let proto = core_team_event_to_proto(event);
        match proto.event {
            Some(Event::WorkerApprovalRequested(requested)) => {
                assert_eq!(requested.worker_id, worker_id.to_string());
                assert_eq!(requested.task_id, task_id.to_string());
                assert_eq!(requested.approval_id, "apr-123");
                assert_eq!(requested.tool_name, "exec_command");
                assert_eq!(requested.reason, "requires human approval");
                assert_eq!(requested.timeout_secs, 45);
            }
            other => panic!("expected WorkerApprovalRequested, got {other:?}"),
        }
    }

    #[test]
    fn team_event_worker_approval_resolved_to_proto() {
        use roz_core::team::TeamEvent as CoreTeamEvent;
        use roz_v1::team_event::Event;
        use uuid::Uuid;

        let worker_id = Uuid::nil();
        let task_id = Uuid::max();
        let event = CoreTeamEvent::WorkerApprovalResolved {
            worker_id,
            task_id,
            approval_id: "apr-123".to_string(),
            approved: true,
            modifier: Some(serde_json::json!({"speed": 0.2})),
        };

        let proto = core_team_event_to_proto(event);
        match proto.event {
            Some(Event::WorkerApprovalResolved(resolved)) => {
                assert_eq!(resolved.worker_id, worker_id.to_string());
                assert_eq!(resolved.task_id, task_id.to_string());
                assert_eq!(resolved.approval_id, "apr-123");
                assert!(resolved.approved);
                assert!(resolved.modifier.is_some());
            }
            other => panic!("expected WorkerApprovalResolved, got {other:?}"),
        }
    }

    #[test]
    fn decode_team_event_payload_accepts_sequenced_wrapper() {
        let worker_id = uuid::Uuid::new_v4();
        let payload = serde_json::to_vec(&SequencedTeamEvent {
            seq: 7,
            timestamp_ns: 123_456,
            event: CoreTeamEvent::WorkerToolCall {
                worker_id,
                tool: "move_arm".into(),
            },
        })
        .unwrap();

        let decoded = decode_team_event_payload(&payload).expect("sequenced payload should decode");
        match decoded {
            CoreTeamEvent::WorkerToolCall { worker_id: id, tool } => {
                assert_eq!(id, worker_id);
                assert_eq!(tool, "move_arm");
            }
            other => panic!("expected WorkerToolCall, got {other:?}"),
        }
    }

    #[test]
    fn decode_team_event_payload_still_accepts_legacy_payload() {
        let worker_id = uuid::Uuid::new_v4();
        let payload = serde_json::to_vec(&CoreTeamEvent::WorkerStarted {
            worker_id,
            host_id: "host-1".into(),
        })
        .unwrap();

        let decoded = decode_team_event_payload(&payload).expect("legacy payload should decode");
        match decoded {
            CoreTeamEvent::WorkerStarted { worker_id: id, host_id } => {
                assert_eq!(id, worker_id);
                assert_eq!(host_id, "host-1");
            }
            other => panic!("expected WorkerStarted, got {other:?}"),
        }
    }
}
