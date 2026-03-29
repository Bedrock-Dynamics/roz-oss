use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_nats::jetstream::Context as JetStreamContext;
use futures::StreamExt;
use roz_nats::dispatch::TaskInvocation;
use tracing::Instrument;
use uuid::Uuid;

/// Build the agent model from worker config, wrapping in `FallbackModel` when configured.
fn build_model(config: &roz_worker::config::WorkerConfig) -> anyhow::Result<Box<dyn roz_agent::model::Model>> {
    let primary = roz_agent::model::create_model(
        &config.model_name,
        &config.gateway_url,
        &config.gateway_api_key,
        config.model_timeout_secs,
        &config.anthropic_provider,
        config.anthropic_api_key.as_deref(),
    )?;

    if let Some(ref fallback_name) = config.fallback_model {
        match roz_agent::model::create_model(
            fallback_name,
            &config.gateway_url,
            &config.gateway_api_key,
            config.model_timeout_secs,
            &config.anthropic_provider,
            config.anthropic_api_key.as_deref(),
        ) {
            Ok(fallback) => {
                tracing::info!(fallback_model = %fallback_name, "model fallback configured");
                Ok(Box::new(roz_agent::model::FallbackModel::new(primary, fallback)))
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to create fallback model, proceeding without");
                Ok(primary)
            }
        }
    } else {
        Ok(primary)
    }
}

