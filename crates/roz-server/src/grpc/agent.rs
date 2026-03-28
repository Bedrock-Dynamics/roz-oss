//! gRPC `AgentService` implementation — bidirectional streaming session state machine.
//!
//! The first message from the client must be `StartSession`. This triggers:
//! 1. Auth validation (API key via the injected `GrpcAuth` trait).
//! 2. Session metadata written to Postgres (`roz_agent_sessions`).
//! 3. `SessionStarted` acknowledgement sent back with session ID, resolved model, and permissions.
//!
//! After `StartSession`, `UserMessage` dispatches an `AgentLoop::run_streaming()` turn,
//! forwarding streaming deltas to the client. `ToolResult` messages resolve pending
//! remote tool calls. `CancelTurn` / `CancelSession` handle lifecycle cleanup.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::Stream;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::constitution::build_constitution;
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::dispatch::remote::{
    PendingApprovals, PendingResults, RemoteToolCall, RemoteToolExecutor, resolve_approval, resolve_pending,
};
use roz_agent::model::types::StreamChunk;
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_core::auth::AuthIdentity;
use roz_core::tools::ToolCategory;

use roz_core::team::{TeamEvent as CoreTeamEvent, WorkerFailReason};
use roz_nats::team::{TEAM_STREAM, team_subject_pattern};

use super::convert::value_to_struct;
use super::roz_v1::agent_service_server::AgentService;
use super::roz_v1::{self, SessionRequest, SessionResponse, WatchTeamRequest, session_request, session_response};

/// Keepalive interval while an agent turn is in progress.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Default tool timeout for remote tool execution.
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Auth trait — allows the binary crate to inject its own auth implementation
// ---------------------------------------------------------------------------

/// Trait for authenticating gRPC requests.
///
/// The binary crate implements this by delegating to its `extract_auth`
/// function, which has access to `AppState` (DB pool for API key lookup).
/// This indirection keeps the library crate independent of the binary's
/// auth wiring.
#[tonic::async_trait]
pub trait GrpcAuth: Send + Sync + 'static {
    async fn authenticate(&self, pool: &PgPool, auth_header: Option<&str>) -> Result<AuthIdentity, String>;
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// In-flight session state held for the lifetime of a single `StreamSession` call.
struct Session {
    id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    #[allow(dead_code)]
    environment_id: uuid::Uuid,
    model_name: String,
    max_context_tokens: u32,
    tools: Vec<roz_core::tools::ToolSchema>,
    /// Proto-declared categories per tool (for dispatcher registration).
    tool_categories: HashMap<String, ToolCategory>,
    messages: Vec<roz_agent::model::types::Message>,
    /// Client-provided project context (AGENTS.md, .substrate/rules/*.md, etc.)
    /// sent once at session start. Included in the system prompt for every turn.
    project_context: Vec<String>,
    #[allow(dead_code)]
    cancel_token: tokio_util::sync::CancellationToken,
    #[allow(dead_code)]
    base_permissions: Vec<roz_v1::PermissionRule>,
    #[allow(dead_code)]
    active_permissions: Vec<roz_v1::PermissionRule>,
    /// Workflow context injected on the next `UserMessage` turn (from `RegisterTools`).
    pending_system_context: Option<String>,
    #[allow(dead_code)]
    pub host_id: Option<String>,
}

/// State for an active agent turn, shared between the session loop and relay tasks.
struct ActiveTurn {
    /// Resolves pending remote tool calls when the client sends `ToolResult`.
    pending: PendingResults,
    /// D2: Resolves Roz-authoritative approvals when the client sends `PermissionDecision`.
    pending_approvals: PendingApprovals,
    /// Cancel token scoped to this turn.
    cancel_token: tokio_util::sync::CancellationToken,
    /// Handle to the spawned agent task (dropped on turn completion).
    _handle: tokio::task::JoinHandle<()>,
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
    auth: Arc<dyn GrpcAuth>,
    default_model: String,
    gateway_url: String,
    api_key: String,
    model_timeout_secs: u64,
    anthropic_provider: String,
    direct_api_key: Option<String>,
    fallback_model_name: Option<String>,
}

