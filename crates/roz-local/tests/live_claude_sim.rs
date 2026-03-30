//! Live test: real Claude + MCP tools + Docker sim.
//! Requires: `ANTHROPIC_API_KEY` + ros2-manipulator container on port 8094

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + running Docker sim"]
async fn real_claude_moves_arm_via_mcp() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");

    // 1. Connect MCP to manipulator container
    let mcp = std::sync::Arc::new(roz_local::mcp::McpManager::new());
    match mcp.connect("arm", 8094, std::time::Duration::from_secs(15)).await {
        Ok(_) => {}
        Err(e) => {
            eprintln!("SKIP: MCP connect failed: {e}");
            return;
        }
    }

    // 2. Create real Claude model
    let model = roz_agent::model::create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&api_key)).unwrap();

    // 3. Create dispatcher with MCP tools
    let mut dispatcher = roz_agent::dispatch::ToolDispatcher::new(std::time::Duration::from_secs(30));
    for tool_info in mcp.all_tools() {
        dispatcher.register_with_category(
            Box::new(roz_local::mcp::McpToolExecutor::new(
                std::sync::Arc::clone(&mcp),
                tool_info.clone(),
            )),
            tool_info.category,
        );
    }

    // 4. Run agent
    let safety = roz_agent::safety::SafetyStack::new(vec![]);
    let spatial = Box::new(roz_agent::spatial_provider::MockSpatialContextProvider::empty());
    let mut agent = roz_agent::agent_loop::AgentLoop::new(model, dispatcher, safety, spatial);

    let input = roz_agent::agent_loop::AgentInput {
        task_id: "live-mcp-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        system_prompt: vec![
            "You are controlling a UR5 robot arm via MCP tools. Use the available tools to move the arm.".into(),
        ],
        user_message: "Move the arm to the home position.".into(),
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
    println!("Cycles: {}, Tokens: {:?}", output.cycles, output.total_usage);

    // Verify the agent used MCP tools
    assert!(output.cycles > 1, "should have used tools (cycles > 1)");
    println!("PASS: Real Claude used MCP tools ({} cycles)", output.cycles);
}
