use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode, AgentOutput, PresenceSignal};
use roz_agent::constitution::build_constitution;
use roz_agent::delegation::DelegationTool;
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::error::AgentError;
use roz_agent::model::create_model;
use roz_agent::model::types::{Model, StreamChunk};
use roz_agent::safety::stack::SafetyStack;
use roz_agent::session_runtime::{
    SessionConfig, SessionRuntime, StreamingTurnExecutor, StreamingTurnHandle,
    StreamingTurnResult as RuntimeStreamingTurnResult, TurnExecutor, TurnInput, TurnOutput,
};
use roz_agent::spatial_provider::{
    NullWorldStateProvider, PrimedWorldStateProvider, WorldStateProvider, bootstrap_runtime_world_state_provider,
    format_runtime_world_state_bootstrap_note, world_state_has_runtime_data,
};
use roz_copper::handle::CopperHandle;
use roz_copper::{channels::ControllerState, evidence_archive::EvidenceArchive};
use roz_core::session::activity::RuntimeFailureKind;
use roz_core::session::control::SessionMode;
use roz_core::session::snapshot::FreshnessState;
use roz_core::tools::ToolCategory;
use roz_core::trust::{TrustLevel, TrustPosture};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::docker::DockerLauncher;
use crate::manifest::{ManifestError, ProjectManifest};
use crate::mcp::{McpError, McpManager, McpToolExecutor};
use crate::session::SessionStore;
use crate::spatial_docker::DockerSpatialProvider;
use crate::tools::bash::BashTool;
use crate::tools::env_start::EnvStartTool;
use crate::tools::env_stop::EnvStopTool;
use crate::tools::file_read::FileReadTool;
use crate::tools::file_write::FileWriteTool;

// ---------------------------------------------------------------------------
// Permission types (local to roz-local; no equivalent in roz-agent)
// ---------------------------------------------------------------------------

/// How the local runtime should handle physical tool calls that require
/// human approval in interactive sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Allow all tool calls without prompting (default for automated use).
    #[default]
    Auto,
    /// Prompt the user before any physical tool call.
    Ask,
    /// Block all physical tool calls unconditionally.
    Safe,
}

/// A `SafetyGuard` that enforces the active `PermissionMode`.
///
/// In `Auto` mode this guard is not added to the stack; in `Ask` mode it
/// emits a `RequireConfirmation` verdict; in `Safe` mode it blocks outright.
pub struct PermissionGuard {
    mode: PermissionMode,
}

impl PermissionGuard {
    pub const fn new(mode: PermissionMode) -> Self {
        Self { mode }
    }
}