impl AgentServiceImpl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        http_client: reqwest::Client,
        restate_ingress_url: String,
        nats_client: Option<async_nats::Client>,
        auth: Arc<dyn GrpcAuth>,
        default_model: String,
        gateway_url: String,
        api_key: String,
        model_timeout_secs: u64,
        anthropic_provider: String,
        direct_api_key: Option<String>,
        fallback_model_name: Option<String>,
    ) -> Self {
        Self {
            pool,
            http_client,
            restate_ingress_url,
            nats_client,
            auth,
            default_model,
            gateway_url,
            api_key,
            model_timeout_secs,
            anthropic_provider,
            direct_api_key,
            fallback_model_name,
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
        let auth = self.auth.clone();
        let default_model = self.default_model.clone();
        let gateway_url = self.gateway_url.clone();
        let api_key = self.api_key.clone();
        let model_timeout_secs = self.model_timeout_secs;
        let anthropic_provider = self.anthropic_provider.clone();
        let direct_api_key = self.direct_api_key.clone();
        let fallback_model_name = self.fallback_model_name.clone();
        let nats_client = self.nats_client.clone();

        // Extract auth header from gRPC metadata before consuming the request.
        let auth_header = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

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
                &auth,
                &default_model,
                &model_config,
                auth_header.as_deref(),
                &mut inbound,
                nats_client.as_ref(),
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
        // --- Auth ---
        let auth_header = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let auth_identity = self
            .auth
            .authenticate(&self.pool, auth_header.as_deref())
            .await
            .map_err(Status::unauthenticated)?;

        let tenant_id = auth_identity.tenant_id().0;
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

                        let core_event: CoreTeamEvent = match serde_json::from_slice(&msg.payload) {
                            Ok(ev) => ev,
                            Err(e) => {
                                tracing::warn!(error = %e, "WatchTeam: failed to decode TeamEvent, skipping");
                                continue;
                            }
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

// ---------------------------------------------------------------------------
// Session message loop
// ---------------------------------------------------------------------------

#[tracing::instrument(
    name = "agent_session.stream",
    skip(tx, pool, auth, model_config, inbound, nats_client),
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
    auth: &Arc<dyn GrpcAuth>,
    default_model: &str,
    model_config: &ModelConfig,
    auth_header: Option<&str>,
    inbound: &mut Streaming<SessionRequest>,
    nats_client: Option<&async_nats::Client>,
) {
    let mut session: Option<Session> = None;
    let mut cancelled = false;
    let mut active_turn: Option<ActiveTurn> = None;
    // When a turn completes, the agent task sends the output (or None on error)
    // so the session loop can update messages, run compaction, and allow the next turn.
    let (turn_done_tx, mut turn_done_rx) = mpsc::channel::<Option<roz_agent::agent_loop::AgentOutput>>(1);

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
                if let Some(Some(output)) = turn_output
                    && let Some(ref mut sess) = session
                {
                    // Update session messages with the turn output (move, not clone).
                    tracing::info!(
                        session_id = %sess.id,
                        tenant_id = %sess.tenant_id,
                        environment_id = %sess.environment_id,
                        model = %sess.model_name,
                        input_tokens = output.total_usage.input_tokens,
                        output_tokens = output.total_usage.output_tokens,
                        cycles = output.cycles,
                        "agent_session.turn_complete"
                    );
                    // Persist token usage to DB for metering/audit.
                    if let Err(e) = roz_db::agent_sessions::update_session_usage(
                        pool,
                        sess.tenant_id,
                        sess.id,
                        i64::from(output.total_usage.input_tokens),
                        i64::from(output.total_usage.output_tokens),
                        1, // one turn completed
                    )
                    .await
                    {
                        tracing::warn!(session_id = %sess.id, error = %e, "failed to update session usage");
                    }

                    sess.messages = output.messages;

                    // Between-turn compaction (Level 1 + 2 only; no model for Level 3).
                    // Budget matches typical model context windows (200k tokens for claude-sonnet-4).
                    let ctx_mgr = roz_agent::context::ContextManager::new(sess.max_context_tokens);
                    let events = ctx_mgr.compact_escalating(&mut sess.messages, None).await;
                    for event in &events {
                        let _ = tx
                            .send(Ok(SessionResponse {
                                response: Some(session_response::Response::ContextCompacted(
                                    roz_v1::ContextCompacted {
                                        level: match event.level {
                                            roz_agent::context::CompactionLevel::ToolResults => {
                                                "tool_results"
                                            }
                                            roz_agent::context::CompactionLevel::Thinking => {
                                                "thinking"
                                            }
                                            roz_agent::context::CompactionLevel::Summary => {
                                                "summary"
                                            }
                                        }
                                        .into(),
                                        messages_before: u32::try_from(event.messages_before)
                                            .unwrap_or(u32::MAX),
                                        messages_after: u32::try_from(event.messages_after)
                                            .unwrap_or(u32::MAX),
                                        tokens_before: event.tokens_before,
                                        tokens_after: event.tokens_after,
                                        summary: event.summary.clone(),
                                    },
                                )),
                            }))
                            .await;
                    }
                }
                active_turn = None;
                continue;
            }
        };

        match req.request {
            Some(session_request::Request::Start(start)) => {
                if !handle_start(tx, pool, auth, default_model, auth_header, start, &mut session).await {
                    break;
                }
                if let Some(ref sess) = session {
                    tracing::Span::current().record("session_id", tracing::field::display(sess.id));
                    tracing::Span::current().record("tenant_id", tracing::field::display(sess.tenant_id));
                    tracing::Span::current().record("environment_id", tracing::field::display(sess.environment_id));
                    tracing::info!(
                        session_id = %sess.id,
                        tenant_id = %sess.tenant_id,
                        environment_id = %sess.environment_id,
                        model = %sess.model_name,
                        tools_count = sess.tools.len(),
                        history_messages = sess.messages.len(),
                        project_context_count = sess.project_context.len(),
                        "agent_session.started"
                    );

                    // Spawn telemetry relay: subscribe to NATS telemetry for the
                    // session's host and forward updates on the gRPC stream.
                    if let Some(ref host_id_str) = sess.host_id
                        && let Some(nats) = nats_client
                    {
                        spawn_telemetry_relay(pool, nats, host_id_str, tx).await;
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
                let (tool_request_tx, mut tool_request_rx) = mpsc::channel::<RemoteToolCall>(16);
                let pending: PendingResults = Arc::new(Mutex::new(HashMap::new()));
                // D2: Roz-authoritative approval map for this turn.
                let pending_approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));

                // Register remote tool executors for each client-declared tool.
                // D3b: prefer per-turn tools from the message (industry-standard
                // stateless pattern); fall back to session-accumulated tools for
                // backward compatibility with older clients.
                let mut dispatcher = ToolDispatcher::new(DEFAULT_TOOL_TIMEOUT);
                let (turn_tools, turn_categories): (Vec<roz_core::tools::ToolSchema>, HashMap<String, ToolCategory>) =
                    if msg.tools.is_empty() {
                        // Backward compat: old clients register via RegisterTools /
                        // StartSession and send UserMessage with no tools field.
                        let cats = sess.tool_categories.clone();
                        (sess.tools.clone(), cats)
                    } else {
                        let cats = msg
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
                        let schemas = msg
                            .tools
                            .iter()
                            .cloned()
                            .map(roz_core::tools::ToolSchema::from)
                            .collect();
                        (schemas, cats)
                    };
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

                let mode = match msg.ai_mode.as_deref() {
                    Some("ooda_react" | "ooda") => AgentLoopMode::OodaReAct,
                    _ => AgentLoopMode::React,
                };

                // D3b: per-turn system_context takes priority over the
                // stashed pending_system_context from a prior RegisterTools.
                let pending_ctx = msg.system_context.or_else(|| sess.pending_system_context.take());
                let system_prompt = build_system_prompt_blocks(mode, &sess.project_context, &msg.context, pending_ctx);

                // Warn if system prompt consumes >30% of context budget.
                if sess.max_context_tokens > 0 {
                    #[allow(clippy::cast_possible_truncation)]
                    let system_chars: u32 = system_prompt
                        .iter()
                        .map(String::len)
                        .sum::<usize>()
                        .min(u32::MAX as usize) as u32;
                    let estimated_tokens = f64::from(system_chars) / 4.0;
                    let budget_fraction = estimated_tokens / f64::from(sess.max_context_tokens);
                    if budget_fraction > 0.30 {
                        tracing::warn!(
                            session_id = %sess.id,
                            estimated_system_tokens = system_chars / 4,
                            max_context_tokens = sess.max_context_tokens,
                            budget_pct = format!("{:.1}%", budget_fraction * 100.0),
                            "system prompt consumes >30% of context budget"
                        );
                    }
                }

                let agent_input = AgentInput {
                    task_id: sess.id.to_string(),
                    tenant_id: sess.tenant_id.to_string(),
                    system_prompt,
                    user_message: user_content,
                    max_cycles: 200, // safety ceiling, not behavioral limit
                    max_tokens: 8192,
                    max_context_tokens: sess.max_context_tokens,
                    mode,
                    phases: vec![],
                    tool_choice: None,
                    response_schema: None,
                    streaming: true,
                    history: sess.messages.clone(), // pass accumulated history
                };

                // Create the chunk channel for streaming.
                let (chunk_tx, mut chunk_rx) = mpsc::channel::<StreamChunk>(64);

                // Create the presence side-channel for UI presence hints.
                let (presence_tx, mut presence_rx) = mpsc::channel::<roz_agent::agent_loop::PresenceSignal>(16);

                // Oneshot so the agent task can wait for the chunk relay to drain
                // before sending TurnComplete. Without this, TurnComplete can race
                // ahead of the final TextDelta/ThinkingDelta messages on the gRPC
                // stream because both the agent task and relay task send to the same
                // mpsc channel independently.
                let (relay_done_tx, relay_done_rx) = tokio::sync::oneshot::channel::<()>();

                let safety = SafetyStack::new(vec![]);
                let spatial = Box::new(MockSpatialContextProvider::empty());
                let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial)
                    .with_pending_approvals(pending_approvals.clone());

                let turn_cancel = tokio_util::sync::CancellationToken::new();

                // Use client-provided message_id if present, else generate one.
                let message_id = msg
                    .message_id
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                tracing::info!(
                    session_id = %sess.id,
                    message_id = %message_id,
                    "agent_session.turn_started"
                );

                // Spawn agent loop task.
                let agent_tx = tx.clone();
                let turn_cancel_agent = turn_cancel.clone();
                let message_id_agent = message_id.clone();
                let turn_done = turn_done_tx.clone();
                let handle = tokio::spawn(async move {
                    let result = tokio::select! {
                        res = agent_loop.run_streaming(agent_input, chunk_tx, presence_tx) => res,
                        () = turn_cancel_agent.cancelled() => {
                            Err(roz_agent::error::AgentError::Safety("turn cancelled".into()))
                        }
                    };

                    // Cancel keepalive regardless of outcome.
                    turn_cancel_agent.cancel();

                    // Wait for the chunk relay task to finish draining all buffered
                    // chunks before sending TurnComplete/Error. This guarantees the
                    // client sees every TextDelta before TurnComplete on the stream.
                    // Timeout prevents a hung relay from blocking turn completion.
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), relay_done_rx).await;

                    match result {
                        Ok(output) => {
                            let _ = agent_tx
                                .send(Ok(SessionResponse {
                                    response: Some(session_response::Response::TurnComplete(roz_v1::TurnComplete {
                                        message_id: message_id_agent,
                                        usage: Some(roz_v1::TokenUsage {
                                            input_tokens: output.total_usage.input_tokens,
                                            output_tokens: output.total_usage.output_tokens,
                                            cache_read_tokens: 0,
                                            cache_creation_tokens: 0,
                                        }),
                                        stop_reason: "end_turn".into(),
                                    })),
                                }))
                                .await;
                            // Send the output so the session loop can update messages and compact.
                            let _ = turn_done.send(Some(output)).await;
                        }
                        Err(e) if e.to_string().contains("turn cancelled") => {
                            // Cancellation is not an error — send TurnComplete with stop_reason
                            // "cancelled" so clients can distinguish cancelled turns from failures.
                            let _ = agent_tx
                                .send(Ok(SessionResponse {
                                    response: Some(session_response::Response::TurnComplete(roz_v1::TurnComplete {
                                        message_id: message_id_agent,
                                        usage: None,
                                        stop_reason: "cancelled".into(),
                                    })),
                                }))
                                .await;
                            let _ = turn_done.send(None).await;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "agent turn failed");
                            // Sanitize: don't leak internal URLs or gateway details to client
                            let client_message = match &e {
                                roz_agent::error::AgentError::Model(_)
                                | roz_agent::error::AgentError::Http(_)
                                | roz_agent::error::AgentError::Stream { .. } => {
                                    "Model request failed. Please try again.".to_string()
                                }
                                roz_agent::error::AgentError::Safety(_) => e.to_string(),
                                _ => "Internal error. Please try again.".to_string(),
                            };
                            let _ = agent_tx
                                .send(Ok(SessionResponse {
                                    response: Some(session_response::Response::Error(roz_v1::SessionError {
                                        code: "agent_error".into(),
                                        message: client_message,
                                        retryable: e.is_retryable(),
                                    })),
                                }))
                                .await;
                            let _ = turn_done.send(None).await;
                        }
                    }
                });

                active_turn = Some(ActiveTurn {
                    pending: pending.clone(),
                    pending_approvals: pending_approvals.clone(),
                    cancel_token: turn_cancel.clone(),
                    _handle: handle,
                });

                // Spawn chunk relay task: reads from chunk_rx, maps to SessionResponse.
                // Signals relay_done_tx when all chunks have been forwarded.
                let relay_tx = tx.clone();
                let message_id_relay = message_id.clone();
                tokio::spawn(async move {
                    while let Some(chunk) = chunk_rx.recv().await {
                        let response = match chunk {
                            StreamChunk::TextDelta(content) => {
                                Some(session_response::Response::TextDelta(roz_v1::TextDelta {
                                    message_id: message_id_relay.clone(),
                                    content,
                                }))
                            }
                            StreamChunk::ThinkingDelta(content) => {
                                Some(session_response::Response::ThinkingDelta(roz_v1::ThinkingDelta {
                                    message_id: message_id_relay.clone(),
                                    content,
                                }))
                            }
                            // ToolUseStart and ToolUseInputDelta are handled internally by
                            // the agent loop; the client sees ToolRequest when RemoteToolExecutor
                            // sends calls. Skip these intermediate chunks.
                            StreamChunk::ToolUseStart { .. }
                            | StreamChunk::ToolUseInputDelta(_)
                            | StreamChunk::Usage(_)
                            | StreamChunk::Done(_) => None,
                        };

                        if let Some(resp) = response
                            && relay_tx
                                .send(Ok(SessionResponse { response: Some(resp) }))
                                .await
                                .is_err()
                        {
                            break; // client disconnected
                        }
                    }
                    // Signal the agent task that all chunks have been relayed.
                    let _ = relay_done_tx.send(());
                });

                // Spawn tool relay task: reads from tool_request_rx, maps to ToolRequest.
                let tool_relay_tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(call) = tool_request_rx.recv().await {
                        let response = session_response::Response::ToolRequest(roz_v1::ToolRequest {
                            tool_call_id: call.id,
                            tool_name: call.name,
                            parameters: Some(value_to_struct(call.parameters)),
                            timeout_ms: call.timeout_ms,
                        });

                        if tool_relay_tx
                            .send(Ok(SessionResponse {
                                response: Some(response),
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
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

                // Spawn presence relay task: reads PresenceSignal from agent loop,
                // maps to SessionResponse::PresenceHint / ActivityUpdate,
                // writes analytics events to the DB (with timeout), and
                // rate-limits gRPC sends to avoid flooding the client.
                let presence_relay_tx = tx.clone();
                let presence_pool = pool.clone();
                let presence_session_id = sess.id;
                let presence_tenant_id = sess.tenant_id;
                tokio::spawn(async move {
                    use roz_agent::agent_loop::PresenceSignal;
                    use tokio::time::Instant;

                    let mut last_sent = Instant::now() - Duration::from_secs(1);
                    let min_interval = Duration::from_millis(250);

                    while let Some(signal) = presence_rx.recv().await {
                        // Truncate detail to 512 chars (matches DB CHECK constraint).
                        let truncate = |s: &str| -> String { s.chars().take(512).collect() };

                        let (response, evt_type, evt_state, evt_detail, evt_level, evt_reason, evt_progress) =
                            match &signal {
                                PresenceSignal::PresenceHint { level, reason } => {
                                    let reason_t = truncate(reason);
                                    (
                                        session_response::Response::PresenceHint(roz_v1::PresenceHint {
                                            level: level.as_str().to_string(),
                                            reason: reason_t.clone(),
                                        }),
                                        "presence_hint",
                                        None::<&str>,
                                        None::<String>,
                                        Some(level.as_str()),
                                        Some(reason_t),
                                        None,
                                    )
                                }
                                PresenceSignal::ActivityUpdate {
                                    state,
                                    detail,
                                    progress,
                                } => {
                                    let detail_t = truncate(detail);
                                    (
                                        session_response::Response::ActivityUpdate(roz_v1::ActivityUpdate {
                                            state: state.as_str().to_string(),
                                            detail: detail_t.clone(),
                                            progress: *progress,
                                        }),
                                        "activity_update",
                                        Some(state.as_str()),
                                        Some(detail_t),
                                        None,
                                        None::<String>,
                                        *progress,
                                    )
                                }
                            };

                        // Rate-limit client-facing gRPC sends (max ~4/sec) — happens first so
                        // the client is never blocked waiting for analytics writes.
                        let now = Instant::now();
                        let should_break = if now.duration_since(last_sent) >= min_interval {
                            last_sent = now;
                            presence_relay_tx
                                .send(Ok(SessionResponse {
                                    response: Some(response),
                                }))
                                .await
                                .is_err()
                        } else {
                            false
                        };

                        // Fire-and-forget analytics write — never blocks the presence relay.
                        // Convert borrowed &str fields to owned Strings so signal can be dropped.
                        let pool_clone = presence_pool.clone();
                        let evt_state_owned = evt_state.map(str::to_owned);
                        let evt_level_owned = evt_level.map(str::to_owned);
                        tokio::spawn(async move {
                            let db_result = tokio::time::timeout(
                                Duration::from_secs(5),
                                roz_db::activity_events::insert_activity_event(
                                    &pool_clone,
                                    presence_session_id,
                                    presence_tenant_id,
                                    evt_type,
                                    evt_state_owned.as_deref(),
                                    evt_detail.as_deref(),
                                    evt_level_owned.as_deref(),
                                    evt_reason.as_deref(),
                                    evt_progress,
                                ),
                            )
                            .await;
                            match db_result {
                                Ok(Err(e)) => tracing::warn!(error = %e, "failed to write activity event"),
                                Err(_) => tracing::warn!("activity event DB write timed out (5s)"),
                                Ok(Ok(())) => {}
                            }
                        });

                        if should_break {
                            break;
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
                    let resolved = resolve_approval(&turn.pending_approvals, &decision.tool_call_id, decision.approved);
                    if !resolved {
                        tracing::warn!(
                            tool_call_id = %decision.tool_call_id,
                            approved = decision.approved,
                            "PermissionDecision for unknown or already-resolved call"
                        );
                    }
                } else {
                    tracing::warn!(
                        tool_call_id = %decision.tool_call_id,
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
                    .map(|t| {
                        let cat = match t.category() {
                            roz_v1::ToolCategoryHint::ToolCategoryPure => ToolCategory::Pure,
                            _ => ToolCategory::Physical,
                        };
                        (t.name.clone(), cat)
                    })
                    .collect();

                // Append new tools (empty list = unregister only).
                let new_tools: Vec<roz_core::tools::ToolSchema> =
                    reg.tools.into_iter().map(roz_core::tools::ToolSchema::from).collect();

                let n_added = new_tools.len();
                sess.tools.extend(new_tools);
                sess.tool_categories.extend(new_categories);

                // Stash workflow context for the next UserMessage.
                if let Some(ctx) = reg.system_context {
                    sess.pending_system_context = Some(ctx);
                }

                tracing::info!(
                    session_id = %sess.id,
                    source = %source,
                    tools_added = n_added,
                    total_tools = sess.tools.len(),
                    "agent_session.register_tools"
                );
            }
            // Phase 1b stub: WebRTC signaling will be wired in Phase 2.
            Some(session_request::Request::WebrtcAnswer(_)) => {
                tracing::debug!("WebRTC answer received (not yet wired)");
            }
            None => {}
        }
    }

    // Cancel any in-flight turn.
    if let Some(ref turn) = active_turn {
        turn.cancel_token.cancel();
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
        && let Err(e) = roz_db::agent_sessions::complete_session(pool, s.tenant_id, s.id, status).await
    {
        tracing::warn!(session_id = %s.id, error = %e, "failed to complete session");
    }
}

/// Subscribe to NATS telemetry for a host and relay `TelemetryUpdate` messages
/// on the gRPC response stream.
///
/// Resolves `host_id` (UUID string) to the host's `name` (= `worker_id`) via Postgres,
/// then subscribes to `telemetry.{host_name}.>`. Each received NATS message is
/// converted to a `TelemetryUpdate` proto and forwarded on `tx`.
async fn spawn_telemetry_relay(
    pool: &PgPool,
    nats: &async_nats::Client,
    host_id_str: &str,
    tx: &mpsc::Sender<Result<SessionResponse, Status>>,
) {
    let host_uuid = match uuid::Uuid::parse_str(host_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::debug!(host_id = %host_id_str, error = %e, "invalid host_id UUID, skipping telemetry relay");
            return;
        }
    };

    let host = match roz_db::hosts::get_by_id(pool, host_uuid).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            tracing::debug!(host_id = %host_id_str, "host not found, skipping telemetry relay");
            return;
        }
        Err(e) => {
            tracing::warn!(host_id = %host_id_str, error = %e, "failed to look up host for telemetry relay");
            return;
        }
    };

    let telem_subject = match roz_nats::subjects::Subjects::telemetry_wildcard(&host.name) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(host_name = %host.name, error = %e, "invalid host name for telemetry subject");
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

    tracing::info!(subject = %telem_subject, host_id = %host_id_str, "telemetry relay started");

    let telem_tx = tx.clone();
    let host_id_owned = host_id_str.to_string();
    tokio::spawn(async move {
        let mut sub = telem_sub;
        while let Some(msg) = sub.next().await {
            if let Ok(data) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                let update = roz_v1::TelemetryUpdate {
                    host_id: host_id_owned.clone(),
                    timestamp: data["timestamp"].as_f64().unwrap_or(0.0),
                    joint_states: vec![],
                    end_effector_pose: None,
                    sensor_readings: std::collections::HashMap::new(),
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

/// Handle the `StartSession` message. Returns `true` to continue the loop,
/// `false` to break (fatal error or auth failure).
async fn handle_start(
    tx: &mpsc::Sender<Result<SessionResponse, Status>>,
    pool: &PgPool,
    auth: &Arc<dyn GrpcAuth>,
    default_model: &str,
    auth_header: Option<&str>,
    start: roz_v1::StartSession,
    session: &mut Option<Session>,
) -> bool {
    if session.is_some() {
        send_error(tx, "already_started", "session already started", false).await;
        return true; // non-fatal, just ignore
    }

    // Auth: validate the authorization header.
    let auth_identity = match auth.authenticate(pool, auth_header).await {
        Ok(id) => id,
        Err(err_msg) => {
            send_error(tx, "unauthenticated", &err_msg, false).await;
            return false; // fatal
        }
    };

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

    // Send SessionStarted acknowledgement.
    let started = SessionResponse {
        response: Some(session_response::Response::SessionStarted(roz_v1::SessionStarted {
            session_id: session_id.to_string(),
            model: model_name.clone(),
            permissions: base_permissions.clone(),
        })),
    };
    if tx.send(Ok(started)).await.is_err() {
        return false; // client disconnected
    }

    *session = Some(Session {
        id: session_id,
        tenant_id,
        environment_id: env_id,
        model_name,
        max_context_tokens,
        tools,
        tool_categories,
        messages,
        project_context: start.project_context,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        base_permissions: base_permissions.clone(),
        active_permissions: base_permissions,
        pending_system_context: None,
        host_id: start.host_id,
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
fn build_system_prompt_blocks(
    mode: AgentLoopMode,
    project_context: &[String],
    per_message_context: &[roz_v1::ContentBlock],
    pending_system_context: Option<String>,
) -> Vec<String> {
    let mut blocks = vec![build_constitution(mode)];

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

/// Send a `SessionError` response on the outbound channel.
async fn send_error(tx: &mpsc::Sender<Result<SessionResponse, Status>>, code: &str, message: &str, retryable: bool) {
    let _ = tx
        .send(Ok(SessionResponse {
            response: Some(session_response::Response::Error(roz_v1::SessionError {
                code: code.into(),
                message: message.into(),
                retryable,
            })),
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
                roz_core::phases::PhaseMode::OodaReAct => "ooda_re_act".to_string(),
            },
        }),
        CoreTeamEvent::WorkerToolCall { worker_id, tool } => Event::WorkerToolCall(roz_v1::WorkerToolCall {
            worker_id: worker_id.to_string(),
            tool,
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

/// Substrings that indicate a tool is "pure" (read-only / observational).
const PURE_VERBS: &[&str] = &["read", "get", "list", "search", "glob", "grep", "find"];

/// Derive default permission rules from tool schemas.
///
/// Physical tools default to `require_confirmation`; tools whose names contain
/// read-like verbs (get, list, search, etc.) default to `allow`.
fn derive_permissions(tools: &[roz_core::tools::ToolSchema]) -> Vec<roz_v1::PermissionRule> {
    if tools.is_empty() {
        // Sensible defaults when no tools are declared.
        return vec![
            roz_v1::PermissionRule {
                tool_pattern: "*".into(),
                policy: "require_confirmation".into(),
                category: Some("physical".into()),
                reason: Some("default: physical tools require confirmation".into()),
            },
            roz_v1::PermissionRule {
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

            roz_v1::PermissionRule {
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
fn plan_mode_permissions(base: &[roz_v1::PermissionRule]) -> Vec<roz_v1::PermissionRule> {
    base.iter()
        .map(|rule| {
            if rule.category.as_deref() == Some("physical") {
                roz_v1::PermissionRule {
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
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
            roz_v1::PermissionRule {
                tool_pattern: "move_arm".into(),
                policy: "require_confirmation".into(),
                category: Some("physical".into()),
                reason: None,
            },
            roz_v1::PermissionRule {
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
                assert_eq!(wp.mode, "ooda_re_act");
                assert_eq!(wp.phase, 2);
            }
            other => panic!("expected WorkerPhase(ooda_re_act), got {other:?}"),
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
}
