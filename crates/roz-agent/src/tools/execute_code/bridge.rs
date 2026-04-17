use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use roz_core::auth::AuthIdentity;
use roz_core::tools::{ToolCall, ToolCategory, ToolResult};
use serde_json::Value;

use crate::agent_loop::{ApprovalGateResult, PresenceSignal, gate_tool_call_for_human_approval};
use crate::dispatch::{ToolContext, ToolDispatcher};
use crate::tools::execute_code::{
    DEFAULT_TIMEOUT_SECS, ExecuteCodeStatus, MAX_TOOL_CALLS, STDERR_LIMIT_BYTES, STDOUT_LIMIT_BYTES,
};

#[derive(Debug, Clone)]
pub struct SandboxOutcome {
    pub status: ExecuteCodeStatus,
    pub output: String,
    pub error: Option<String>,
    pub tool_calls_made: u32,
    pub duration: Duration,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct SandboxExecutionConfig {
    pub max_tool_calls: u32,
    pub stdout_limit_bytes: usize,
    pub stderr_limit_bytes: usize,
    pub timeout: Duration,
}

impl Default for SandboxExecutionConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: MAX_TOOL_CALLS,
            stdout_limit_bytes: STDOUT_LIMIT_BYTES,
            stderr_limit_bytes: STDERR_LIMIT_BYTES,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxError {
    message: String,
}

impl SandboxError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SandboxError {}

#[derive(Clone)]
pub struct SandboxBridge {
    inner: Arc<SandboxBridgeInner>,
}

impl std::panic::RefUnwindSafe for SandboxBridge {}
impl std::panic::UnwindSafe for SandboxBridge {}

struct SandboxBridgeInner {
    dispatcher: Arc<ToolDispatcher>,
    base_ctx: ToolContext,
    caller_identity: AuthIdentity,
    runtime_handle: tokio::runtime::Handle,
    config: SandboxExecutionConfig,
    started_at: Instant,
    stdout: Mutex<String>,
    stderr: Mutex<String>,
    stdout_truncated: AtomicBool,
    stderr_truncated: AtomicBool,
    tool_calls_made: AtomicU32,
}

impl SandboxBridge {
    pub fn new(
        dispatcher: Arc<ToolDispatcher>,
        base_ctx: ToolContext,
        runtime_handle: tokio::runtime::Handle,
        config: SandboxExecutionConfig,
    ) -> Result<Self, SandboxError> {
        let caller_identity = base_ctx
            .extensions
            .get::<AuthIdentity>()
            .cloned()
            .ok_or_else(|| SandboxError::new("execute_code: AuthIdentity extension missing"))?;

        Ok(Self {
            inner: Arc::new(SandboxBridgeInner {
                dispatcher,
                base_ctx,
                caller_identity,
                runtime_handle,
                config,
                started_at: Instant::now(),
                stdout: Mutex::new(String::new()),
                stderr: Mutex::new(String::new()),
                stdout_truncated: AtomicBool::new(false),
                stderr_truncated: AtomicBool::new(false),
                tool_calls_made: AtomicU32::new(0),
            }),
        })
    }

    pub fn print(&self, text: impl AsRef<str>) {
        push_capped(
            &self.inner.stdout,
            text.as_ref(),
            self.inner.config.stdout_limit_bytes,
            &self.inner.stdout_truncated,
        );
    }

    pub fn write_stderr(&self, text: impl AsRef<str>) {
        push_capped(
            &self.inner.stderr,
            text.as_ref(),
            self.inner.config.stderr_limit_bytes,
            &self.inner.stderr_truncated,
        );
    }

    pub fn call_tool_json(&self, tool_name: &str, params: Value) -> Result<Value, SandboxError> {
        self.ensure_not_timed_out()?;

        let count = self.inner.tool_calls_made.fetch_add(1, Ordering::Relaxed) + 1;
        if count > self.inner.config.max_tool_calls {
            return Err(SandboxError::new(format!(
                "execute_code tool-call limit exceeded ({})",
                self.inner.config.max_tool_calls
            )));
        }

        let category = self.inner.dispatcher.category(tool_name);
        if category == ToolCategory::CodeSandbox {
            return Err(SandboxError::new(format!(
                "execute_code cannot recursively call code-sandbox tools: {tool_name}"
            )));
        }

        let call = ToolCall {
            id: self.nested_call_id(tool_name, count),
            tool: tool_name.to_string(),
            params,
        };
        let nested_ctx = self.nested_tool_context(&call.id);
        let result = match category {
            ToolCategory::Pure => self
                .inner
                .runtime_handle
                .block_on(self.inner.dispatcher.dispatch(&call, &nested_ctx)),
            ToolCategory::Physical => self.dispatch_physical_tool_with_approval(&call, &nested_ctx)?,
            ToolCategory::CodeSandbox => unreachable!("code-sandbox tools are rejected above"),
        };

        if let Some(error) = result.error {
            Err(SandboxError::new(error))
        } else {
            Ok(result.output)
        }
    }