#[async_trait::async_trait]
impl roz_agent::safety::stack::SafetyGuard for PermissionGuard {
    fn name(&self) -> &'static str {
        "permission_guard"
    }

    async fn check(
        &self,
        _action: &roz_core::tools::ToolCall,
        _state: &roz_core::spatial::WorldState,
    ) -> roz_core::safety::SafetyVerdict {
        match self.mode {
            PermissionMode::Auto => roz_core::safety::SafetyVerdict::Allow,
            PermissionMode::Ask => roz_core::safety::SafetyVerdict::RequireConfirmation {
                reason: "Permission required for physical tool call".to_string(),
                timeout_secs: 60,
            },
            PermissionMode::Safe => roz_core::safety::SafetyVerdict::Block {
                reason: "Physical tool calls are blocked in safe mode".to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("model error: {0}")]
    Model(#[from] AgentError),
    #[error("mode transition blocked: {0}")]
    ModeTransitionBlocked(String),
    #[error("agent error: {0}")]
    Agent(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

fn agent_error_to_turn_execution_failure(error: AgentError) -> roz_agent::session_runtime::TurnExecutionFailure {
    if std::env::var_os("ROZ_DEBUG_LOCAL_RUNTIME").is_some() {
        eprintln!("LocalRuntime agent error: {error:?}");
    }
    match error {
        AgentError::Safety(message) => {
            roz_agent::session_runtime::TurnExecutionFailure::new(RuntimeFailureKind::SafetyBlocked, message)
        }
        AgentError::ToolDispatch { message, .. } => {
            roz_agent::session_runtime::TurnExecutionFailure::new(RuntimeFailureKind::ToolError, message)
        }
        AgentError::CircuitBreakerTripped {
            consecutive_error_turns,
        } => roz_agent::session_runtime::TurnExecutionFailure::new(
            RuntimeFailureKind::CircuitBreakerTripped,
            format!("circuit breaker tripped after {consecutive_error_turns} consecutive all-error turns"),
        ),
        AgentError::Cancelled { .. } => {
            roz_agent::session_runtime::TurnExecutionFailure::new(RuntimeFailureKind::OperatorAbort, "turn cancelled")
        }
        other => {
            roz_agent::session_runtime::TurnExecutionFailure::new(RuntimeFailureKind::ModelError, other.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Local config (.roz/config.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct LocalConfig {
    api_key: Option<String>,
}

fn load_api_key(project_dir: &Path) -> Option<String> {
    // 1. ROZ_API_KEY env var (highest priority)
    if let Ok(key) = std::env::var("ROZ_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    // 2. ANTHROPIC_API_KEY env var
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    // 3. .roz/config.toml (lowest priority)
    let path = project_dir.join(".roz").join("config.toml");
    let content = std::fs::read_to_string(path).ok()?;
    let config: LocalConfig = toml::from_str(&content).ok()?;
    config.api_key
}

/// Resolve the effective base URL from env vars, falling back to manifest value.
fn resolve_base_url(manifest_url: &str) -> String {
    std::env::var("ROZ_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| manifest_url.to_string())
}

/// Resolve the permission mode from `ROZ_PERMISSION_MODE` env var.
const fn default_permission_mode(is_interactive: bool) -> PermissionMode {
    if is_interactive {
        PermissionMode::Ask
    } else {
        PermissionMode::Auto
    }
}

fn resolve_permission_mode_from_env(env_override: Option<&str>, is_interactive: bool) -> PermissionMode {
    match env_override {
        Some("ask") => PermissionMode::Ask,
        Some("safe") => PermissionMode::Safe,
        _ => default_permission_mode(is_interactive),
    }
}

/// Resolve the permission mode from `ROZ_PERMISSION_MODE` env var.
fn resolve_permission_mode() -> PermissionMode {
    use std::io::IsTerminal as _;

    resolve_permission_mode_from_env(
        std::env::var("ROZ_PERMISSION_MODE").ok().as_deref(),
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
    )
}

/// Build the prefixed model name from provider + name, used for routing to the correct backend.
fn build_model_name(provider: &str, name: &str) -> String {
    match provider {
        "anthropic" => {
            if name.starts_with("anthropic/") || name.starts_with("claude-") {
                name.to_string()
            } else {
                format!("anthropic/{name}")
            }
        }
        "ollama" => format!("ollama/{name}"),
        "local" => format!("local/{name}"),
        _ => format!("openai-compat/{name}"),
    }
}

/// Resolve the effective model name, checking `ROZ_MODEL` env var first.
fn resolve_model_name(manifest_provider: &str, manifest_name: &str, effective_base_url: &str) -> String {
    if let Ok(model_ov) = std::env::var("ROZ_MODEL")
        && !model_ov.is_empty()
    {
        // Infer provider from the model name or base URL
        if model_ov.starts_with("claude-") || model_ov.starts_with("anthropic/") {
            return model_ov;
        }
        if effective_base_url.contains("anthropic.com") {
            return format!("anthropic/{model_ov}");
        }
        return format!("openai-compat/{model_ov}");
    }
    build_model_name(manifest_provider, manifest_name)
}

fn load_blueprint_toml(project_dir: &Path) -> String {
    std::fs::read_to_string(project_dir.join("blueprint.toml")).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// LocalRuntime
// ---------------------------------------------------------------------------

/// A factory that creates a `Box<dyn Model>` on each call.
///
/// Used to decouple model creation from the runtime, enabling both
/// config-based creation (production) and injected mocks (testing).
type ModelFactory = Box<dyn Fn() -> Result<Box<dyn Model>, AgentError> + Send + Sync>;

#[derive(Debug, Clone)]
struct LocalModeState {
    trust_posture: TrustPosture,
    telemetry_freshness: FreshnessState,
}

impl Default for LocalModeState {
    fn default() -> Self {
        Self {
            trust_posture: TrustPosture::default(),
            telemetry_freshness: FreshnessState::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
struct ModeTransitionAssessment {
    mode: AgentLoopMode,
    blocker: Option<String>,
}

impl ModeTransitionAssessment {
    fn react(blocker: impl Into<String>) -> Self {
        Self {
            mode: AgentLoopMode::React,
            blocker: Some(blocker.into()),
        }
    }

    const fn ooda_ready() -> Self {
        Self {
            mode: AgentLoopMode::OodaReAct,
            blocker: None,
        }
    }
}

#[allow(clippy::fn_params_excessive_bools)]
fn assess_embodied_mode_readiness(
    has_connections: bool,
    has_embodiment_manifest: bool,
    has_physical_tools: bool,
    has_world_state_tools: bool,
    controller_estop_reason: Option<&str>,
    trust_posture: &TrustPosture,
    telemetry_freshness: &FreshnessState,
) -> ModeTransitionAssessment {
    if !has_connections {
        return ModeTransitionAssessment::react("no connected embodied environment");
    }
    if !has_embodiment_manifest {
        return ModeTransitionAssessment::react(
            "connected MCP environment is not embodiment-backed; add embodiment.toml (legacy robot.toml also accepted) to enable OodaReAct",
        );
    }
    if !has_physical_tools {
        return ModeTransitionAssessment::react("connected MCP environment does not expose physical actuation tools");
    }
    if !has_world_state_tools {
        return ModeTransitionAssessment::react(
            "connected MCP environment does not expose bounded world-state observation tools",
        );
    }
    if let Some(reason) = controller_estop_reason {
        return ModeTransitionAssessment::react(format!(
            "controller safety interlock active: {reason}; clear e-stop before entering OodaReAct"
        ));
    }
    if *telemetry_freshness != FreshnessState::Fresh {
        return ModeTransitionAssessment::react(format!(
            "telemetry freshness is {telemetry_freshness:?}; OodaReAct requires fresh runtime telemetry and heartbeat"
        ));
    }
    if trust_posture.physical_execution_trust < TrustLevel::High {
        return ModeTransitionAssessment::react(format!(
            "physical execution trust is {:?}; OodaReAct requires High or better",
            trust_posture.physical_execution_trust
        ));
    }
    if trust_posture.environment_trust < TrustLevel::High {
        return ModeTransitionAssessment::react(format!(
            "environment trust is {:?}; OodaReAct requires High or better",
            trust_posture.environment_trust
        ));
    }

    ModeTransitionAssessment::ooda_ready()
}

pub struct LocalRuntime {
    manifest: ProjectManifest,
    project_dir: PathBuf,
    session_store: SessionStore,
    session_id: String,
    #[allow(dead_code, reason = "retained for cloud mode")]
    api_key: Option<String>,
    launcher: Arc<DockerLauncher>,
    mcp: Arc<McpManager>,
    model_factory: ModelFactory,
    effective_model_name: String,
    permission_mode: PermissionMode,
    evidence_archive: EvidenceArchive,
    evidence_persist_started: bool,
    evidence_persist_cancel: CancellationToken,
    /// Running Copper controller handle — spawned lazily when simulation is active.
    /// Must not be dropped while the controller is in use (drop sends Halt).
    copper_handle: Option<CopperHandle>,
    /// Session lifecycle runtime — the single source of truth for session state.
    /// All turns go through `SessionRuntime::run_turn` which delegates to a `TurnExecutor`.
    session_runtime: Arc<AsyncMutex<SessionRuntime>>,
    mode_state: std::sync::Mutex<LocalModeState>,
}

/// Extracted executor state that implements [`TurnExecutor`].
///
/// Holds everything needed to create an `AgentLoop` and run a single turn,
/// without borrowing `LocalRuntime` (which owns `SessionRuntime`).
///
/// Fields are wrapped in `Option` so they can be taken (moved) into the async
/// block without requiring placeholder values.
struct LocalTurnExecutor<'a> {
    model: Option<Box<dyn Model>>,
    dispatcher: Option<ToolDispatcher>,
    extensions: Option<roz_agent::dispatch::Extensions>,
    safety: Option<SafetyStack>,
    spatial: Option<Box<dyn WorldStateProvider>>,
    session_store: &'a SessionStore,
    session_id: &'a str,
}

impl TurnExecutor for LocalTurnExecutor<'_> {
    fn execute_turn(
        &mut self,
        prepared: roz_agent::session_runtime::PreparedTurn,
    ) -> roz_agent::session_runtime::TurnFuture<'_> {
        let prepared_agent_mode: AgentLoopMode = prepared.cognition_mode();
        let user_msg = prepared.user_message;
        debug_assert!(
            !prepared.system_blocks.is_empty(),
            "SessionRuntime should always provide system blocks"
        );
        let system_prompt: Vec<String> = prepared.system_blocks.into_iter().map(|b| b.content).collect();
        let history = prepared.history;
        Box::pin(async move {
            let model = self.model.take().expect("execute_turn called more than once");
            let dispatcher = self.dispatcher.take().expect("execute_turn called more than once");
            let safety = self.safety.take().expect("execute_turn called more than once");
            let spatial = self.spatial.take().expect("execute_turn called more than once");
            let extensions = self.extensions.take().expect("execute_turn called more than once");

            let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);
            let seed = AgentInputSeed::new(system_prompt, history, user_msg);
            let input = AgentInput::runtime_shell(
                uuid::Uuid::new_v4().to_string(),
                "local",
                "",
                prepared_agent_mode,
                10,
                4096,
                100_000,
                false,
                None,
                roz_core::safety::ControlMode::default(),
            );

            let output =
                agent
                    .run_seeded(input, seed)
                    .await
                    .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> {
                        Box::new(agent_error_to_turn_execution_failure(error))
                    })?;

            self.session_store
                .save(self.session_id, &output.messages)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

            // Extract assistant text for TurnOutput
            let assistant_message: String = output
                .messages
                .iter()
                .filter(|m| m.role == roz_agent::model::types::MessageRole::Assistant)
                .filter_map(roz_agent::model::types::Message::text)
                .collect();

            Ok(TurnOutput {
                assistant_message,
                tool_calls_made: output.cycles,
                input_tokens: u64::from(output.total_usage.input_tokens),
                output_tokens: u64::from(output.total_usage.output_tokens),
                cache_read_tokens: u64::from(output.total_usage.cache_read_tokens),
                cache_creation_tokens: u64::from(output.total_usage.cache_creation_tokens),
                messages: output.messages,
            })
        })
    }
}

struct LocalStreamingTurnExecutor {
    agent: Option<AgentLoop>,
    agent_input: AgentInput,
    project_dir: PathBuf,
    session_id: String,
    cancel: CancellationToken,
    output_slot: Arc<std::sync::Mutex<Option<AgentOutput>>>,
    stream_chunks_to: Option<mpsc::Sender<StreamChunk>>,
    stream_presence_to: Option<mpsc::Sender<PresenceSignal>>,
}

impl StreamingTurnExecutor for LocalStreamingTurnExecutor {
    fn execute_turn_streaming(
        &mut self,
        prepared: roz_agent::session_runtime::PreparedTurn,
    ) -> StreamingTurnHandle<'_> {
        debug_assert!(
            !prepared.system_blocks.is_empty(),
            "SessionRuntime should always provide system blocks"
        );
        let system_prompt: Vec<String> = prepared.system_blocks.into_iter().map(|block| block.content).collect();
        let seed = AgentInputSeed::new(system_prompt, prepared.history, prepared.user_message);

        let (bridge_chunk_tx, mut bridge_chunk_rx) = mpsc::channel::<StreamChunk>(64);
        let (bridge_presence_tx, mut bridge_presence_rx) = mpsc::channel::<PresenceSignal>(64);
        let (internal_chunk_tx, chunk_rx) = mpsc::channel::<StreamChunk>(64);
        let (internal_presence_tx, presence_rx) = mpsc::channel::<PresenceSignal>(64);
        let project_dir = self.project_dir.clone();
        let session_id = self.session_id.clone();
        let cancel = self.cancel.clone();
        let output_slot = self.output_slot.clone();
        let mut agent = self.agent.take().expect("streaming executor called more than once");
        let agent_input = self.agent_input.clone();
        let external_chunk_tx = self.stream_chunks_to.take().expect("streaming chunk sender missing");
        let external_presence_tx = self
            .stream_presence_to
            .take()
            .expect("streaming presence sender missing");

        tokio::spawn(async move {
            while let Some(chunk) = bridge_chunk_rx.recv().await {
                let _ = internal_chunk_tx.send(chunk.clone()).await;
                let _ = external_chunk_tx.send(chunk).await;
            }
        });
        tokio::spawn(async move {
            while let Some(signal) = bridge_presence_rx.recv().await {
                let _ = internal_presence_tx.send(signal.clone()).await;
                let _ = external_presence_tx.send(signal).await;
            }
        });

        StreamingTurnHandle {
            completion: Box::pin(async move {
                let result = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        Err(AgentError::Cancelled {
                            partial_input_tokens: 0,
                            partial_output_tokens: 0,
                        })
                    }
                    result = agent.run_streaming_seeded(agent_input, seed, bridge_chunk_tx, bridge_presence_tx) => result,
                }
                .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::new(agent_error_to_turn_execution_failure(error))
                })?;

                let store = SessionStore::new(&project_dir);
                let _ = store.save(&session_id, &result.messages);
                let assistant_message = result.final_response.clone().unwrap_or_default();
                *output_slot.lock().expect("output slot mutex poisoned") = Some(result.clone());

                Ok(TurnOutput {
                    assistant_message,
                    tool_calls_made: result.cycles,
                    input_tokens: u64::from(result.total_usage.input_tokens),
                    output_tokens: u64::from(result.total_usage.output_tokens),
                    cache_read_tokens: u64::from(result.total_usage.cache_read_tokens),
                    cache_creation_tokens: u64::from(result.total_usage.cache_creation_tokens),
                    messages: result.messages,
                })
            }),
            chunk_rx,
            presence_rx,
            tool_call_rx: None,
        }
    }
}

fn prompt_tool_schemas(dispatcher: &ToolDispatcher) -> Vec<roz_agent::prompt_assembler::ToolSchema> {
    dispatcher
        .schemas()
        .into_iter()
        .map(|schema| roz_agent::prompt_assembler::ToolSchema {
            name: schema.name,
            description: schema.description,
            parameters_json: serde_json::to_string(&schema.parameters).unwrap_or_else(|_| "{}".to_string()),
        })
        .collect()
}

fn spawn_evidence_archive_persister(
    state: Arc<arc_swap::ArcSwap<ControllerState>>,
    archive: EvidenceArchive,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut seen_bundle_ids = HashSet::new();
        let mut interval = tokio::time::interval(Duration::from_millis(250));

        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let current = state.load();
                    for bundle in [
                        current.last_live_evidence_bundle.as_ref(),
                        current.last_candidate_evidence_bundle.as_ref(),
                    ] {
                        let Some(bundle) = bundle else {
                            continue;
                        };
                        if !seen_bundle_ids.insert(bundle.bundle_id.clone()) {
                            continue;
                        }
                        if let Err(error) = archive.save(bundle) {
                            tracing::warn!(bundle_id = %bundle.bundle_id, %error, "failed to persist controller evidence bundle");
                        }
                    }
                }
            }
        }
    })
}

