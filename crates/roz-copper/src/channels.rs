//! Synchronization channels between the async agent loop and the
//! real-time Copper task graph.
//!
//! The agent loop runs on Tokio at 1-3 Hz while the Copper graph ticks
//! on a dedicated thread at ~100 Hz.  This module provides the
//! cross-boundary primitives:
//!
//! * **Command channel** (`agent → Copper`): a bounded `tokio::sync::mpsc`
//!   channel carrying [`ControllerCommand`] values with back-pressure.
//! * **Shared state** (`Copper → agent`): an [`ArcSwap<ControllerState>`]
//!   that the Copper thread publishes into and the agent reads lock-free.

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use roz_core::controller::artifact::{ControllerArtifact, ExecutionMode};
use roz_core::controller::deployment::DeploymentState;
use roz_core::controller::evidence::ControllerEvidenceBundle;
use roz_core::embodiment::{EmbodimentRuntime, binding::ControlInterfaceManifest};

// ---------------------------------------------------------------------------
// Agent → Copper
// ---------------------------------------------------------------------------

/// Commands from the agent to the Copper controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControllerCommand {
    /// Load a verified WASM controller artifact with its compiled bytecode.
    ///
    /// The artifact carries pre-computed digests, verification key, and
    /// lifecycle metadata. The bytecode is the raw WASM/WAT source.
    /// The runtime lowers the canonical control interface into the legacy
    /// hot-path manifest only when preparing the controller off-thread.
    ///
    /// Production callers must supply a compiled [`EmbodimentRuntime`]; a
    /// missing runtime is treated as compatibility-only input and rejected
    /// before the control thread activates the controller.
    LoadArtifact(
        Box<ControllerArtifact>,
        Vec<u8>,
        ControlInterfaceManifest,
        Option<EmbodimentRuntime>,
    ),
    /// Request configured staged promotion for the currently loaded candidate.
    ///
    /// Copper executes the already-configured rollout path (`verified_only ->
    /// shadow -> canary -> active`, with allowed skips); this command only
    /// arms that progression. Under compatibility fallback policy, Copper
    /// ignores this command until a runtime-authoritative policy is injected.
    PromoteActive,
    /// Update controller parameters (JSON).
    UpdateParams(serde_json::Value),
    /// Halt the controller (stop ticking).
    Halt,
    /// Resume the controller.
    Resume,
}

impl ControllerCommand {
    /// Test-only helper for compatibility fixtures that intentionally omit a
    /// compiled embodiment runtime.
    #[cfg(test)]
    pub fn load_artifact_from_control_manifest(
        artifact: ControllerArtifact,
        bytes: Vec<u8>,
        control_manifest: &ControlInterfaceManifest,
    ) -> Self {
        Self::LoadArtifact(Box::new(artifact), bytes, control_manifest.clone(), None)
    }

    /// Build a load command from the canonical control-interface manifest plus
    /// a resolved embodiment runtime when the caller has one.
    pub fn load_artifact_with_embodiment_runtime(
        artifact: ControllerArtifact,
        bytes: Vec<u8>,
        control_manifest: &ControlInterfaceManifest,
        embodiment_runtime: &EmbodimentRuntime,
    ) -> Self {
        Self::LoadArtifact(
            Box::new(artifact),
            bytes,
            control_manifest.clone(),
            Some(embodiment_runtime.clone()),
        )
    }

    /// Convert a public agent-facing command into a Copper runtime command.
    ///
    /// `LoadArtifact` performs the expensive controller preparation step, so
    /// callers should run this off the real-time thread.
    pub fn into_runtime(self) -> Result<CopperRuntimeCommand, String> {
        match self {
            Self::LoadArtifact(artifact, bytes, control_manifest, embodiment_runtime) => {
                crate::controller::prepare_controller(*artifact, bytes, control_manifest, embodiment_runtime)
                    .map(CopperRuntimeCommand::PreparedArtifact)
            }
            Self::PromoteActive => Ok(CopperRuntimeCommand::PromoteActive),
            Self::UpdateParams(params) => Ok(CopperRuntimeCommand::UpdateParams(params)),
            Self::Halt => Ok(CopperRuntimeCommand::Halt),
            Self::Resume => Ok(CopperRuntimeCommand::Resume),
        }
    }
}