    pub fn success_outcome(&self) -> SandboxOutcome {
        self.outcome(ExecuteCodeStatus::Success, None)
    }

    pub fn error_outcome(&self, error: impl Into<String>) -> SandboxOutcome {
        let error = error.into();
        self.outcome(ExecuteCodeStatus::Error, Some(error))
    }

    pub fn timeout_outcome(&self, error: impl Into<String>) -> SandboxOutcome {
        let error = error.into();
        self.outcome(ExecuteCodeStatus::Timeout, Some(error))
    }

    pub fn tool_calls_made(&self) -> u32 {
        self.inner.tool_calls_made.load(Ordering::Relaxed)
    }

    fn ensure_not_timed_out(&self) -> Result<(), SandboxError> {
        if self.inner.started_at.elapsed() > self.inner.config.timeout {
            Err(SandboxError::new(format!(
                "execute_code timed out after {}s",
                self.inner.config.timeout.as_secs()
            )))
        } else {
            Ok(())
        }
    }

    fn nested_call_id(&self, tool_name: &str, call_index: u32) -> String {
        let parent = if self.inner.base_ctx.call_id.is_empty() {
            "execute_code".to_string()
        } else {
            self.inner.base_ctx.call_id.clone()
        };
        format!("{parent}::nested::{call_index}::{tool_name}")
    }

    fn nested_tool_context(&self, call_id: &str) -> ToolContext {
        let mut extensions = self.inner.base_ctx.extensions.clone();
        extensions.insert(self.inner.caller_identity.clone());
        ToolContext {
            task_id: self.inner.base_ctx.task_id.clone(),
            tenant_id: self.inner.base_ctx.tenant_id.clone(),
            call_id: call_id.to_string(),
            extensions,
        }
    }

    fn dispatch_physical_tool_with_approval(
        &self,
        call: &ToolCall,
        nested_ctx: &ToolContext,
    ) -> Result<ToolResult, SandboxError> {
        let approval_runtime = nested_ctx
            .extensions
            .get::<crate::session_runtime::ApprovalRuntimeHandle>()
            .cloned()
            .ok_or_else(|| SandboxError::new("execute_code: ApprovalRuntimeHandle extension missing"))?;
        let presence_tx = nested_ctx
            .extensions
            .get::<tokio::sync::mpsc::Sender<PresenceSignal>>()
            .cloned()
            .ok_or_else(|| SandboxError::new("execute_code: PresenceSignal sender missing from ToolContext"))?;
        let cancellation_token = nested_ctx
            .extensions
            .get::<tokio_util::sync::CancellationToken>()
            .cloned();

        let gated = self.inner.runtime_handle.block_on(gate_tool_call_for_human_approval(
            call,
            "execute_code nested physical tool requires approval",
            self.inner.config.timeout.as_secs(),
            &approval_runtime,
            &presence_tx,
            &nested_ctx.task_id,
            cancellation_token.as_ref(),
        ));

        match gated {
            ApprovalGateResult::Approved(effective_call) => Ok(self
                .inner
                .runtime_handle
                .block_on(self.inner.dispatcher.dispatch(&effective_call, nested_ctx))),
            ApprovalGateResult::Rejected(result) => Ok(result),
        }
    }

    fn outcome(&self, status: ExecuteCodeStatus, error: Option<String>) -> SandboxOutcome {
        let mut output = self.inner.stdout.lock().clone();
        let stderr = self.inner.stderr.lock().clone();
        if !stderr.is_empty() && status != ExecuteCodeStatus::Success {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&stderr);
        }
        if output.is_empty()
            && status != ExecuteCodeStatus::Success
            && let Some(error) = error.as_ref()
        {
            output = error.clone();
        }

        SandboxOutcome {
            status,
            output,
            error,
            tool_calls_made: self.tool_calls_made(),
            duration: self.inner.started_at.elapsed(),
            stdout_truncated: self.inner.stdout_truncated.load(Ordering::Relaxed),
            stderr_truncated: self.inner.stderr_truncated.load(Ordering::Relaxed),
        }
    }
}

fn push_capped(buffer: &Mutex<String>, text: &str, limit_bytes: usize, truncated: &AtomicBool) {
    let mut buffer = buffer.lock();
    if buffer.len() >= limit_bytes {
        truncated.store(true, Ordering::Relaxed);
        return;
    }

    if !buffer.is_empty() {
        buffer.push('\n');
    }

    let remaining = limit_bytes.saturating_sub(buffer.len());
    if text.len() <= remaining {
        buffer.push_str(text);
        return;
    }

    let boundary = char_boundary_before(text, remaining.saturating_sub("...[truncated]".len()));
    buffer.push_str(&text[..boundary]);
    buffer.push_str("...[truncated]");
    truncated.store(true, Ordering::Relaxed);
}

fn char_boundary_before(text: &str, max_len: usize) -> usize {
    if text.len() <= max_len {
        return text.len();
    }
    let mut boundary = 0;
    for (index, _) in text.char_indices() {
        if index > max_len {
            break;
        }
        boundary = index;
    }
    boundary
}