impl LocalRuntime {
    /// Create a new `LocalRuntime` rooted at `project_dir`.
    ///
    /// Loads `roz.toml`, creates a session store, and resolves an API key
    /// from env vars and project-local config. For global credential resolution
    /// (keyring, `~/.roz/credentials.toml`), use [`new_with_api_key`] instead.
    pub fn new(project_dir: &Path) -> Result<Self, RuntimeError> {
        Self::new_with_api_key(project_dir, None)
    }

    /// Create a new `LocalRuntime` with an optional pre-resolved API key.
    ///
    /// If `api_key_override` is `Some`, it takes priority over all other sources.
    /// Otherwise falls back to env vars → project-local `.roz/config.toml`.
    pub fn new_with_api_key(project_dir: &Path, api_key_override: Option<String>) -> Result<Self, RuntimeError> {
        let manifest = ProjectManifest::load(project_dir)?;
        let session_store = SessionStore::new(project_dir);
        let session_id = session_store.create().map_err(RuntimeError::Io)?;
        let api_key = api_key_override.or_else(|| load_api_key(project_dir));
        let launcher = Arc::new(DockerLauncher::new());
        let mcp = Arc::new(McpManager::new());

        // Resolve effective config from env vars → manifest
        let base_url = resolve_base_url(&manifest.model.base_url);
        let effective_model_name = resolve_model_name(&manifest.model.provider, &manifest.model.name, &base_url);

        let model_name = effective_model_name.clone();
        let key = api_key.clone();
        let blueprint_toml = load_blueprint_toml(project_dir);
        let evidence_archive = EvidenceArchive::new(project_dir);
        let model_factory: ModelFactory = Box::new(move || {
            create_model(
                &model_name,
                "",             // gateway_url: unused for direct access
                "",             // gateway api_key: unused for direct access
                120,            // timeout_secs
                "anthropic",    // proxy_provider
                key.as_deref(), // direct_api_key: hits provider API directly
            )
        });

        let session_config = SessionConfig {
            session_id: session_id.clone(),
            tenant_id: "local".to_string(),
            mode: SessionMode::Local,
            cognition_mode: roz_core::session::control::CognitionMode::React,
            constitution_text: String::new(),
            blueprint_toml,
            model_name: Some(effective_model_name.clone()),
            permissions: Vec::new(),
            tool_schemas: Vec::new(),
            project_context: Vec::new(),
            initial_history: Vec::new(),
        };
        let session_runtime = Arc::new(AsyncMutex::new(SessionRuntime::new(&session_config)));

        Ok(Self {
            manifest,
            project_dir: project_dir.to_path_buf(),
            session_store,
            session_id,
            api_key,
            launcher,
            mcp,
            model_factory,
            effective_model_name,
            permission_mode: resolve_permission_mode(),
            evidence_archive,
            evidence_persist_started: false,
            evidence_persist_cancel: CancellationToken::new(),
            copper_handle: None,
            session_runtime,
            mode_state: std::sync::Mutex::new(LocalModeState::default()),
        })
    }

