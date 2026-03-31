use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_nats::jetstream::Context as JetStreamContext;
use futures::StreamExt;
use roz_nats::dispatch::TaskInvocation;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

/// Run the agent loop for a single task, publish `WorkerExited` to the parent's team stream
/// if this is a child task, then signal the result back to Restate.
#[expect(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "sequential task lifecycle with model + tools + safety"
)]
async fn execute_task(
    invocation: TaskInvocation,
    task_id: Uuid,
    task_config: roz_worker::config::WorkerConfig,
    task_nats: async_nats::Client,
    task_js: JetStreamContext,
    task_http: reqwest::Client,
    restate_url: String,
    mut estop_rx: tokio::sync::watch::Receiver<bool>,
    camera_manager: Option<Arc<roz_worker::camera::CameraManager>>,
) {
    tracing::info!("starting task execution");

    let agent_cancel = CancellationToken::new();
    let mut agent_input = roz_worker::dispatch::build_agent_input(&invocation);
    agent_input.cancellation_token = Some(agent_cancel.clone());
    let model = match roz_worker::model_factory::build_model(&task_config) {
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
    let guards: Vec<Box<dyn roz_agent::safety::SafetyGuard>> = vec![Box::new(
        roz_agent::safety::guards::VelocityLimiter::new(task_config.max_velocity.unwrap_or(1.5)),
    )];
    let safety = roz_agent::safety::SafetyStack::new(guards);

    // Spawn Copper controller for OodaReAct mode.
    let mut copper_handle = match invocation.mode {
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
        Box::new(roz_agent::spatial_provider::NullSpatialContextProvider)
    };

    // When Copper is active, register the deploy_controller tool and inject the
    // command channel into Extensions so tool implementations can reach it.
    let mut extensions = roz_agent::dispatch::Extensions::new();
    if let Some(ref handle) = copper_handle {
        extensions.insert(handle.cmd_tx());
        // TODO: Load ChannelManifest from EnvironmentConfig in task invocation.
        extensions.insert(roz_core::channels::ChannelManifest::default());
        dispatcher.register_with_category(
            Box::new(roz_local::tools::deploy_controller::DeployControllerTool),
            roz_core::tools::ToolCategory::Physical,
        );
    }

    // Register camera perception tools when cameras are available.
    if let Some(ref cam_mgr) = camera_manager {
        extensions.insert(cam_mgr.clone());
        let shared_vision_config = Arc::new(tokio::sync::RwLock::new(roz_core::edge::vision::VisionConfig::default()));
        extensions.insert(shared_vision_config);
        dispatcher.register_with_category(
            Box::new(roz_worker::camera::perception::CaptureFrameTool),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(roz_worker::camera::perception::ListCamerasTool),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(roz_worker::camera::perception::SetVisionStrategyTool),
            roz_core::tools::ToolCategory::Pure,
        );
        tracing::info!("camera perception tools registered");
    }

    // Register team tools (spawn_worker, watch_team) when this is an orchestrator
    // (no parent_task_id). Workers cannot spawn their own workers.
    if invocation.parent_task_id.is_none() {
        if let Ok(tenant_uuid) = invocation.tenant_id.parse::<Uuid>() {
            dispatcher.register_with_category(
                Box::new(roz_agent::tools::spawn_worker::SpawnWorkerTool::new(
                    task_nats.clone(),
                    task_id,
                    invocation.environment_id,
                    task_js.clone(),
                    tenant_uuid,
                )),
                roz_core::tools::ToolCategory::Pure,
            );
            dispatcher.register_with_category(
                Box::new(roz_agent::tools::watch_team::WatchTeamTool::new(
                    task_js.clone(),
                    task_id,
                )),
                roz_core::tools::ToolCategory::Pure,
            );
            tracing::info!("team tools registered (orchestrator mode)");
        } else {
            tracing::warn!(
                tenant_id = %invocation.tenant_id,
                "skipping team tool registration: tenant_id is not a valid UUID"
            );
        }
    }

    // Rebuild constitution now that all tools are registered, so conditional
    // tiers (camera, WASM, team, etc.) match the actual tool set.
    {
        let names = dispatcher.tool_names();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        agent_input.system_prompt[0] = roz_agent::constitution::build_constitution(agent_input.mode, &name_refs);
    }

    let mut agent =
        roz_agent::agent_loop::AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);

    let output = tokio::select! {
        result = agent.run(agent_input) => result,
        _ = estop_rx.changed() => {
            if *estop_rx.borrow() {
                tracing::error!(task_id = %task_id, "E-STOP during task execution");
                agent_cancel.cancel(); // cooperative cancel first
                // DROP copper handle — triggers emergency halt (zeroes all commands).
                drop(copper_handle.take());
                let agent_err = roz_agent::error::AgentError::Safety("E-STOP activated during task execution".into());
                let result = roz_worker::dispatch::build_task_result(task_id, Err(agent_err));
                if let Err(e) = roz_worker::dispatch::signal_result(
                    &task_http,
                    &restate_url,
                    &task_id.to_string(),
                    &result,
                )
                .await
                {
                    tracing::error!(error = %e, "failed to signal E-STOP result to Restate");
                }
                return;
            }
            // Spurious wakeup (value still false) — agent future already cancelled,
            // input consumed. This is unreachable in practice because estop_rx only
            // transitions false→true, but we cannot recover the moved input.
            Err(roz_agent::error::AgentError::Internal(
                anyhow::anyhow!("estop watch fired without activation — agent turn lost"),
            ))
        }
    };

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
        let subject = roz_nats::subjects::Subjects::event(&hb_worker_id, "heartbeat").expect("valid worker_id");
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Err(e) = hb_nats.publish(subject.clone(), bytes::Bytes::from_static(b"{}")).await {
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
    let camera_manager: Option<Arc<roz_worker::camera::CameraManager>> =
        if config.camera.enabled || config.camera.test_pattern {
            let hub = roz_worker::camera::stream_hub::StreamHub::new();
            let mut manager = roz_worker::camera::CameraManager::new(hub);
            if config.camera.test_pattern {
                let cam_info = manager.add_test_pattern().await;
                tracing::info!(camera = %cam_info.id, "test pattern camera registered");
            }
            Some(Arc::new(manager))
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

    // Spawn idle watchdog — fires if no NATS message arrives within 30s.
    let watchdog = Arc::new(roz_worker::command_watchdog::CommandWatchdog::new(Duration::from_secs(
        30,
    )));
    let watchdog_cancel = CancellationToken::new();
    let wd = watchdog.clone();
    let wd_cancel = watchdog_cancel.clone();
    tokio::spawn(async move { wd.run(wd_cancel).await });
    tracing::info!("idle watchdog active (30s deadline)");

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
    let relay_estop_rx = estop_rx.clone();
    let relay_camera_mgr = camera_manager.clone();
    tokio::spawn(async move {
        if let Err(e) = roz_worker::session_relay::spawn_session_relay(
            relay_nats,
            relay_worker_id,
            relay_config,
            relay_estop_rx,
            relay_camera_mgr,
        )
        .await
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
        watchdog.pet();

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

        let task_nats = nats.clone();
        let task_http = http.clone();
        let restate_url = restate_url.clone();
        let task_id = invocation.task_id;
        let task_config = config.clone();
        let task_js = js.clone();
        let task_camera_mgr = camera_manager.clone();

        let task_estop_rx = estop_rx.clone();
        let span = tracing::info_span!("worker.execute_task", task_id = %task_id);
        tokio::spawn(
            execute_task(
                invocation,
                task_id,
                task_config,
                task_nats,
                task_js,
                task_http,
                restate_url,
                task_estop_rx,
                task_camera_mgr,
            )
            .instrument(span),
        );
    }

    tracing::warn!("NATS subscription closed, worker exiting");
    Ok(())
}