/// Internal command type used by the real-time Copper loop.
///
/// Unlike [`ControllerCommand`], this can carry a fully prepared controller
/// slot that has already been compiled and validated off the control thread.
#[derive(Debug)]
pub enum CopperRuntimeCommand {
    PreparedArtifact(crate::controller::PreparedController),
    PromoteActive,
    UpdateParams(serde_json::Value),
    Halt,
    Resume,
}

/// Lightweight summary of finalized controller evidence exposed through shared state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceSummaryState {
    pub bundle_id: String,
    pub controller_id: String,
    pub execution_mode: ExecutionMode,
    pub verifier_status: String,
    pub verifier_reason: Option<String>,
    pub ticks_run: u64,
    pub trap_count: u32,
    pub rejection_count: u32,
    pub limit_clamp_count: u32,
    pub channels_untouched: Vec<String>,
    pub state_freshness: roz_core::session::snapshot::FreshnessState,
    pub created_at_rfc3339: String,
}

impl From<&ControllerEvidenceBundle> for EvidenceSummaryState {
    fn from(bundle: &ControllerEvidenceBundle) -> Self {
        Self {
            bundle_id: bundle.bundle_id.clone(),
            controller_id: bundle.controller_id.clone(),
            execution_mode: bundle.execution_mode,
            verifier_status: bundle.verifier_status.to_string(),
            verifier_reason: bundle.verifier_reason.clone(),
            ticks_run: bundle.ticks_run,
            trap_count: bundle.trap_count,
            rejection_count: bundle.rejection_count,
            limit_clamp_count: bundle.limit_clamp_count,
            channels_untouched: bundle.channels_untouched.clone(),
            state_freshness: bundle.state_freshness.clone(),
            created_at_rfc3339: bundle.created_at.to_rfc3339(),
        }
    }
}

// ---------------------------------------------------------------------------
// Copper → Agent
// ---------------------------------------------------------------------------

/// State feedback from the Copper controller to the agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControllerState {
    /// Last tick number executed.
    pub last_tick: u64,
    /// Whether the controller is running.
    pub running: bool,
    /// Last motor command output (for observability).
    pub last_output: Option<serde_json::Value>,
    /// Entity poses from Gazebo (updated each tick when gazebo feature is active).
    #[serde(default)]
    pub entities: Vec<roz_core::spatial::EntityState>,
    /// E-stop reason, if an e-stop was triggered by the WASM controller or a
    /// controller error (trap, epoch timeout, OOM). `None` when healthy.
    /// The agent's `SpatialContextProvider` can observe this field.
    #[serde(default)]
    pub estop_reason: Option<String>,
    /// Live deployment stage currently visible in Copper.
    ///
    /// When a candidate is staged, this is that candidate stage; otherwise it
    /// reflects the active controller state.
    #[serde(default)]
    pub deployment_state: Option<DeploymentState>,
    /// Active controller id, if Copper currently has an actuating controller.
    #[serde(default)]
    pub active_controller_id: Option<String>,
    /// Candidate controller id, if Copper is shadowing/canarying a replacement.
    #[serde(default)]
    pub candidate_controller_id: Option<String>,
    /// Last known good controller id remembered during promotion.
    #[serde(default)]
    pub last_known_good_controller_id: Option<String>,
    /// Whether an operator/agent has requested staged promotion for the candidate.
    #[serde(default)]
    pub promotion_requested: bool,
    /// Progress within the candidate's current staged rollout window.
    #[serde(default)]
    pub candidate_stage_ticks_completed: u64,
    /// Required ticks for the candidate's current staged rollout window.
    #[serde(default)]
    pub candidate_stage_ticks_required: u64,
    /// Last absolute delta observed between active and candidate commands.
    #[serde(default)]
    pub candidate_last_max_abs_delta: Option<f64>,
    /// Last normalized delta observed between active and candidate commands.
    #[serde(default)]
    pub candidate_last_normalized_delta: Option<f64>,
    /// Whether the current canary command was clamped by the rollout envelope.
    #[serde(default)]
    pub candidate_canary_bounded: bool,
    /// Last reason a staged candidate was retired.
    ///
    /// Prefixes use explicit terminal disposition labels such as
    /// `rejected: ...` or `rolled_back: ...`.
    #[serde(default)]
    pub candidate_last_rejection_reason: Option<String>,
    /// Most recent finalized evidence summary for a live controller instance.
    #[serde(default)]
    pub last_live_evidence: Option<EvidenceSummaryState>,
    /// Most recent finalized live evidence bundle.
    #[serde(default)]
    pub last_live_evidence_bundle: Option<ControllerEvidenceBundle>,
    /// Most recent finalized evidence summary for a staged candidate instance.
    #[serde(default)]
    pub last_candidate_evidence: Option<EvidenceSummaryState>,
    /// Most recent finalized staged candidate evidence bundle.
    #[serde(default)]
    pub last_candidate_evidence_bundle: Option<ControllerEvidenceBundle>,
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Bounded channel depth for agent-to-Copper commands.
const COMMAND_CHANNEL_CAPACITY: usize = 64;

