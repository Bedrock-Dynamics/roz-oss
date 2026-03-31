use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode, AgentOutput, PresenceSignal};
use roz_agent::constitution::build_constitution;
use roz_agent::delegation::DelegationTool;
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::dispatch::remote::PendingApprovals;
use roz_agent::error::AgentError;
use roz_agent::model::create_model;
use roz_agent::model::types::{Message, Model, StreamChunk};
use roz_agent::safety::stack::SafetyStack;
use roz_agent::spatial_provider::{NullSpatialContextProvider, SpatialContextProvider};
use roz_copper::handle::CopperHandle;
use roz_core::tools::ToolCategory;
use tokio::sync::mpsc;
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
        _state: &roz_core::spatial::SpatialContext,
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
    #[error("agent error: {0}")]
    Agent(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
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
fn resolve_permission_mode() -> PermissionMode {
    match std::env::var("ROZ_PERMISSION_MODE").ok().as_deref() {
        Some("ask") => PermissionMode::Ask,
        Some("safe") => PermissionMode::Safe,
        _ => PermissionMode::Auto,
    }
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

// ---------------------------------------------------------------------------
// LocalRuntime
// ---------------------------------------------------------------------------

/// A factory that creates a `Box<dyn Model>` on each call.
///
/// Used to decouple model creation from the runtime, enabling both
/// config-based creation (production) and injected mocks (testing).
type ModelFactory = Box<dyn Fn() -> Result<Box<dyn Model>, AgentError> + Send + Sync>;

pub struct LocalRuntime {
    manifest: ProjectManifest,
    project_dir: PathBuf,
    session_store: SessionStore,
    session_id: String,
    history: Vec<Message>,
    #[allow(dead_code, reason = "retained for cloud mode")]
    api_key: Option<String>,
    launcher: Arc<DockerLauncher>,
    mcp: Arc<McpManager>,
    model_factory: ModelFactory,
    effective_model_name: String,
    permission_mode: PermissionMode,
    /// Running Copper controller handle — spawned lazily when simulation is active.
    /// Must not be dropped while the controller is in use (drop sends Halt).
    copper_handle: Option<CopperHandle>,
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

        Ok(Self {
            manifest,
            project_dir: project_dir.to_path_buf(),
            session_store,
            session_id,
            history: Vec::new(),
            api_key,
            launcher,
            mcp,
            model_factory,
            effective_model_name,
            permission_mode: resolve_permission_mode(),
            copper_handle: None,
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

        Ok(Self {
            manifest,
            project_dir: project_dir.to_path_buf(),
            session_store,
            session_id,
            history: Vec::new(),
            api_key,
            launcher,
            mcp,
            model_factory: Box::new(factory),
            effective_model_name,
            permission_mode: resolve_permission_mode(),
            copper_handle: None,
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

    /// Determine the agent mode based on simulation state.
    fn current_mode(&self) -> AgentLoopMode {
        if self.mcp.has_connections() {
            AgentLoopMode::OodaReAct
        } else {
            AgentLoopMode::React
        }
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
    fn build_spatial_provider(&self) -> Box<dyn SpatialContextProvider> {
        if self.mcp.has_connections() {
            let mut provider = DockerSpatialProvider::new(self.mcp.clone());
            provider.auto_detect_telemetry_tool();
            Box::new(provider)
        } else {
            Box::new(NullSpatialContextProvider)
        }
    }

    /// Whether a simulation environment is currently running.
    pub fn has_simulation(&self) -> bool {
        self.mcp.has_connections()
    }

    /// Human-readable status of the simulation environment.
    pub fn simulation_status(&self) -> String {
        let instances = self.launcher.list();
        if instances.is_empty() {
            "No simulation running".into()
        } else {
            let tools = self.mcp.all_tools();
            format!(
                "{} container(s) running, {} MCP tools available",
                instances.len(),
                tools.len(),
            )
        }
    }

    /// Connect to an MCP server at `port` on localhost, registering it under `container_id`.
    ///
    /// After a successful connect, `mode()` returns `OodaReAct` and the next
    /// `run_turn()` will register MCP tools and lazily spawn `CopperHandle`.
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

        // Lazily spawn Copper controller when simulation is active
        if self.mcp.has_connections() && self.copper_handle.is_none() {
            self.copper_handle = Some(CopperHandle::spawn(1.5));
        }

        // Register deploy_controller tool when controller is available
        if self.copper_handle.is_some() {
            dispatcher.register_with_category(
                Box::new(crate::tools::deploy_controller::DeployControllerTool),
                ToolCategory::Physical,
            );
        }

        // Build Extensions with cmd_tx + manifest if controller is running
        let mut extensions = roz_agent::dispatch::Extensions::new();
        if let Some(ref handle) = self.copper_handle {
            extensions.insert(handle.cmd_tx());
            // Load channel manifest from robot.toml if present in project directory.
            let robot_toml_path = self.project_dir.join("robot.toml");
            if let Ok(robot_manifest) = roz_core::manifest::RobotManifest::load(&robot_toml_path) {
                if let Some(channel_manifest) = robot_manifest.channel_manifest() {
                    extensions.insert(channel_manifest);
                }
            } else {
                // No robot.toml — deploy_controller will error if called without a manifest.
                tracing::debug!("no robot.toml found, skipping channel manifest injection");
            }
        }

        // System prompt blocks
        let names = dispatcher.tool_names();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut system_prompt = vec![build_constitution(mode, &name_refs)];
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
    /// Creates the model, registers tools, builds the system prompt,
    /// runs the `AgentLoop`, and persists the resulting messages.
    pub async fn run_turn(&mut self, user_message: &str) -> Result<AgentOutput, RuntimeError> {
        let model = (self.model_factory)().map_err(RuntimeError::Model)?;
        let mode = self.current_mode();
        let (dispatcher, extensions, system_prompt) = self.prepare_turn(mode);
        let safety = self.build_safety_stack();
        let spatial = self.build_spatial_provider();

        let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);

        let input = AgentInput {
            task_id: uuid::Uuid::new_v4().to_string(),
            tenant_id: "local".to_string(),
            model_name: String::new(),
            system_prompt,
            user_message: user_message.to_string(),
            max_cycles: 10,
            max_tokens: 4096,
            max_context_tokens: 100_000,
            mode,
            phases: vec![],
            tool_choice: None,
            response_schema: None,
            streaming: false,
            history: self.history.clone(),
            cancellation_token: None,
            control_mode: roz_core::safety::ControlMode::default(),
        };

        let output = agent.run(input).await.map_err(RuntimeError::Model)?;
        self.history.clone_from(&output.messages);
        self.session_store
            .save(&self.session_id, &self.history)
            .map_err(RuntimeError::Io)?;

        Ok(output)
    }

    /// Run a single conversational turn with streaming output.
    ///
    /// Returns a [`TurnHandle`] whose `chunks` receiver yields [`StreamChunk`]s
    /// in real time. Call [`TurnHandle::finish`] after draining chunks to persist
    /// history and retrieve the final [`AgentOutput`].
    ///
    /// The non-streaming `run_turn()` is unchanged — `exec` mode continues to use it.
    pub fn run_turn_streaming(&mut self, user_message: &str) -> Result<TurnHandle, RuntimeError> {
        let model = (self.model_factory)().map_err(RuntimeError::Model)?;
        let mode = self.current_mode();
        let (dispatcher, extensions, system_prompt) = self.prepare_turn(mode);
        let safety = self.build_safety_stack();
        let spatial = self.build_spatial_provider();

        let pending_approvals: PendingApprovals = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let mut agent = AgentLoop::new(model, dispatcher, safety, spatial)
            .with_extensions(extensions)
            .with_pending_approvals(pending_approvals.clone());

        let input = AgentInput {
            task_id: uuid::Uuid::new_v4().to_string(),
            tenant_id: "local".to_string(),
            model_name: String::new(),
            system_prompt,
            user_message: user_message.to_string(),
            max_cycles: 10,
            max_tokens: 4096,
            max_context_tokens: 100_000,
            mode,
            phases: vec![],
            tool_choice: None,
            response_schema: None,
            streaming: true,
            history: self.history.clone(),
            cancellation_token: None,
            control_mode: roz_core::safety::ControlMode::default(),
        };

        let (chunk_tx, chunk_rx) = mpsc::channel::<StreamChunk>(64);
        let (presence_tx, presence_rx) = mpsc::channel::<PresenceSignal>(64);

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let project_dir = self.project_dir.clone();
        let session_id = self.session_id.clone();
        let join = tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = cancel_clone.cancelled() => {
                    Err(RuntimeError::Agent("cancelled by user".to_string()))
                }
                result = agent.run_streaming(input, chunk_tx, presence_tx) => {
                    result.map_err(RuntimeError::Model)
                }
            };
            if let Ok(ref output) = result {
                let store = SessionStore::new(&project_dir);
                let _ = store.save(&session_id, &output.messages);
            }
            result
        });

        Ok(TurnHandle {
            chunks: chunk_rx,
            presence: presence_rx,
            pending_approvals,
            cancel,
            join,
        })
    }

    /// Update history from a completed streaming turn.
    pub fn update_history(&mut self, messages: &[Message]) {
        self.history.clone_from(&messages.to_vec());
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
    /// Pending safety approvals — resolve via [`roz_agent::dispatch::remote::resolve_approval`].
    pub pending_approvals: PendingApprovals,
    /// Cancellation token — call `cancel()` to abort the agent turn.
    pub cancel: CancellationToken,
    join: JoinHandle<Result<AgentOutput, RuntimeError>>,
}

impl TurnHandle {
    /// Await completion of the agent turn and return the final output.
    pub async fn finish(self) -> Result<AgentOutput, RuntimeError> {
        self.join
            .await
            .map_err(|e| RuntimeError::Agent(format!("agent task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