    /// Create a runtime with a model factory (for testing).
    ///
    /// The factory is called once per `run_turn` to produce a fresh model.
    /// Create a runtime with a custom model factory.
    ///
    /// Primarily for testing — the factory is called once per `run_turn`.
    pub fn with_model_factory(
        project_dir: &Path,
        factory: impl Fn() -> Result<Box<dyn Model>, AgentError> + Send + Sync + 'static,
    ) -> Result<Self, RuntimeError> {
        let manifest = ProjectManifest::load(project_dir)?;
        let session_store = SessionStore::new(project_dir);
        let session_id = session_store.create().map_err(RuntimeError::Io)?;
        let api_key = load_api_key(project_dir);
        let launcher = Arc::new(DockerLauncher::new());
        let mcp = Arc::new(McpManager::new());
        let base_url = resolve_base_url(&manifest.model.base_url);
        let effective_model_name = resolve_model_name(&manifest.model.provider, &manifest.model.name, &base_url);
        let blueprint_toml = load_blueprint_toml(project_dir);
        let evidence_archive = EvidenceArchive::new(project_dir);

        let session_config = SessionConfig {
            session_id: session_id.clone(),
            tenant_id: "local".to_string(),
            mode: SessionMode::Local,
            cognition_mode: roz_core::session::control::CognitionMode::React,
            constitution_text: String::new(),
            blueprint_toml,
            model_name: Some(effective_model_name.clone()),
            permissions: Vec::new(),
            tool_schemas: Vec::new(),
            project_context: Vec::new(),
            initial_history: Vec::new(),
        };
        let session_runtime = Arc::new(AsyncMutex::new(SessionRuntime::new(&session_config)));

        Ok(Self {
            manifest,
            project_dir: project_dir.to_path_buf(),
            session_store,
            session_id,
            api_key,
            launcher,
            mcp,
            model_factory: Box::new(factory),
            effective_model_name,
            permission_mode: resolve_permission_mode(),
            evidence_archive,
            evidence_persist_started: false,
            evidence_persist_cancel: CancellationToken::new(),
            copper_handle: None,
            session_runtime,
            mode_state: std::sync::Mutex::new(LocalModeState::default()),
        })
    }

    /// The effective model name (with provider prefix), reflecting env var overrides.
    ///
    /// Examples: `claude-sonnet-4-6`, `ollama/llama3.1`, `openai-compat/gpt-4o`.
    pub fn model_name(&self) -> String {
        self.effective_model_name.clone()
    }

    /// The current session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the current permission mode.
    pub const fn permission_mode(&self) -> PermissionMode {
        self.permission_mode
    }

