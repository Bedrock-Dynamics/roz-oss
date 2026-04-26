//! Shared physical-runtime assembly for worker OodaReAct tasks.
//!
//! This is the production seam between a task's authoritative
//! `EmbodimentRuntime`, the worker-side IO factory registry, Copper, and the
//! agent-visible lifecycle tools.

use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use anyhow::Context as _;
use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher};
use roz_copper::channels::ControllerState;
use roz_copper::handle::CopperHandle;
use roz_copper::io::ActuatorSink;
use roz_copper::latch::LatchState;
use roz_core::embodiment::EmbodimentRuntime;
use roz_core::embodiment::binding::ControlInterfaceManifest;
use roz_core::tools::ToolCategory;

use crate::io_backends::factory_for;

/// Inputs required to spawn the physical runtime for an OodaReAct task.
pub struct PhysicalRuntimeConfig {
    pub runtime: EmbodimentRuntime,
    pub control_manifest: ControlInterfaceManifest,
    pub max_velocity: f64,
    pub hot_policy: roz_copper::policy::HotCopperPolicy,
    pub shared_backpressure: Arc<AtomicU8>,
    pub latch_persist_tx: Option<std::sync::mpsc::SyncSender<LatchState>>,
    pub dispatcher: ToolDispatcher,
    pub extensions: Extensions,
    pub task_id: String,
    pub tenant_id: String,
    #[cfg(feature = "test-fixtures")]
    rollout_authority: Option<PhysicalRuntimeRolloutAuthority>,
}

impl PhysicalRuntimeConfig {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        runtime: EmbodimentRuntime,
        control_manifest: ControlInterfaceManifest,
        max_velocity: f64,
        hot_policy: roz_copper::policy::HotCopperPolicy,
        shared_backpressure: Arc<AtomicU8>,
        latch_persist_tx: Option<std::sync::mpsc::SyncSender<LatchState>>,
        dispatcher: ToolDispatcher,
        extensions: Extensions,
        task_id: String,
        tenant_id: String,
    ) -> Self {
        Self {
            runtime,
            control_manifest,
            max_velocity,
            hot_policy,
            shared_backpressure,
            latch_persist_tx,
            dispatcher,
            extensions,
            task_id,
            tenant_id,
            #[cfg(feature = "test-fixtures")]
            rollout_authority: None,
        }
    }

    /// Give the test harness explicit rollout authority so it can prove
    /// actuator movement after the worker/agent path loads a verified
    /// controller. Production callers use the execution-only default.
    #[cfg(feature = "test-fixtures")]
    #[must_use]
    pub fn with_rollout_authority(mut self, authority: PhysicalRuntimeRolloutAuthority) -> Self {
        self.rollout_authority = Some(authority);
        self
    }
}

/// Test-only rollout policy for vertical acceptance harnesses.
#[cfg(feature = "test-fixtures")]
#[derive(Debug, Clone, Copy)]
pub struct PhysicalRuntimeRolloutAuthority {
    pub shadow_ticks_required: u64,
    pub canary_ticks_required: u64,
}

#[cfg(feature = "test-fixtures")]
impl Default for PhysicalRuntimeRolloutAuthority {
    fn default() -> Self {
        Self {
            shadow_ticks_required: 1,
            canary_ticks_required: 1,
        }
    }
}

/// Handle returned after physical runtime assembly.
pub struct PhysicalRuntimeHandle {
    pub copper: CopperHandle,
    pub dispatcher: ToolDispatcher,
    pub context: ToolContext,
    pub factory_name: &'static str,
    #[cfg(feature = "test-fixtures")]
    pub openclaw_observation: Option<FakeOpenclawObservation>,
}

/// Snapshot of commands observed at the fake OpenClaw actuator boundary.
#[cfg(feature = "test-fixtures")]
#[derive(Debug, Clone, Default)]
pub struct FakeOpenclawObservedState {
    pub joint_positions: Vec<f64>,
    pub joint_velocities: Vec<f64>,
    pub command_count: u64,
}

#[cfg(feature = "test-fixtures")]
#[derive(Clone)]
pub struct FakeOpenclawObservation {
    inner: Arc<std::sync::Mutex<FakeOpenclawObservedState>>,
}

#[cfg(feature = "test-fixtures")]
impl FakeOpenclawObservation {
    #[must_use]
    fn new(joint_count: usize) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(FakeOpenclawObservedState {
                joint_positions: vec![0.0; joint_count],
                joint_velocities: vec![0.0; joint_count],
                command_count: 0,
            })),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> FakeOpenclawObservedState {
        self.inner
            .lock()
            .expect("fake-openclaw observation mutex poisoned")
            .clone()
    }

    fn record_command(&self, values: &[f64]) {
        let mut state = self.inner.lock().expect("fake-openclaw observation mutex poisoned");
        let count = state.joint_velocities.len().min(values.len());
        for (i, value) in values.iter().copied().enumerate().take(count) {
            state.joint_velocities[i] = value;
            state.joint_positions[i] += value * 0.01;
        }
        state.command_count += 1;
    }
}

