#![allow(dead_code)]

use std::process::Stdio;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher, TypedToolExecutor};
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_copper::channels::ControllerCommand;
use roz_copper::handle::CopperHandle;
use roz_core::embodiment::binding::ControlInterfaceManifest;
use roz_core::embodiment::{EmbodimentModel, EmbodimentRuntime, FrameSource, Joint, Link, Transform3D};
use tokio::net::TcpStream;
use tokio::process::Command as AsyncCommand;

pub struct DockerSimSpec {
    pub name: &'static str,
    pub image: &'static str,
    pub args: &'static [&'static str],
    pub grpc_port: u16,
    pub ros_domain_id: u8,
    pub startup_timeout: Duration,
}

const MANIPULATOR_DOCKER_ARGS: &[&str] = &["-p", "9094:9090", "-p", "8094:8090", "-e", "ROS_LOCALHOST_ONLY=1"];

pub const MANIPULATOR_SIM: DockerSimSpec = DockerSimSpec {
    name: "roz-test-manip",
    image: "bedrockdynamics/substrate-sim:ros2-manipulator",
    args: MANIPULATOR_DOCKER_ARGS,
    grpc_port: 9094,
    ros_domain_id: 44,
    startup_timeout: Duration::from_secs(180),
};

pub fn live_test_mutex() -> &'static tokio::sync::Mutex<()> {
    static LIVE_TEST_MUTEX: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LIVE_TEST_MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub fn compile_test_embodiment_runtime(control_manifest: &ControlInterfaceManifest) -> EmbodimentRuntime {
    let mut frame_tree = roz_core::embodiment::FrameTree::new();
    frame_tree.set_root("world", FrameSource::Static);

    let mut links = vec![Link {
        name: "world".into(),
        parent_joint: None,
        inertial: None,
        visual_geometry: None,
        collision_geometry: None,
    }];
    let mut watched_frames = Vec::new();
    let mut seen_frames = std::collections::BTreeSet::new();

    for frame_id in control_manifest
        .channels
        .iter()
        .map(|channel| channel.frame_id.as_str())
        .chain(
            control_manifest
                .bindings
                .iter()
                .map(|binding| binding.frame_id.as_str()),
        )
    {
        if frame_id.is_empty() || !seen_frames.insert(frame_id.to_string()) {
            continue;
        }
        let _ = frame_tree.add_frame(frame_id, "world", Transform3D::identity(), FrameSource::Dynamic);
        links.push(Link {
            name: frame_id.to_string(),
            parent_joint: None,
            inertial: None,
            visual_geometry: None,
            collision_geometry: None,
        });
        watched_frames.push(frame_id.to_string());
    }

    if watched_frames.is_empty() {
        watched_frames.push("world".into());
    }

    let model = EmbodimentModel {
        model_id: "roz-local-live-test".into(),
        model_digest: String::new(),
        embodiment_family: None,
        links,
        joints: Vec::<Joint>::new(),
        frame_tree,
        collision_bodies: Vec::new(),
        allowed_collision_pairs: Vec::new(),
        tcps: Vec::new(),
        sensor_mounts: Vec::new(),
        workspace_zones: Vec::new(),
        watched_frames,
        channel_bindings: control_manifest.bindings.clone(),
    };

    EmbodimentRuntime::compile(model, None, None)
}

pub async fn recreate_docker_sim(spec: &DockerSimSpec) -> Result<(), String> {
    if !docker_available() {
        return Err("Docker daemon is not reachable".into());
    }

    let _ = AsyncCommand::new("docker")
        .args(["rm", "-f", spec.name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        spec.name.to_string(),
    ];
    args.extend(spec.args.iter().map(|arg| (*arg).to_string()));
    args.push("-e".to_string());
    args.push(format!("ROS_DOMAIN_ID={}", spec.ros_domain_id));
    args.push(spec.image.to_string());

    let output = AsyncCommand::new("docker")
        .args(&args)
        .output()
        .await
        .map_err(|error| format!("failed to launch {}: {error}", spec.name))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "docker run for {} failed: stdout=`{stdout}` stderr=`{stderr}`",
            spec.name
        ));
    }

    wait_for_tcp_port(spec.grpc_port, spec.startup_timeout).await?;
    wait_for_container_health(spec.name, spec.startup_timeout).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    Ok(())
}