    /// Set the permission mode at runtime (e.g. via `/permissions` slash command).
    pub const fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.permission_mode = mode;
    }

    /// Synchronize the runtime-owned trust posture for subsequent turns.
    pub async fn sync_trust_posture(&self, trust: TrustPosture) {
        self.mode_state.lock().expect("mode state mutex poisoned").trust_posture = trust.clone();
        let mut runtime = self.session_runtime.lock().await;
        runtime.sync_trust_posture(trust);
    }

    /// Synchronize telemetry freshness for subsequent turns.
    pub async fn sync_telemetry_freshness(&self, freshness: FreshnessState) {
        self.mode_state
            .lock()
            .expect("mode state mutex poisoned")
            .telemetry_freshness = freshness.clone();
        let mut runtime = self.session_runtime.lock().await;
        runtime.sync_telemetry_freshness(freshness);
    }

    /// Build the safety stack, including the permission guard if mode is not Auto.
    fn build_safety_stack(&self) -> SafetyStack {
        let mut guards: Vec<Box<dyn roz_agent::safety::SafetyGuard>> = Vec::new();
        if self.permission_mode != PermissionMode::Auto {
            guards.push(Box::new(PermissionGuard::new(self.permission_mode)));
        }
        SafetyStack::new(guards)
    }

    /// The loaded project manifest.
    pub const fn manifest(&self) -> &ProjectManifest {
        &self.manifest
    }

    fn load_embodiment_manifest(&self) -> Option<roz_core::manifest::EmbodimentManifest> {
        match roz_core::manifest::EmbodimentManifest::load_from_project_dir(&self.project_dir) {
            Ok(manifest) => Some(manifest),
            Err(error)
                if roz_core::manifest::EmbodimentManifest::project_manifest_path(&self.project_dir).is_some() =>
            {
                tracing::warn!("failed to parse embodiment manifest: {error}");
                None
            }
            Err(_) => None,
        }
    }

    fn controller_estop_reason(&self) -> Option<String> {
        self.copper_handle
            .as_ref()
            .and_then(|handle| handle.state().load().estop_reason.clone())
    }

    fn assess_mode_transition(&self) -> ModeTransitionAssessment {
        let tools = self.mcp.all_tools();
        let state = self.mode_state.lock().expect("mode state mutex poisoned");
        assess_embodied_mode_readiness(
            self.mcp.has_connections(),
            self.load_embodiment_manifest().is_some(),
            tools.iter().any(|tool| matches!(tool.category, ToolCategory::Physical)),
            DockerSpatialProvider::supports_runtime_world_state(&tools),
            self.controller_estop_reason().as_deref(),
            &state.trust_posture,
            &state.telemetry_freshness,
        )
    }

    /// Determine the agent mode based on embodiment readiness, not mere MCP connectivity.
    fn current_mode(&self) -> AgentLoopMode {
        self.assess_mode_transition().mode
    }

    fn mode_transition_blocker(&self) -> Option<String> {
        self.assess_mode_transition().blocker
    }

    /// Attempt to create a spatial delegation model from environment variables.
    ///
    /// Checks `GEMINI_API_KEY` for direct Gemini access, or falls back to gateway
    /// config via `ROZ_GATEWAY_URL` + `ROZ_API_KEY`. Returns `None` if no spatial
    /// model can be configured (the `DelegationTool` simply won't be registered).
    ///
    /// Takes `&self` for future use with manifest-based spatial config (roz.toml).
    #[allow(clippy::unused_self)]
    fn create_spatial_model(&self) -> Option<Arc<dyn Model>> {
        // Direct Gemini access via GEMINI_API_KEY
        if let Ok(gemini_key) = std::env::var("GEMINI_API_KEY")
            && !gemini_key.is_empty()
        {
            let model = create_model(
                "gemini-2.5-flash",
                "",       // no gateway
                "",       // no gateway key
                120,      // timeout_secs
                "google", // proxy_provider
                Some(&gemini_key),
            );
            match model {
                Ok(m) => return Some(Arc::from(m)),
                Err(e) => {
                    tracing::warn!("failed to create spatial model from GEMINI_API_KEY: {e}");
                }
            }
        }

        // Gateway-based access (e.g. Pydantic AI Gateway)
        if let Ok(gateway_url) = std::env::var("ROZ_GATEWAY_URL")
            && !gateway_url.is_empty()
        {
            let gateway_key = std::env::var("ROZ_GATEWAY_KEY").unwrap_or_default();
            let model = create_model("gemini-2.5-flash", &gateway_url, &gateway_key, 120, "google", None);
            match model {
                Ok(m) => return Some(Arc::from(m)),
                Err(e) => {
                    tracing::warn!("failed to create spatial model from ROZ_GATEWAY_URL: {e}");
                }
            }
        }

        None
    }

    /// Build the spatial provider appropriate for the current mode.
    fn build_spatial_provider(&self) -> Box<dyn WorldStateProvider> {
        if matches!(self.current_mode(), AgentLoopMode::OodaReAct) {
            let mut provider = DockerSpatialProvider::new(self.mcp.clone());
            provider.auto_detect_telemetry_tool();
            Box::new(provider)
        } else {
            Box::new(NullWorldStateProvider)
        }
    }

    /// Whether a simulation environment is currently running.
    pub fn has_simulation(&self) -> bool {
        self.mcp.has_connections() && self.mode_transition_blocker().is_none()
    }

    /// Human-readable status of the simulation environment.
    pub fn simulation_status(&self) -> String {
        let instances = self.launcher.list();
        if instances.is_empty() {
            "No simulation running".into()
        } else {
            let tools = self.mcp.all_tools();
            let mut status = format!(
                "{} container(s) running, {} MCP tools available",
                instances.len(),
                tools.len(),
            );
            if let Some(blocker) = self.mode_transition_blocker() {
                let _ = write!(status, "; OodaReAct blocked: {blocker}");
            }
            status
        }
    }

    /// Connect to an MCP server at `port` on localhost, registering it under `container_id`.
    ///
    /// MCP tools become available immediately after connect, but `OodaReAct`
    /// only becomes available once embodiment readiness preconditions are met.
    pub async fn connect_mcp(&self, container_id: &str, port: u16, timeout: Duration) -> Result<(), McpError> {
        self.mcp.connect(container_id, port, timeout).await?;
        Ok(())
    }

    /// Current agent mode (React or `OodaReAct`).
    pub fn mode(&self) -> AgentLoopMode {
        self.current_mode()
    }

    /// Shut down all Docker containers and MCP connections.
    ///
    /// Call from the REPL's cleanup handler (e.g. on Ctrl-C or `exit`).
    pub fn shutdown(&self) {
        self.mcp.disconnect_all();
        self.launcher.stop_all();
    }

    /// Build the `ToolDispatcher`, `Extensions`, and system prompt for a turn.
    ///
    /// Lazily spawns the Copper controller when a simulation is connected.
    /// Shared by `run_turn` and `run_turn_streaming` to keep each method short.
    fn prepare_turn(&mut self, mode: AgentLoopMode) -> (ToolDispatcher, roz_agent::dispatch::Extensions, Vec<String>) {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
        dispatcher.register_with_category(
            Box::new(FileReadTool {
                project_dir: self.project_dir.clone(),
            }),
            ToolCategory::Pure,
        );
        if !matches!(mode, AgentLoopMode::OodaReAct) {
            dispatcher.register_with_category(
                Box::new(FileWriteTool {
                    project_dir: self.project_dir.clone(),
                }),
                ToolCategory::Physical,
            );
            dispatcher.register_with_category(Box::new(BashTool), ToolCategory::Physical);
            dispatcher.register_with_category(
                Box::new(EnvStartTool::new(
                    self.launcher.clone(),
                    self.mcp.clone(),
                    self.project_dir.clone(),
                )),
                ToolCategory::Physical,
            );
        }
        dispatcher.register_with_category(
            Box::new(EnvStopTool::new(self.launcher.clone(), self.mcp.clone())),
            ToolCategory::Physical,
        );
        for tool_info in self.mcp.all_tools() {
            let category = tool_info.category;
            dispatcher.register_with_category(Box::new(McpToolExecutor::new(self.mcp.clone(), tool_info)), category);
        }

        // Register spatial delegation tool if a spatial model is available
        if let Some(spatial_model) = self.create_spatial_model() {
            dispatcher.register_with_category(Box::new(DelegationTool::new(spatial_model)), ToolCategory::Pure);
        }

        // Load embodiment.toml (legacy robot.toml fallback accepted; daemon
        // tools do not require Copper)
        let robot_manifest = self.load_embodiment_manifest();
        let authoritative_embodiment_runtime = robot_manifest
            .as_ref()
            .and_then(roz_core::manifest::EmbodimentManifest::authoritative_embodiment_runtime);

        // Register daemon REST tools if [daemon] section present.
        // These register as Physical — the PermissionGuard in the safety stack
        // gates execution at call time (Ask → requires approval, Safe → blocked),
        // so no permission check is needed at registration time.
        if let Some(ref rm) = robot_manifest
            && let Some(ref daemon) = rm.daemon
        {
            let control_manifest = rm.control_interface_manifest();
            let embodiment_runtime = rm.authoritative_embodiment_runtime();
            for (tool, category) in
                crate::tools::daemon::daemon_tools(daemon, control_manifest.as_ref(), embodiment_runtime.as_ref())
            {
                dispatcher.register_with_category(tool, category);
            }
        }

        // Lazily spawn Copper controller when simulation is active
        if matches!(mode, AgentLoopMode::OodaReAct) && self.copper_handle.is_none() {
            self.copper_handle = Some(CopperHandle::spawn_execution_only(1.5));
        }
        if let Some(ref handle) = self.copper_handle
            && !self.evidence_persist_started
        {
            spawn_evidence_archive_persister(
                handle.state().clone(),
                self.evidence_archive.clone(),
                self.evidence_persist_cancel.clone(),
            );
            self.evidence_persist_started = true;
        }

        // Build Extensions with the evidence archive plus controller/context handles.
        let mut extensions = roz_agent::dispatch::Extensions::new();
        extensions.insert(self.evidence_archive.clone());
        if let Some(ref embodiment_runtime) = authoritative_embodiment_runtime {
            extensions.insert(embodiment_runtime.clone());
        }
        if let Some(ref rm) = robot_manifest
            && let Some(control_manifest) = rm.control_interface_manifest()
        {
            let replay_tool = crate::tools::replay_controller::ReplayControllerTool::new(&control_manifest);
            dispatcher.register_with_category(Box::new(replay_tool), ToolCategory::Pure);
            extensions.insert(control_manifest);

            if let Some(ref handle) = self.copper_handle {
                extensions.insert(handle.cmd_tx());
            }
        } else if let Some(ref handle) = self.copper_handle {
            extensions.insert(handle.cmd_tx());
        }

        // System prompt blocks
        let names = dispatcher.tool_names();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut system_prompt = vec![build_constitution(mode, &name_refs)];

        // Add robot system prompt if available
        if let Some(ref rm) = robot_manifest {
            system_prompt.push(rm.to_system_prompt());
        }

        // Add AGENTS.md
        let agents_md_path = self.project_dir.join("AGENTS.md");
        if let Ok(agents_content) = std::fs::read_to_string(&agents_md_path)
            && !agents_content.is_empty()
        {
            system_prompt.push(agents_content);
        }

        (dispatcher, extensions, system_prompt)
    }

    /// Run a single conversational turn.
    ///
    /// Delegates to [`SessionRuntime::run_turn`] which manages lifecycle (state
    /// checks, event emission, snapshot updates). The actual model invocation
    /// happens inside [`LocalTurnExecutor`].
    pub async fn run_turn(&mut self, user_message: &str) -> Result<AgentOutput, RuntimeError> {
        let model = (self.model_factory)().map_err(RuntimeError::Model)?;
        let assessment = self.assess_mode_transition();
        let mode = assessment.mode;
        let (dispatcher, extensions, system_prompt) = self.prepare_turn(mode);
        let tool_schemas = prompt_tool_schemas(&dispatcher);
        let safety = self.build_safety_stack();
        let spatial = self.build_spatial_provider();
        let (spatial, primed_spatial_context, world_state_note) = if matches!(mode, AgentLoopMode::OodaReAct) {
            let bootstrap = bootstrap_runtime_world_state_provider(spatial, &self.session_id).await;
            let world_state = bootstrap.world_state().cloned();
            let note = format_runtime_world_state_bootstrap_note(
                "local_mcp",
                world_state.as_ref(),
                "no bounded world-state data available from the connected embodiment environment",
            );
            (bootstrap.provider, world_state, Some(note))
        } else {
            (spatial, None, None)
        };
        if matches!(mode, AgentLoopMode::OodaReAct)
            && !primed_spatial_context
                .as_ref()
                .is_some_and(world_state_has_runtime_data)
        {
            let reason = assessment.blocker.unwrap_or_else(|| {
                "connected embodiment environment did not yield bounded world-state data".to_string()
            });
            let mut runtime = self.session_runtime.lock().await;
            runtime.sync_world_state_with_note(None, world_state_note);
            runtime.emit_activity_changed(
                roz_core::session::activity::RuntimeActivity::Degraded,
                reason.clone(),
                Some("world_state_ready".into()),
            );
            drop(runtime);
            return Err(RuntimeError::ModeTransitionBlocked(reason));
        }
        let spatial: Box<dyn WorldStateProvider> = if let Some(context) = primed_spatial_context.clone() {
            Box::new(PrimedWorldStateProvider::new(spatial, context))
        } else {
            spatial
        };
        let constitution = system_prompt.first().cloned().unwrap_or_default();

        // Extract project context for PromptAssembler (AGENTS.md,
        // embodiment.toml-derived prompt blocks, etc.)
        // These come from prepare_turn's system_prompt blocks (indices 1+).
        let project_context: Vec<String> = system_prompt.get(1..).unwrap_or_default().to_vec();

        let mut executor = LocalTurnExecutor {
            model: Some(model),
            dispatcher: Some(dispatcher),
            extensions: Some(extensions),
            safety: Some(safety),
            spatial: Some(spatial),
            session_store: &self.session_store,
            session_id: &self.session_id,
        };

        let turn_input = TurnInput {
            user_message: user_message.to_string(),
            cognition_mode: mode,
            custom_context: Vec::new(),
            volatile_blocks: Vec::new(),
        };

        let turn_output = {
            let mut runtime = self.session_runtime.lock().await;
            runtime.sync_cognition_mode(mode);
            runtime.sync_prompt_surface(constitution, tool_schemas, project_context);
            runtime.sync_world_state_with_note(primed_spatial_context, world_state_note);
            runtime
                .run_turn(turn_input, &mut executor)
                .await
                .map_err(|e| RuntimeError::Agent(e.to_string()))?
        };

        // Reconstruct an AgentOutput from the TurnOutput + updated history.
        // This preserves the existing public API for callers of run_turn.
        Ok(AgentOutput {
            cycles: turn_output.tool_calls_made,
            final_response: if turn_output.assistant_message.is_empty() {
                None
            } else {
                Some(turn_output.assistant_message)
            },
            total_usage: roz_agent::model::types::TokenUsage {
                #[allow(clippy::cast_possible_truncation)]
                input_tokens: turn_output.input_tokens as u32,
                #[allow(clippy::cast_possible_truncation)]
                output_tokens: turn_output.output_tokens as u32,
                ..Default::default()
            },
            messages: {
                let runtime = self.session_runtime.lock().await;
                runtime.history().to_vec()
            },
        })
    }

    /// The non-streaming `run_turn()` is unchanged — `exec` mode continues to use it.
    #[allow(clippy::too_many_lines)]
    pub async fn run_turn_streaming(&mut self, user_message: &str) -> Result<TurnHandle, RuntimeError> {
        let model = (self.model_factory)().map_err(RuntimeError::Model)?;
        let assessment = self.assess_mode_transition();
        let mode = assessment.mode;
        let (dispatcher, extensions, prepare_prompt) = self.prepare_turn(mode);
        let tool_schemas = prompt_tool_schemas(&dispatcher);
        let safety = self.build_safety_stack();
        let spatial = self.build_spatial_provider();
        let (spatial, primed_spatial_context, world_state_note) = if matches!(mode, AgentLoopMode::OodaReAct) {
            let bootstrap = bootstrap_runtime_world_state_provider(spatial, &self.session_id).await;
            let world_state = bootstrap.world_state().cloned();
            let note = format_runtime_world_state_bootstrap_note(
                "local_mcp",
                world_state.as_ref(),
                "no bounded world-state data available from the connected embodiment environment",
            );
            (bootstrap.provider, world_state, Some(note))
        } else {
            (spatial, None, None)
        };
        if matches!(mode, AgentLoopMode::OodaReAct)
            && !primed_spatial_context
                .as_ref()
                .is_some_and(world_state_has_runtime_data)
        {
            let reason = assessment.blocker.unwrap_or_else(|| {
                "connected embodiment environment did not yield bounded world-state data".to_string()
            });
            let mut runtime = self.session_runtime.lock().await;
            runtime.sync_world_state_with_note(None, world_state_note);
            runtime.emit_activity_changed(
                roz_core::session::activity::RuntimeActivity::Degraded,
                reason.clone(),
                Some("world_state_ready".into()),
            );
            drop(runtime);
            return Err(RuntimeError::ModeTransitionBlocked(reason));
        }
        let spatial: Box<dyn WorldStateProvider> = if let Some(context) = primed_spatial_context.clone() {
            Box::new(PrimedWorldStateProvider::new(spatial, context))
        } else {
            spatial
        };
        let constitution = prepare_prompt.first().cloned().unwrap_or_default();

        let project_context: Vec<String> = prepare_prompt.get(1..).unwrap_or_default().to_vec();
        let turn_input = TurnInput {
            user_message: user_message.to_string(),
            cognition_mode: mode,
            custom_context: Vec::new(),
            volatile_blocks: Vec::new(),
        };

        let approval_runtime = {
            let runtime = self.session_runtime.lock().await;
            runtime.approval_handle()
        };
        let agent = AgentLoop::new(model, dispatcher, safety, spatial)
            .with_extensions(extensions)
            .with_approval_runtime(approval_runtime.clone());

        let cancel = CancellationToken::new();
        let input = AgentInput::runtime_shell(
            uuid::Uuid::new_v4().to_string(),
            "local",
            "",
            mode,
            10,
            4096,
            100_000,
            true,
            Some(cancel.clone()),
            roz_core::safety::ControlMode::default(),
        );

        let (chunk_tx, chunk_rx) = mpsc::channel::<StreamChunk>(64);
        let (presence_tx, presence_rx) = mpsc::channel::<PresenceSignal>(64);
        let output_slot = Arc::new(std::sync::Mutex::new(None));
        let mut executor = LocalStreamingTurnExecutor {
            agent: Some(agent),
            agent_input: input,
            project_dir: self.project_dir.clone(),
            session_id: self.session_id.clone(),
            cancel: cancel.clone(),
            output_slot,
            stream_chunks_to: Some(chunk_tx),
            stream_presence_to: Some(presence_tx),
        };
        let session_runtime = self.session_runtime.clone();
        let join_cancel = cancel.clone();

        let join = tokio::spawn(async move {
            let result = {
                let mut runtime = session_runtime.lock().await;
                runtime.sync_cognition_mode(mode);
                runtime.sync_prompt_surface(constitution, tool_schemas, project_context);
                runtime.sync_world_state_with_note(primed_spatial_context, world_state_note);
                runtime.run_turn_streaming(turn_input, None, &mut executor).await
            };
            join_cancel.cancel();

            match result {
                Ok(RuntimeStreamingTurnResult::Completed(summary)) => {
                    let output = executor
                        .output_slot
                        .lock()
                        .expect("output slot mutex poisoned")
                        .take()
                        .ok_or_else(|| RuntimeError::Agent("streaming turn completed without output".to_string()))?;
                    Ok(StreamingTurnResult { output, summary })
                }
                Ok(RuntimeStreamingTurnResult::Cancelled) => Err(RuntimeError::Agent("cancelled by user".to_string())),
                Err(error) => Err(RuntimeError::Agent(error.to_string())),
            }
        });

        Ok(TurnHandle {
            chunks: chunk_rx,
            presence: presence_rx,
            approval_runtime,
            cancel,
            join,
        })
    }

    /// Await a streaming turn handle and apply the runtime completion state.
    pub async fn finish_turn_streaming(&mut self, handle: TurnHandle) -> Result<AgentOutput, RuntimeError> {
        handle.finish().await
    }
}