/// Return type of [`create_sync_pair`]: command channel, shared state, and e-stop channel.
pub type SyncPair = (
    mpsc::Sender<ControllerCommand>,
    mpsc::Receiver<ControllerCommand>,
    Arc<ArcSwap<ControllerState>>,
    mpsc::Sender<String>,
    mpsc::Receiver<String>,
);

/// Create command channel, shared state, and e-stop notification channel.
///
/// Returns [`SyncPair`] `(cmd_tx, cmd_rx, shared_state, estop_tx, estop_rx)` where:
/// - `cmd_tx` / `cmd_rx` form a bounded `mpsc` channel for
///   [`ControllerCommand`] values.
/// - `shared_state` is an [`ArcSwap`] that the Copper thread
///   [`store`](ArcSwap::store)s into and the agent
///   [`load`](ArcSwap::load)s from.
/// - `estop_tx` / `estop_rx` carry e-stop reason strings from the
///   controller loop to the supervisor/adapter. The buffer is small (4)
///   because e-stops are rare events; overflow is handled via `try_send`.
pub fn create_sync_pair() -> SyncPair {
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
    let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
    let (estop_tx, estop_rx) = mpsc::channel(4);
    (cmd_tx, cmd_rx, state, estop_tx, estop_rx)
}

/// Spawn a tokio task that forwards commands from the async agent side
/// to the synchronous Copper side.
///
/// The agent sends commands via `tokio::sync::mpsc`, and this bridge
/// forwards them to `std::sync::mpsc::SyncSender` for non-blocking
/// `try_recv()` on the Copper thread.
///
/// Returns a `JoinHandle` that can be aborted on shutdown.
pub fn spawn_command_bridge(
    mut agent_rx: mpsc::Receiver<ControllerCommand>,
    copper_tx: std::sync::mpsc::SyncSender<CopperRuntimeCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(cmd) = agent_rx.recv().await {
            let runtime_cmd = match cmd {
                ControllerCommand::LoadArtifact(artifact, bytes, control_manifest, embodiment_runtime) => {
                    match tokio::task::spawn_blocking(move || {
                        ControllerCommand::LoadArtifact(artifact, bytes, control_manifest, embodiment_runtime)
                            .into_runtime()
                    })
                    .await
                    {
                        Ok(Ok(prepared)) => prepared,
                        Ok(Err(error)) => {
                            tracing::error!(%error, "rejecting controller artifact load before control thread");
                            continue;
                        }
                        Err(error) => {
                            tracing::error!(%error, "controller preparation task failed before control thread");
                            continue;
                        }
                    }
                }
                ControllerCommand::PromoteActive => CopperRuntimeCommand::PromoteActive,
                ControllerCommand::UpdateParams(params) => CopperRuntimeCommand::UpdateParams(params),
                ControllerCommand::Halt => CopperRuntimeCommand::Halt,
                ControllerCommand::Resume => CopperRuntimeCommand::Resume,
            };

            match copper_tx.try_send(runtime_cmd) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(cmd)) => {
                    tracing::warn!("copper command channel full, blocking");
                    if copper_tx.send(cmd).is_err() {
                        tracing::warn!("copper command channel closed, bridge exiting");
                        break;
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    tracing::warn!("copper command channel closed, bridge exiting");
                    break;
                }
            }
        }
        tracing::debug!("command bridge task exiting");
    })
}