async fn wait_for_container_health(name: &str, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut saw_healthcheck = false;

    loop {
        let output = AsyncCommand::new("docker")
            .args([
                "inspect",
                "-f",
                "{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}",
                name,
            ])
            .output()
            .await
            .map_err(|error| format!("failed to inspect container {name}: {error}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(format!("docker inspect for {name} failed: {stderr}"));
        }

        let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
        match status.as_str() {
            "healthy" => return Ok(()),
            "none" => {
                if !saw_healthcheck {
                    return Ok(());
                }
            }
            "starting" => {
                saw_healthcheck = true;
            }
            "unhealthy" => {
                return Err(format!("container {name} reported unhealthy"));
            }
            _ => {}
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "container {name} did not become healthy before timeout; last health status was `{last_status}`",
                last_status = status
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub fn extract_wat_blob(response: &str) -> &str {
    if let Some(start) = response.find("```") {
        let after_fence = &response[start + 3..];
        let code_start = after_fence.find('\n').map_or(0, |index| index + 1);
        let code = &after_fence[code_start..];
        if let Some(end) = code.find("```") {
            return code[..end].trim();
        }
    }
    response.trim()
}

pub fn assert_live_controller_wat(response: &str) {
    let wat = extract_wat_blob(response);
    assert_eq!(
        wat.matches("(module").count(),
        1,
        "Claude should return a single WAT module"
    );
    assert!(
        wat.contains(r#"(memory (export "cm32p2_memory")"#),
        "missing canonical exported memory"
    );
    assert!(
        wat.contains(r#"(import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode""#),
        "missing canonical current-execution-mode import"
    );
    assert!(
        wat.contains(r#"(export "cm32p2|bedrock:controller/control@1|process")"#),
        "missing canonical process export"
    );
    assert!(
        wat.contains(r#"(export "cm32p2|bedrock:controller/control@1|process_post")"#),
        "missing canonical process_post export"
    );
    assert!(wat.contains(r#"(export "cm32p2_realloc")"#), "missing realloc export");
    assert!(
        wat.contains(r#"(export "cm32p2_initialize")"#),
        "missing initialize export"
    );
}

pub fn constant_controller_prompt(control_manifest: &ControlInterfaceManifest, command_values: &[f64]) -> String {
    assert_eq!(
        control_manifest.channels.len(),
        command_values.len(),
        "command_values length must match control manifest channel count"
    );

    let mut result_record = Vec::new();
    result_record.extend_from_slice(&(64u32).to_le_bytes());
    result_record.extend_from_slice(&(command_values.len() as u32).to_le_bytes());
    let result_record = escape_bytes(&result_record);
    let command_bytes: Vec<u8> = command_values.iter().flat_map(|value| value.to_le_bytes()).collect();
    let command_bytes = escape_bytes(&command_bytes);

    let mut channel_lines = String::new();
    for (index, channel) in control_manifest.channels.iter().enumerate() {
        let value = command_values[index];
        channel_lines.push_str(&format!(
            "  {index}: {} {:?} frame={} => {value:.3}\n",
            channel.name, channel.interface_type, channel.frame_id
        ));
    }

    format!(
        "You are a robot controller engineer. Return ONLY raw WAT.\n\n\
         Write a canonical core-Wasm source module for the checked-in `live-controller` world.\n\
         Command channels and required outputs:\n\
{channel_lines}\n\
         Required ABI:\n\
         - import `cm32p2|bedrock:controller/runtime@1` `current-execution-mode`\n\
         - export memory as `cm32p2_memory`\n\
         - export `cm32p2|bedrock:controller/control@1|process`\n\
         - export `cm32p2|bedrock:controller/control@1|process_post`\n\
         - export `cm32p2_realloc`\n\
         - export `cm32p2_initialize`\n\
         Required behavior:\n\
         - static result record at offset 0 with pointer 64 and count {count}\n\
         - static command vector at offset 64 with EXACTLY the channel values listed above\n\
         - `process` returns `i32.const 0`\n\
         - `process_post` resets heap to 1024\n\
         You may use this exact known-good scaffold:\n\
         (module\n\
             (type (func (result i32)))\n\
             (type (func (param i32) (result i32)))\n\
             (type (func (param i32)))\n\
             (type (func (param i32 i32 i32 i32) (result i32)))\n\
             (type (func))\n\
             (import \"cm32p2|bedrock:controller/runtime@1\" \"current-execution-mode\" (func $current_execution_mode (type 0)))\n\
             (memory (export \"cm32p2_memory\") 1)\n\
             (global $heap (mut i32) (i32.const 1024))\n\
             (data (i32.const 0) \"{result_record}\")\n\
             (data (i32.const 64) \"{command_bytes}\")\n\
             (func (export \"cm32p2|bedrock:controller/control@1|process\") (type 1) (param $input i32) (result i32)\n\
                 (i32.const 0)\n\
             )\n\
             (func (export \"cm32p2|bedrock:controller/control@1|process_post\") (type 2) (param $result i32)\n\
                 (global.set $heap (i32.const 1024))\n\
             )\n\
             (func (export \"cm32p2_realloc\") (type 3) (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)\n\
                 (local $ptr i32)\n\
                 global.get $heap\n\
                 local.get $align\n\
                 i32.const 1\n\
                 i32.sub\n\
                 i32.add\n\
                 local.get $align\n\
                 i32.const 1\n\
                 i32.sub\n\
                 i32.const -1\n\
                 i32.xor\n\
                 i32.and\n\
                 local.tee $ptr\n\
                 local.get $new_size\n\
                 i32.add\n\
                 global.set $heap\n\
                 local.get $ptr\n\
             )\n\
             (func (export \"cm32p2_initialize\") (type 4))\n\
         )",
        count = command_values.len(),
    )
}

pub async fn generate_constant_wat_with_claude(
    api_key: &str,
    task_id: &str,
    control_manifest: &ControlInterfaceManifest,
    command_values: &[f64],
    user_message: &str,
) -> String {
    let model = roz_agent::model::create_model(
        "claude-sonnet-4-6",
        "",
        "",
        120,
        "anthropic",
        Some(api_key),
        &roz_core::auth::TenantId::new(uuid::Uuid::nil()),
        std::sync::Arc::new(roz_core::model_endpoint::EndpointRegistry::empty()),
    )
    .unwrap();
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, ToolDispatcher::new(Duration::from_secs(30)), safety, spatial);

    let input = AgentInput {
        task_id: task_id.into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec![constant_controller_prompt(control_manifest, command_values)],
            Vec::new(),
            user_message,
        ),
        max_cycles: 1,
        max_tokens: 4096,
        max_context_tokens: 100_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    let wat_response = output.final_response.as_deref().unwrap_or("");
    assert_live_controller_wat(wat_response);
    let wat_source = extract_wat_blob(wat_response).to_string();
    println!(
        "{task_id} Claude WAT preview: {}",
        wat_source.chars().take(200).collect::<String>()
    );
    wat_source
}

pub async fn promote_and_activate_live_controller(
    handle: &CopperHandle,
    task_id: &str,
    control_manifest: &ControlInterfaceManifest,
    wat_source: &str,
) {
    let embodiment_runtime = compile_test_embodiment_runtime(control_manifest);
    let (tool_cmd_tx, mut tool_cmd_rx) = tokio::sync::mpsc::channel(4);
    let mut tool_extensions = Extensions::new();
    tool_extensions.insert(tool_cmd_tx);
    tool_extensions.insert(control_manifest.clone());
    tool_extensions.insert(embodiment_runtime);
    let tool_ctx = ToolContext {
        task_id: task_id.into(),
        tenant_id: "test".into(),
        call_id: format!("promote-{task_id}"),
        extensions: tool_extensions,
    };
    let promote_tool = roz_local::tools::promote_controller::PromoteControllerTool::new(control_manifest);
    let promote_result = TypedToolExecutor::execute(
        &promote_tool,
        roz_local::tools::promote_controller::PromoteControllerInput {
            code: wat_source.to_string(),
        },
        &tool_ctx,
    )
    .await
    .unwrap();
    println!(
        "{task_id} promote result: {}",
        serde_json::to_string(&promote_result).unwrap_or_default()
    );
    assert!(
        promote_result.is_success(),
        "{task_id}: promote_controller failed: {}",
        promote_result
            .error
            .as_deref()
            .unwrap_or("unknown promote_controller failure")
    );

    let load_cmd: ControllerCommand = tool_cmd_rx
        .recv()
        .await
        .expect("promote_controller should emit a controller load command");
    load_cmd
        .clone()
        .into_runtime()
        .expect("promoted controller command should prepare successfully");
    handle
        .send(load_cmd)
        .await
        .expect("prepared load command should reach Copper");
    handle
        .send(ControllerCommand::PromoteActive)
        .await
        .expect("rollout authorization should reach Copper");
}

fn escape_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn wait_for_tcp_port(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("port {port} did not become reachable within {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