impl Drop for LocalRuntime {
    fn drop(&mut self) {
        self.evidence_persist_cancel.cancel();
    }
}

/// Handle returned by [`LocalRuntime::run_turn_streaming`].
///
/// Drain [`chunks`](Self::chunks) for real-time display, then call [`finish`](Self::finish)
/// to await completion and retrieve the final output.
pub struct TurnHandle {
    /// Streaming chunks (text deltas, tool use events, etc.).
    pub chunks: mpsc::Receiver<StreamChunk>,
    /// Presence signals (activity state changes).
    pub presence: mpsc::Receiver<PresenceSignal>,
    /// Runtime-owned safety approvals for this turn.
    pub approval_runtime: roz_agent::session_runtime::ApprovalRuntimeHandle,
    /// Cancellation token — call `cancel()` to abort the agent turn.
    pub cancel: CancellationToken,
    join: JoinHandle<Result<StreamingTurnResult, RuntimeError>>,
}

struct StreamingTurnResult {
    output: AgentOutput,
    #[allow(dead_code)]
    summary: TurnOutput,
}

impl TurnHandle {
    /// Await completion of the agent turn and return the final output.
    pub async fn finish(self) -> Result<AgentOutput, RuntimeError> {
        Ok(self.finish_with_summary().await?.output)
    }