/// Create a synchronous (non-tokio) command channel for Copper-side use.
///
/// The Copper thread calls `rx.try_recv()` at the top of each tick
/// to drain zero or more queued commands.  Never blocks.
///
/// A forwarding bridge (tokio task) connects this to the async
/// [`create_sync_pair`] channel — see WS2 integration.
pub fn create_copper_channel() -> (
    std::sync::mpsc::SyncSender<CopperRuntimeCommand>,
    std::sync::mpsc::Receiver<CopperRuntimeCommand>,
) {
    std::sync::mpsc::sync_channel(COMMAND_CHANNEL_CAPACITY)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::embodiment::binding::{
        BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
    };

    #[tokio::test]
    async fn command_channel_sends_and_receives() {
        let (tx, mut rx, _state, _estop_tx, _estop_rx) = create_sync_pair();
        tx.send(ControllerCommand::Halt).await.unwrap();
        let cmd = rx.recv().await.unwrap();
        assert!(matches!(cmd, ControllerCommand::Halt));
    }

    #[tokio::test]
    async fn command_channel_preserves_order() {
        let (tx, mut rx, _state, _estop_tx, _estop_rx) = create_sync_pair();

        tx.send(ControllerCommand::Halt).await.unwrap();
        tx.send(ControllerCommand::Resume).await.unwrap();
        tx.send(ControllerCommand::UpdateParams(serde_json::json!({"gain": 1.5})))
            .await
            .unwrap();

        assert!(matches!(rx.recv().await.unwrap(), ControllerCommand::Halt));
        assert!(matches!(rx.recv().await.unwrap(), ControllerCommand::Resume));
        assert!(matches!(rx.recv().await.unwrap(), ControllerCommand::UpdateParams(_)));
    }

    #[test]
    fn load_artifact_from_control_manifest_keeps_canonical_manifest_on_command_surface() {
        let mut control_manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![
                ControlChannelDef {
                    name: "joint0/velocity".into(),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: "joint0_link".into(),
                },
                ControlChannelDef {
                    name: "joint1/velocity".into(),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: "joint1_link".into(),
                },
            ],
            bindings: vec![
                ChannelBinding {
                    physical_name: "joint0".into(),
                    channel_index: 0,
                    binding_type: BindingType::JointVelocity,
                    frame_id: "joint0_link".into(),
                    units: "rad/s".into(),
                    semantic_role: None,
                },
                ChannelBinding {
                    physical_name: "joint1".into(),
                    channel_index: 1,
                    binding_type: BindingType::JointVelocity,
                    frame_id: "joint1_link".into(),
                    units: "rad/s".into(),
                    semantic_role: None,
                },
            ],
        };
        control_manifest.stamp_digest();
        let artifact = ControllerArtifact {
            controller_id: "ctrl".into(),
            sha256: "sha".into(),
            source_kind: roz_core::controller::artifact::SourceKind::LlmGenerated,
            controller_class: roz_core::controller::artifact::ControllerClass::LowRiskCommandGenerator,
            generator_model: None,
            generator_provider: None,
            channel_manifest_version: 1,
            host_abi_version: 2,
            evidence_bundle_id: None,
            created_at: chrono::Utc::now(),
            promoted_at: None,
            replaced_controller_id: None,
            verification_key: roz_core::controller::artifact::VerificationKey {
                controller_digest: "sha".into(),
                wit_world_version: "bedrock:controller@1.0.0".into(),
                model_digest: "model".into(),
                calibration_digest: "calibration".into(),
                manifest_digest: control_manifest.manifest_digest.clone(),
                execution_mode: ExecutionMode::Verify,
                compiler_version: "wasmtime".into(),
                embodiment_family: None,
            },
            wit_world: "live-controller".into(),
            verifier_result: None,
        };

        let command =
            ControllerCommand::load_artifact_from_control_manifest(artifact, b"(module)".to_vec(), &control_manifest);

        match command {
            ControllerCommand::LoadArtifact(_, _, stored_manifest, stored_runtime) => {
                assert_eq!(stored_manifest, control_manifest);
                assert!(stored_runtime.is_none());
            }
            other => panic!("expected LoadArtifact, got {other:?}"),
        }
    }

    #[test]
    fn shared_state_updates_visible() {
        let (_, _, state, _, _) = create_sync_pair();

        // Update state (simulating Copper writing).
        state.store(Arc::new(ControllerState {
            last_tick: 42,
            running: true,
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
            last_live_evidence: None,
            last_live_evidence_bundle: None,
            last_candidate_evidence: None,
            last_candidate_evidence_bundle: None,
        }));

        // Read state (simulating agent reading).
        let current = state.load();
        assert_eq!(current.last_tick, 42);
        assert!(current.running);
    }

    #[test]
    fn copper_channel_try_recv_is_nonblocking() {
        let (tx, rx) = create_copper_channel();

        // Empty channel — try_recv returns Err immediately.
        assert!(rx.try_recv().is_err(), "empty channel should return Err");

        // Send a command.
        tx.send(CopperRuntimeCommand::Halt).unwrap();

        // try_recv returns it.
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, CopperRuntimeCommand::Halt));

        // Empty again.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn bridge_forwards_commands() {
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(64);
        let (copper_tx, copper_rx) = std::sync::mpsc::sync_channel(64);

        let bridge = spawn_command_bridge(agent_rx, copper_tx);

        // Send from agent side (tokio).
        agent_tx.send(ControllerCommand::Halt).await.unwrap();
        agent_tx.send(ControllerCommand::Resume).await.unwrap();

        // Small delay for bridge to forward.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Receive on Copper side (std sync).
        let cmd1 = copper_rx.try_recv().unwrap();
        assert!(matches!(cmd1, CopperRuntimeCommand::Halt));
        let cmd2 = copper_rx.try_recv().unwrap();
        assert!(matches!(cmd2, CopperRuntimeCommand::Resume));

        bridge.abort();
    }

    #[test]
    fn shared_state_defaults_to_idle() {
        let (_, _, state, _, _) = create_sync_pair();
        let current = state.load();
        assert_eq!(current.last_tick, 0);
        assert!(!current.running);
        assert!(current.last_output.is_none());
        assert!(current.estop_reason.is_none());
    }

    #[tokio::test]
    async fn estop_channel_receives_notification() {
        let (_cmd_tx, _cmd_rx, _state, estop_tx, mut estop_rx) = create_sync_pair();
        estop_tx
            .send("e-stop requested by WASM module".to_string())
            .await
            .unwrap();
        let msg = estop_rx.recv().await.unwrap();
        assert!(msg.contains("e-stop"));
    }

    #[tokio::test]
    async fn estop_channel_try_send_does_not_block() {
        let (_cmd_tx, _cmd_rx, _state, estop_tx, _estop_rx) = create_sync_pair();
        // Fill the buffer (capacity 4).
        for i in 0..4 {
            estop_tx.try_send(format!("error {i}")).unwrap();
        }
        // 5th send should fail (full), NOT block.
        let result = estop_tx.try_send("overflow".to_string());
        assert!(result.is_err(), "try_send on full estop channel should return Err");
    }

    #[test]
    fn estop_reason_in_shared_state() {
        let (_, _, state, _, _) = create_sync_pair();
        let reason = "controller_error: wasm trap: unreachable".to_string();
        state.store(Arc::new(ControllerState {
            last_tick: 10,
            running: false,
            last_output: None,
            entities: vec![],
            estop_reason: Some(reason.clone()),
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
            last_live_evidence: None,
            last_live_evidence_bundle: None,
            last_candidate_evidence: None,
            last_candidate_evidence_bundle: None,
        }));
        let current = state.load();
        assert_eq!(current.estop_reason.as_deref(), Some(reason.as_str()));
    }
}
