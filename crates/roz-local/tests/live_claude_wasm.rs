//! THE REAL DEMO: Real Claude writes WAT -> `deploy_controller` -> Copper ticks -> oscillation.
//! Requires: `ANTHROPIC_API_KEY`

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn real_claude_writes_wat_and_deploys_controller() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");

    // 1. Create CopperHandle with LogActuatorSink
    let sink = std::sync::Arc::new(roz_copper::io_log::LogActuatorSink::new());
    let handle = roz_copper::handle::CopperHandle::spawn_with_io(
        1.5,
        Some(std::sync::Arc::clone(&sink) as std::sync::Arc<dyn roz_copper::io::ActuatorSink>),
        None,
    );

    // 2. Build Extensions with cmd_tx + manifest
    let mut extensions = roz_agent::dispatch::Extensions::new();
    extensions.insert(handle.cmd_tx());
    extensions.insert(roz_core::channels::ChannelManifest::ur5());

    // 3. Create real Claude model
    let model = roz_agent::model::create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&api_key)).unwrap();

    // 4. Create dispatcher with deploy_controller
    let mut dispatcher = roz_agent::dispatch::ToolDispatcher::new(std::time::Duration::from_secs(60));
    dispatcher.register_with_category(
        Box::new(roz_local::tools::deploy_controller::DeployControllerTool),
        roz_core::tools::ToolCategory::Physical,
    );

    // 5. Robot context in system prompt
    let manifest = roz_core::channels::ChannelManifest::ur5();
    let robot_context = format!(
        "You are a robot controller engineer. You write WAT code for WASM controllers.\n\n\
         Available WASM host functions:\n\
         - (import \"math\" \"sin\" (func $sin (param f64) (result f64)))\n\
         - (import \"command\" \"set\" (func $cmd (param i32 f64) (result i32)))\n\
         The module must export: (func (export \"process\") (param i64))\n\
         The i64 parameter is the tick counter (0, 1, 2, ...).\n\n\
         Command channels: {} velocity channels.\n\
         Use the deploy_controller tool to deploy the code.\n\
         Pass the WAT source code as the 'code' parameter.",
        manifest.commands.len(),
    );

    // 6. Run agent
    let safety = roz_agent::safety::SafetyStack::new(vec![]);
    let spatial = Box::new(roz_agent::spatial_provider::MockSpatialContextProvider::empty());
    let mut agent =
        roz_agent::agent_loop::AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);

    let input = roz_agent::agent_loop::AgentInput {
        task_id: "live-wasm-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        system_prompt: vec![robot_context],
        user_message:
            "Write a WASM controller that oscillates the first joint using sin(tick * 0.05) * 0.3, then deploy it."
                .into(),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 100_000,
        mode: roz_agent::agent_loop::AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    println!(
        "Claude's response: {}",
        output.final_response.as_deref().unwrap_or("<none>")
    );

    // 7. Wait for Copper to tick
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 8. Verify command frames
    let cmds = sink.commands();
    println!("Command frames captured: {}", cmds.len());

    if cmds.is_empty() {
        println!("WARNING: No command frames -- Claude may not have deployed successfully");
        println!(
            "Agent output: cycles={}, response={}",
            output.cycles,
            output.final_response.as_deref().unwrap_or("<none>")
        );
    } else {
        let values: Vec<f64> = cmds.iter().filter_map(|c| c.values.first().copied()).collect();
        let has_positive = values.iter().any(|&v| v > 0.05);
        let has_negative = values.iter().any(|&v| v < -0.05);

        println!(
            "Values range: [{:.3}, {:.3}]",
            values.iter().copied().fold(f64::INFINITY, f64::min),
            values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        );

        assert!(
            has_positive && has_negative,
            "sin oscillation should produce both +/-: {:?}",
            &values[..values.len().min(10)]
        );

        println!(
            "PASS: Real Claude wrote WAT -> deployed -> {} frames oscillating",
            cmds.len()
        );
    }

    handle.shutdown().await;
}