    async fn finish_with_summary(self) -> Result<StreamingTurnResult, RuntimeError> {
        self.join
            .await
            .map_err(|e| RuntimeError::Agent(format!("agent task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use chrono::Utc;
    use roz_copper::channels::{ControllerState, EvidenceSummaryState};
    use roz_core::controller::artifact::ExecutionMode;
    use roz_core::controller::evidence::{ControllerEvidenceBundle, StabilitySummary};
    use roz_core::session::snapshot::FreshnessState;

    fn sample_evidence_bundle(bundle_id: &str) -> ControllerEvidenceBundle {
        ControllerEvidenceBundle {
            bundle_id: bundle_id.into(),
            controller_id: "ctrl-live".into(),
            ticks_run: 8,
            rejection_count: 0,
            limit_clamp_count: 1,
            rate_clamp_count: 0,
            position_limit_stop_count: 0,
            epoch_interrupt_count: 0,
            trap_count: 0,
            watchdog_near_miss_count: 0,
            channels_touched: vec!["joint_0".into()],
            channels_untouched: vec![],
            config_reads: 1,
            tick_latency_p50: 100.into(),
            tick_latency_p95: 150.into(),
            tick_latency_p99: 200.into(),
            controller_stability_summary: StabilitySummary {
                command_oscillation_detected: false,
                idle_output_stable: true,
                runtime_jitter_us: 3.0,
                missed_tick_count: 0,
                steady_state_reached: true,
            },
            verifier_status: "pass".into(),
            verifier_reason: None,
            controller_digest: "ctrl".into(),
            model_digest: "model".into(),
            calibration_digest: "cal".into(),
            frame_snapshot_id: 1,
            manifest_digest: "manifest".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            execution_mode: ExecutionMode::Live,
            compiler_version: "wasmtime".into(),
            created_at: Utc::now(),
            state_freshness: FreshnessState::Fresh,
        }
    }

    fn high_trust() -> TrustPosture {
        TrustPosture {
            workspace_trust: TrustLevel::High,
            host_trust: TrustLevel::High,
            environment_trust: TrustLevel::High,
            tool_trust: TrustLevel::High,
            physical_execution_trust: TrustLevel::High,
            controller_artifact_trust: TrustLevel::High,
            edge_transport_trust: TrustLevel::High,
        }
    }

    #[test]
    fn model_name_ollama() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("roz.toml"),
            r#"
[project]
name = "test"
[model]
provider = "ollama"
name = "llama3.1"
"#,
        )
        .unwrap();

        let rt = LocalRuntime::new(dir.path()).unwrap();
        assert_eq!(rt.model_name(), "ollama/llama3.1");
    }

    #[test]
    fn model_name_anthropic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("roz.toml"),
            r#"
[project]
name = "test"
[model]
provider = "anthropic"
name = "claude-sonnet-4-5"
"#,
        )
        .unwrap();

        let rt = LocalRuntime::new(dir.path()).unwrap();
        // claude-* already matches create_local_model routing
        assert_eq!(rt.model_name(), "claude-sonnet-4-5");
    }