/// Run the agent loop for a single task, publish `WorkerExited` to the parent's team stream
/// if this is a child task, then signal the result back to Restate.
async fn execute_task(
    invocation: TaskInvocation,
    task_id: Uuid,
    task_config: roz_worker::config::WorkerConfig,
    task_js: JetStreamContext,
    task_http: reqwest::Client,
    restate_url: String,
) {
    tracing::info!("starting task execution");

    let agent_input = roz_worker::dispatch::build_agent_input(&invocation);
    let model = match build_model(&task_config) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "failed to build model for task, aborting");
            let agent_err = roz_agent::error::AgentError::Model(e.into());
            let result = roz_worker::dispatch::build_task_result(task_id, Err(agent_err));
            if let Err(sig_err) =
                roz_worker::dispatch::signal_result(&task_http, &restate_url, &task_id.to_string(), &result).await
            {
                tracing::error!(error = %sig_err, "failed to signal model-build failure to Restate");
            }
            return;
        }
    };

    let mut dispatcher = roz_agent::dispatch::ToolDispatcher::new(Duration::from_secs(30));
    let safety = roz_agent::safety::SafetyStack::new(vec![]);

    // Spawn Copper controller for OodaReAct mode.
    let copper_handle = match invocation.mode {
        roz_nats::dispatch::ExecutionMode::OodaReAct => {
            let max_velocity = task_config.max_velocity.unwrap_or(1.5);
            let handle = roz_worker::copper_handle::CopperHandle::spawn(max_velocity);
            tracing::info!("copper controller spawned for OodaReAct task");
            Some(handle)
        }
        roz_nats::dispatch::ExecutionMode::React => None,
    };

    let spatial: Box<dyn roz_agent::spatial_provider::SpatialContextProvider> = if let Some(ref handle) = copper_handle
    {
        Box::new(roz_worker::spatial_bridge::CopperSpatialProvider::new(Arc::clone(
            handle.state(),
        )))
    } else {
        Box::new(roz_agent::spatial_provider::MockSpatialContextProvider::empty())
    };

    // When Copper is active, register the deploy_controller tool and inject the
    // command channel into Extensions so tool implementations can reach it.
    let mut extensions = roz_agent::dispatch::Extensions::new();
    if let Some(ref handle) = copper_handle {
        extensions.insert(handle.cmd_tx());
        dispatcher.register_with_category(
            Box::new(roz_local::tools::deploy_controller::DeployControllerTool),
            roz_core::tools::ToolCategory::Physical,
        );
    }

    let mut agent =
        roz_agent::agent_loop::AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);
    let output = agent.run(agent_input).await;

    // If this is a child task (has a parent), notify the parent's team stream that this
    // child worker has exited. Complements WorkerCompleted/WorkerFailed which are published
    // earlier in the model result path.
    if let Some(parent_task_id) = invocation.parent_task_id {
        let event = roz_core::team::TeamEvent::WorkerExited {
            worker_id: task_id,
            parent_task_id,
        };
        if let Err(e) = roz_nats::team::publish_team_event(&task_js, parent_task_id, task_id, &event).await {
            tracing::warn!(
                error = %e,
                %task_id,
                %parent_task_id,
                "failed to publish WorkerExited"
            );
        }
    }

    let result = roz_worker::dispatch::build_task_result(task_id, output);

    if let Err(e) = roz_worker::dispatch::signal_result(&task_http, &restate_url, &task_id.to_string(), &result).await {
        tracing::error!(error = %e, "failed to signal result to Restate");
    }

    // Shut down Copper if it was spawned.
    if let Some(handle) = copper_handle {
        handle.shutdown().await;
    }
}

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "sequential startup with telemetry + capabilities + task loop"
)]
async fn main() -> Result<()> {
    let logfire = logfire::configure()
        .with_service_name("roz-worker")
        .with_service_version(env!("CARGO_PKG_VERSION"))
        .with_environment(std::env::var("ROZ_ENVIRONMENT").unwrap_or_else(|_| "development".into()))
        .finish()
        .expect("failed to configure logfire");
    let _guard = logfire.shutdown_guard();

    let config = roz_worker::config::WorkerConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;

    tracing::info!(worker_id = %config.worker_id, "starting roz-worker");

    // Connect to NATS
    let nats = async_nats::connect(&config.nats_url).await?;
    tracing::info!(nats_url = %config.nats_url, "connected to NATS");
    let js = async_nats::jetstream::new(nats.clone());

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client");

    // Publish heartbeat on interval
    let hb_nats = nats.clone();
    let hb_worker_id = config.worker_id.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let subject = format!("events.{hb_worker_id}.heartbeat");
            if let Err(e) = hb_nats.publish(subject, bytes::Bytes::from_static(b"{}")).await {
                tracing::warn!(error = %e, "failed to publish heartbeat");
            }
        }
    });

    // Spawn telemetry publisher (10 Hz)
    let telem_nats = nats.clone();
    let telem_worker_id = config.worker_id.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            let state = serde_json::json!({
                "timestamp": chrono::Utc::now().timestamp_millis(),
                "joints": [],
                "sensors": {}
            });
            if let Err(e) = roz_worker::telemetry::publish_state(&telem_nats, &telem_worker_id, &state).await {
                tracing::trace!(error = %e, "telemetry publish failed");
            }
        }
    });

    // Initialize camera system
    let camera_manager = if config.camera.enabled || config.camera.test_pattern {
        let hub = roz_worker::camera::stream_hub::StreamHub::new();
        let mut manager = roz_worker::camera::CameraManager::new(hub);
        if config.camera.test_pattern {
            let cam_info = manager.add_test_pattern().await;
            tracing::info!(camera = %cam_info.id, "test pattern camera registered");
        }
        Some(manager)
    } else {
        tracing::info!("camera system disabled");
        None
    };

    // Publish capabilities on startup
    let mut caps = roz_core::capabilities::RobotCapabilities {
        robot_type: "generic".to_string(),
        joints: vec![],
        control_modes: vec!["position".to_string(), "velocity".to_string()],
        workspace_bounds: None,
        sensors: vec![],
        max_velocity: config.max_velocity.unwrap_or(1.5),
        cameras: vec![],
    };

    if let Some(ref cam_mgr) = camera_manager {
        caps.cameras = cam_mgr
            .cameras()
            .iter()
            .map(|c| roz_core::capabilities::CameraCapability {
                id: c.id.0.clone(),
                label: c.label.clone(),
                resolution: [
                    c.supported_resolutions.first().map_or(640, |r| r.0),
                    c.supported_resolutions.first().map_or(480, |r| r.1),
                ],
                fps: c.max_fps,
                hw_encoder: c.hw_encoder_available,
            })
            .collect();
    }
    let caps_subject =
        roz_nats::subjects::Subjects::capabilities(&config.worker_id).expect("valid worker_id for capabilities");
    if let Ok(payload) = serde_json::to_vec(&caps)
        && let Err(e) = nats.publish(caps_subject, payload.into()).await
    {
        tracing::warn!(error = %e, "failed to publish capabilities");
    }

    // Subscribe to e-stop events
    let estop_sub = roz_worker::estop::subscribe_estop(&nats, &config.worker_id).await?;
    let estop_rx = roz_worker::estop::spawn_estop_listener(estop_sub);
    tracing::info!(worker_id = %config.worker_id, "e-stop listener active");

    // Register with server
    if !config.api_key.is_empty() {
        match roz_worker::registration::register_host(&config.api_url, &config.api_key, &config.worker_id).await {
            Ok(host_id) => tracing::info!(host_id = %host_id, "registered with server"),
            Err(e) => tracing::warn!(error = %e, "host registration failed"),
        }
    }

    // Spawn edge agent session relay (handles gRPC sessions relayed via NATS).
    let relay_nats = nats.clone();
    let relay_worker_id = config.worker_id.clone();
    let relay_config = config.clone();
    tokio::spawn(async move {
        if let Err(e) = roz_worker::session_relay::spawn_session_relay(relay_nats, relay_worker_id, relay_config).await
        {
            tracing::error!(error = %e, "session relay exited");
        }
    });

    // Subscribe to task invocations
    let worker_id = &config.worker_id;
    let subject = format!("invoke.{worker_id}.>");
    let mut sub = nats.subscribe(subject.clone()).await?;
    tracing::info!(subject, "subscribed to invocations, waiting for tasks");

    let restate_url = config.restate_url.clone();

    while let Some(msg) = sub.next().await {
        if *estop_rx.borrow() {
            tracing::error!("E-STOP active — rejecting task invocation");
            continue;
        }

        tracing::info!(
            subject = %msg.subject,
            bytes = msg.payload.len(),
            "received invocation"
        );

        let invocation: TaskInvocation = match serde_json::from_slice(&msg.payload) {
            Ok(inv) => inv,
            Err(e) => {
                tracing::error!(error = %e, "failed to deserialize TaskInvocation");
                continue;
            }
        };

        if let Some(ref tp) = invocation.traceparent {
            tracing::info!(traceparent = %tp, task_id = %invocation.task_id, "linking to server trace");
        }

        tracing::info!(
            task_id = %invocation.task_id,
            tenant_id = %invocation.tenant_id,
            mode = ?invocation.mode,
            "dispatching task"
        );

        let task_http = http.clone();
        let restate_url = restate_url.clone();
        let task_id = invocation.task_id;
        let task_config = config.clone();
        let task_js = js.clone();

        let span = tracing::info_span!("worker.execute_task", task_id = %task_id);
        tokio::spawn(execute_task(invocation, task_id, task_config, task_js, task_http, restate_url).instrument(span));
    }

    tracing::warn!("NATS subscription closed, worker exiting");
    Ok(())
}