#[cfg(feature = "test-fixtures")]
struct ObservedActuator {
    inner: Arc<dyn ActuatorSink>,
    observation: FakeOpenclawObservation,
}

#[cfg(feature = "test-fixtures")]
impl ActuatorSink for ObservedActuator {
    fn send(&self, frame: &roz_core::command::CommandFrame) -> anyhow::Result<()> {
        self.observation.record_command(&frame.values);
        self.inner.send(frame)
    }
}

#[cfg(feature = "test-fixtures")]
fn maybe_observe_openclaw_actuator(
    family_id: Option<&str>,
    actuator: Arc<dyn ActuatorSink>,
    joint_count: usize,
) -> (Arc<dyn ActuatorSink>, Option<FakeOpenclawObservation>) {
    if matches!(family_id, Some("openclaw" | "manipulator")) {
        let observation = FakeOpenclawObservation::new(joint_count);
        let observed = Arc::new(ObservedActuator {
            inner: actuator,
            observation: observation.clone(),
        });
        (observed, Some(observation))
    } else {
        (actuator, None)
    }
}

#[cfg(not(feature = "test-fixtures"))]
fn maybe_observe_openclaw_actuator(
    _family_id: Option<&str>,
    actuator: Arc<dyn ActuatorSink>,
    _joint_count: usize,
) -> (Arc<dyn ActuatorSink>, Option<()>) {
    (actuator, None)
}

/// Spawn Copper, install physical tool extensions, and register lifecycle tools.
///
/// # Errors
///
/// Returns an error when the task lacks a registered IO factory for the
/// embodiment family or when that factory cannot construct actuator/sensor IO.
pub fn spawn_physical_runtime(mut config: PhysicalRuntimeConfig) -> anyhow::Result<PhysicalRuntimeHandle> {
    let family_id = config
        .runtime
        .model
        .embodiment_family
        .as_ref()
        .map(|family| family.family_id.as_str());
    let factory = factory_for(family_id).ok_or_else(|| {
        anyhow::anyhow!(
            "no IoFactory for embodiment_family={:?}",
            config.runtime.model.embodiment_family
        )
    })?;
    let factory_name = factory.name();

    let (actuator, sensor) = factory
        .build(&config.runtime, &config.control_manifest)
        .with_context(|| format!("IoFactory build failed for {factory_name}"))?;
    let joint_count = config.runtime.model.joints.len();
    let (actuator, openclaw_observation) = maybe_observe_openclaw_actuator(family_id, actuator, joint_count);
    #[cfg(not(feature = "test-fixtures"))]
    let _ = openclaw_observation;

    #[cfg(feature = "test-fixtures")]
    let copper = if let Some(authority) = config.rollout_authority {
        let deployment_manager = roz_copper::deployment_manager::DeploymentManager::with_rollout_policy(
            false,
            true,
            true,
            authority.shadow_ticks_required,
            authority.canary_ticks_required,
            10_000,
            10_000,
            u64::MAX,
        );
        CopperHandle::spawn_with_io_and_deployment_manager_and_wiring(
            config.max_velocity,
            Some(actuator),
            Some(sensor),
            deployment_manager,
            Some(config.hot_policy.clone()),
            Some(config.shared_backpressure.clone()),
            config.latch_persist_tx.take(),
        )
    } else {
        CopperHandle::spawn_with_policy_and_io(
            config.max_velocity,
            actuator,
            Some(sensor),
            config.hot_policy.clone(),
            config.shared_backpressure.clone(),
            config.latch_persist_tx.take(),
        )
    };

    #[cfg(not(feature = "test-fixtures"))]
    let copper = CopperHandle::spawn_with_policy_and_io(
        config.max_velocity,
        actuator,
        Some(sensor),
        config.hot_policy.clone(),
        config.shared_backpressure.clone(),
        config.latch_persist_tx.take(),
    );

    let mut extensions = config.extensions;
    extensions.insert(copper.cmd_tx());
    extensions.insert(config.control_manifest.clone());
    extensions.insert(Arc::clone(copper.state()) as Arc<arc_swap::ArcSwap<ControllerState>>);
    extensions.insert(config.runtime.clone());

    config.dispatcher.register_with_category(
        Box::new(crate::tools::promote_controller::PromoteControllerTool::new(
            &config.control_manifest,
        )),
        ToolCategory::Physical,
    );
    config.dispatcher.register_with_category(
        Box::new(crate::tools::stop_controller::StopControllerTool),
        ToolCategory::Physical,
    );
    config.dispatcher.register_with_category(
        Box::new(crate::tools::controller_status::ControllerStatusTool),
        ToolCategory::Physical,
    );

    Ok(PhysicalRuntimeHandle {
        copper,
        dispatcher: config.dispatcher,
        context: ToolContext {
            task_id: config.task_id,
            tenant_id: config.tenant_id,
            call_id: String::new(),
            extensions,
        },
        factory_name,
        #[cfg(feature = "test-fixtures")]
        openclaw_observation,
    })
}