    #[test]
    fn model_name_openai_compat() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("roz.toml"),
            r#"
[project]
name = "test"
[model]
provider = "openai-compat"
name = "gpt-4o"
"#,
        )
        .unwrap();

        let rt = LocalRuntime::new(dir.path()).unwrap();
        assert_eq!(rt.model_name(), "openai-compat/gpt-4o");
    }

    #[test]
    fn api_key_loaded_from_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("roz.toml"),
            r#"
[project]
name = "test"
[model]
provider = "ollama"
name = "llama3.1"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".roz")).unwrap();
        std::fs::write(dir.path().join(".roz/config.toml"), "api_key = \"sk-ant-test123\"\n").unwrap();

        let rt = LocalRuntime::new(dir.path()).unwrap();
        assert_eq!(rt.api_key.as_deref(), Some("sk-ant-test123"));
    }

    #[test]
    fn missing_config_gives_no_api_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("roz.toml"),
            r#"
[project]
name = "test"
[model]
provider = "ollama"
name = "llama3.1"
"#,
        )
        .unwrap();

        let rt = LocalRuntime::new(dir.path()).unwrap();
        assert!(rt.api_key.is_none());
    }

    #[test]
    fn default_permission_mode_prefers_ask_for_interactive_sessions() {
        assert_eq!(default_permission_mode(true), PermissionMode::Ask);
        assert_eq!(default_permission_mode(false), PermissionMode::Auto);
    }

    #[test]
    fn resolve_permission_mode_respects_explicit_env_override() {
        assert_eq!(
            resolve_permission_mode_from_env(Some("safe"), true),
            PermissionMode::Safe
        );
        assert_eq!(
            resolve_permission_mode_from_env(Some("ask"), false),
            PermissionMode::Ask
        );
        assert_eq!(resolve_permission_mode_from_env(None, false), PermissionMode::Auto);
    }

    #[test]
    fn session_id_is_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("roz.toml"),
            r#"
[project]
name = "test"
[model]
provider = "ollama"
name = "llama3.1"
"#,
        )
        .unwrap();

        let rt = LocalRuntime::new(dir.path()).unwrap();
        assert!(!rt.session_id().is_empty());
    }

    #[tokio::test]
    async fn evidence_persister_writes_finalized_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let archive = EvidenceArchive::new(dir.path());
        let bundle = sample_evidence_bundle("ev-live-001");
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            running: false,
            last_tick: 0,
            last_output: None,
            entities: vec![],
            estop_reason: None,
            deployment_state: None,
            active_controller_id: None,
            candidate_controller_id: None,
            last_known_good_controller_id: None,
            promotion_requested: false,
            candidate_stage_ticks_completed: 0,
            candidate_stage_ticks_required: 0,
            candidate_last_max_abs_delta: None,
            candidate_last_normalized_delta: None,
            candidate_canary_bounded: false,
            candidate_last_rejection_reason: None,
            last_live_evidence: Some(EvidenceSummaryState::from(&bundle)),
            last_live_evidence_bundle: Some(bundle.clone()),
            last_candidate_evidence: None,
            last_candidate_evidence_bundle: None,
        }));
        let cancel = CancellationToken::new();
        let task = spawn_evidence_archive_persister(state, archive.clone(), cancel.clone());

        for _ in 0..10 {
            if archive.path_for(&bundle.bundle_id).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        cancel.cancel();
        let _ = task.await;

        let loaded = archive.load(&bundle.bundle_id).unwrap();
        assert_eq!(loaded.bundle_id, bundle.bundle_id);
        assert_eq!(loaded.controller_id, bundle.controller_id);
    }

    #[test]
    fn mode_assessment_requires_embodiment_backing() {
        let assessment =
            assess_embodied_mode_readiness(true, false, true, true, None, &high_trust(), &FreshnessState::Fresh);
        assert_eq!(assessment.mode, AgentLoopMode::React);
        assert!(
            assessment
                .blocker
                .as_deref()
                .is_some_and(|reason| reason.contains("embodiment.toml")),
            "expected embodiment-backing blocker, got: {:?}",
            assessment.blocker
        );
    }

    #[test]
    fn mode_assessment_requires_trust_and_freshness() {
        let blocked = assess_embodied_mode_readiness(
            true,
            true,
            true,
            true,
            None,
            &TrustPosture::default(),
            &FreshnessState::Unknown,
        );
        assert_eq!(blocked.mode, AgentLoopMode::React);

        let ready = assess_embodied_mode_readiness(true, true, true, true, None, &high_trust(), &FreshnessState::Fresh);
        assert_eq!(ready.mode, AgentLoopMode::OodaReAct);
    }

    #[test]
    fn mode_assessment_requires_estop_clear() {
        let blocked = assess_embodied_mode_readiness(
            true,
            true,
            true,
            true,
            Some("watchdog tripped"),
            &high_trust(),
            &FreshnessState::Fresh,
        );
        assert_eq!(blocked.mode, AgentLoopMode::React);
        assert!(
            blocked
                .blocker
                .as_deref()
                .is_some_and(|reason| reason.contains("e-stop") || reason.contains("safety interlock")),
            "expected estop blocker, got: {:?}",
            blocked.blocker
        );
    }
}
