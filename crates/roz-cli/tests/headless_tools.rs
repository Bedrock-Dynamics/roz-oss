//! Headless integration tests for CLI client-side tool execution.
//!
//! These verify that tools from `robot.toml` are discovered, registered,
//! and their schemas are correctly built -- without requiring a live daemon
//! or cloud server.

use std::fs;

use roz_cli::tui::tools;
use tempfile::TempDir;

const REACHY_MINI_ROBOT_TOML: &str = r#"
[robot]
name = "reachy-mini"
description = "Pollen Robotics Reachy Mini -- 5-DoF expressive head + mobile base"

[channels]
robot_id = "reachy_mini"
robot_class = "expressive"
control_rate_hz = 50

[[channels.commands]]
name = "left_antenna"
type = "position"
unit = "rad"
limits = [-0.3, 0.3]

[[channels.commands]]
name = "right_antenna"
type = "position"
unit = "rad"
limits = [-0.3, 0.3]

[[channels.commands]]
name = "head_roll"
type = "position"
unit = "rad"
limits = [-0.26, 0.26]

[[channels.commands]]
name = "head_pitch"
type = "position"
unit = "rad"
limits = [-0.35, 0.17]

[[channels.commands]]
name = "head_yaw"
type = "position"
unit = "rad"
limits = [-1.13, 1.13]

[[channels.states]]
name = "left_antenna"
type = "position"
unit = "rad"
limits = [-0.3, 0.3]

[[channels.states]]
name = "right_antenna"
type = "position"
unit = "rad"
limits = [-0.3, 0.3]

[[channels.states]]
name = "head_roll"
type = "position"
unit = "rad"
limits = [-0.26, 0.26]

[[channels.states]]
name = "head_pitch"
type = "position"
unit = "rad"
limits = [-0.35, 0.17]

[[channels.states]]
name = "head_yaw"
type = "position"
unit = "rad"
limits = [-1.13, 1.13]

[daemon]
base_url = "http://localhost:8000"

[daemon.get_state]
method = "GET"
path = "/api/state/full"

[daemon.set_motors]
method = "POST"
path = "/api/motors/set_mode/{{mode}}"

[daemon.move_to]
method = "POST"
path = "/api/move/goto"
body = '{"left_antenna": {{left_antenna}}, "right_antenna": {{right_antenna}}, "roll": {{head_roll}}, "pitch": {{head_pitch}}, "yaw": {{head_yaw}}, "duration": {{duration}}}'

[daemon.play_animation]
method = "POST"
path_prefix = "/api/move/play"
available_moves = ["wake_up", "goto_sleep"]

[daemon.stop_motion]
method = "POST"
path = "/api/motors/set_mode/disabled"
"#;

// ---------------------------------------------------------------------------
// Tool discovery (unified: builtins + daemon)
// ---------------------------------------------------------------------------

#[test]
fn discovers_all_tools_from_robot_toml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    let names: Vec<&str> = schemas.iter().map(|(s, _)| s.name.as_str()).collect();

    // CLI built-ins
    assert!(names.contains(&"bash"), "should have bash");
    assert!(names.contains(&"read_file"), "should have read_file");
    assert!(names.contains(&"write_file"), "should have write_file");
    assert!(names.contains(&"list_files"), "should have list_files");
    assert!(names.contains(&"search"), "should have search");
    assert!(names.contains(&"execute_code"), "should have execute_code");

    // Daemon tools
    assert!(names.contains(&"get_robot_state"), "should have get_robot_state");
    assert!(names.contains(&"set_motors"), "should have set_motors");
    assert!(names.contains(&"move_to"), "should have move_to");
    assert!(names.contains(&"play_animation"), "should have play_animation");
}

#[test]
fn builtins_only_without_robot_toml() {
    let dir = TempDir::new().unwrap();
    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    assert_eq!(
        schemas.len(),
        6,
        "should have exactly 6 CLI built-in tools (with categories)"
    );
}

#[test]
fn builtins_only_without_daemon_section() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("robot.toml"),
        "[robot]\nname = \"test\"\ndescription = \"no daemon\"\n",
    )
    .unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    assert_eq!(
        schemas.len(),
        6,
        "should have only CLI built-ins (with categories) when [daemon] is missing"
    );
}

// ---------------------------------------------------------------------------
// Schema generation
// ---------------------------------------------------------------------------

#[test]
fn builds_tool_schemas_from_robot_toml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    let names: Vec<&str> = schemas.iter().map(|(s, _)| s.name.as_str()).collect();

    assert!(names.contains(&"get_robot_state"));
    assert!(names.contains(&"set_motors"));
    assert!(names.contains(&"move_to"));
    assert!(names.contains(&"play_animation"));
}

#[test]
fn move_to_schema_has_channel_properties() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    let move_to = schemas
        .iter()
        .map(|(s, _)| s)
        .find(|s| s.name == "move_to")
        .expect("move_to schema should exist");

    let props = move_to.parameters["properties"]
        .as_object()
        .expect("move_to should have properties");

    // Verify all command channels appear as properties
    assert!(props.contains_key("left_antenna"));
    assert!(props.contains_key("right_antenna"));
    assert!(props.contains_key("head_roll"));
    assert!(props.contains_key("head_pitch"));
    assert!(props.contains_key("head_yaw"));
    assert!(props.contains_key("duration_secs"));

    // Verify channel property has type and limit description
    let pitch = &props["head_pitch"];
    assert_eq!(pitch["type"], "number");
    let desc = pitch["description"].as_str().expect("should have description");
    assert!(desc.contains("rad"), "should mention unit");
    assert!(desc.contains("-0.350"), "should mention lower limit");
    assert!(desc.contains("0.170"), "should mention upper limit");

    // duration_secs is required
    let required = move_to.parameters["required"]
        .as_array()
        .expect("should have required array");
    assert!(
        required.iter().filter_map(|v| v.as_str()).any(|n| n == "duration_secs"),
        "duration_secs should be required"
    );
}

#[test]
fn set_motors_schema_has_mode_parameter() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    let set_motors = schemas
        .iter()
        .map(|(s, _)| s)
        .find(|s| s.name == "set_motors")
        .expect("set_motors should exist");

    let props = set_motors.parameters["properties"]
        .as_object()
        .expect("set_motors should have properties");
    assert!(props.contains_key("mode"), "set_motors should have mode parameter");
}

#[test]
fn play_animation_schema_mentions_available_moves() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    let play = schemas
        .iter()
        .map(|(s, _)| s)
        .find(|s| s.name == "play_animation")
        .expect("play_animation should exist");

    let props = play.parameters["properties"]
        .as_object()
        .expect("play_animation should have properties");
    let name_desc = props["name"]["description"]
        .as_str()
        .expect("name param should have description");
    assert!(
        name_desc.contains("wake_up"),
        "description should list available moves, got: {name_desc}"
    );
    assert!(
        name_desc.contains("goto_sleep"),
        "description should list available moves, got: {name_desc}"
    );
}

#[test]
fn schemas_include_builtins_for_missing_robot_toml() {
    let dir = TempDir::new().unwrap();
    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    assert_eq!(schemas.len(), 6, "should have 6 CLI built-in schemas (with categories)");
}

// ---------------------------------------------------------------------------
// Proto schema conversion
// ---------------------------------------------------------------------------

#[test]
fn proto_schema_conversion_roundtrips() {
    use roz_cli::tui::convert::{struct_to_value, value_to_struct};

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());
    for (schema, _category) in &schemas {
        // Convert to proto struct and back
        let prost_struct = value_to_struct(schema.parameters.clone());
        let roundtripped = struct_to_value(prost_struct);

        // The "type" field should survive the roundtrip
        assert_eq!(
            roundtripped["type"], "object",
            "parameters type should roundtrip for tool: {}",
            schema.name
        );

        // Properties should be present
        assert!(
            roundtripped.get("properties").is_some(),
            "properties should roundtrip for tool: {}",
            schema.name
        );
    }
}

// ---------------------------------------------------------------------------
// Tool category correctness (would have caught hardcoded Physical bug)
// ---------------------------------------------------------------------------

/// Verify that `get_robot_state` is Pure and actuation tools are Physical.
///
/// This test would have caught the bug where `core_schema_to_proto` hardcoded
/// `ToolCategoryPhysical` for every tool -- `get_robot_state`, `read_file`,
/// `list_files`, and `search` should be `Pure`.
#[test]
fn tool_categories_are_correct_for_all_tools() {
    use roz_core::tools::ToolCategory;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());

    for (schema, category) in &schemas {
        match schema.name.as_str() {
            // Pure tools: read-only / no physical side effects
            "get_robot_state" | "read_file" | "list_files" | "search" => {
                assert_eq!(
                    *category,
                    ToolCategory::Pure,
                    "{} should be Pure, got Physical",
                    schema.name
                );
            }
            // Physical tools: actuation / real-world side effects
            "bash" | "write_file" | "execute_code" | "move_to" | "set_motors" | "play_animation" => {
                assert_eq!(
                    *category,
                    ToolCategory::Physical,
                    "{} should be Physical, got Pure",
                    schema.name
                );
            }
            other => panic!("unexpected tool in schema list: {other}"),
        }
    }
}

/// Verify that `schemas_with_categories` on the dispatcher matches `build_all_tools`.
///
/// This ensures the proto conversion path (which uses the category from
/// `build_all_tools`) will produce correct `ToolCategoryHint` values.
#[test]
fn dispatcher_categories_match_schema_categories() {
    use roz_core::tools::ToolCategory;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (dispatcher, schemas) = tools::build_all_tools(dir.path());

    // Every schema's category should match what the dispatcher reports
    for (schema, category) in &schemas {
        let dispatcher_category = dispatcher.category(&schema.name);
        assert_eq!(
            *category, dispatcher_category,
            "category mismatch for '{}': build_all_tools says {:?}, dispatcher says {:?}",
            schema.name, category, dispatcher_category
        );
    }

    // Spot-check: get_robot_state must be Pure, move_to must be Physical
    assert_eq!(dispatcher.category("get_robot_state"), ToolCategory::Pure);
    assert_eq!(dispatcher.category("move_to"), ToolCategory::Physical);
}

/// Verify that all Physical-category tools would be logged (not just bash/write_file/execute_code).
///
/// This test documents the set of Physical tools so any future additions
/// are caught and verified.
#[test]
fn physical_tool_set_matches_expectations() {
    use roz_core::tools::ToolCategory;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (_dispatcher, schemas) = tools::build_all_tools(dir.path());

    let physical_names: Vec<&str> = schemas
        .iter()
        .filter(|(_, cat)| *cat == ToolCategory::Physical)
        .map(|(s, _)| s.name.as_str())
        .collect();

    // All actuation/destructive tools must be Physical
    assert!(physical_names.contains(&"bash"), "bash should be Physical");
    assert!(physical_names.contains(&"write_file"), "write_file should be Physical");
    assert!(
        physical_names.contains(&"execute_code"),
        "execute_code should be Physical"
    );
    assert!(physical_names.contains(&"move_to"), "move_to should be Physical");
    assert!(physical_names.contains(&"set_motors"), "set_motors should be Physical");
    assert!(
        physical_names.contains(&"play_animation"),
        "play_animation should be Physical"
    );

    // Pure tools must NOT be in the Physical set
    assert!(
        !physical_names.contains(&"get_robot_state"),
        "get_robot_state should NOT be Physical"
    );
    assert!(
        !physical_names.contains(&"read_file"),
        "read_file should NOT be Physical"
    );
    assert!(
        !physical_names.contains(&"list_files"),
        "list_files should NOT be Physical"
    );
    assert!(!physical_names.contains(&"search"), "search should NOT be Physical");
}

// ---------------------------------------------------------------------------
// Tool execution against real daemon (requires external service)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Reachy Mini daemon on localhost:8000"]
async fn get_robot_state_executes_against_daemon() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("robot.toml"), REACHY_MINI_ROBOT_TOML).unwrap();

    let (dispatcher, _schemas) = tools::build_all_tools(dir.path());

    let ctx = tools::default_context();
    let call = roz_core::tools::ToolCall {
        id: "test-call".into(),
        tool: "get_robot_state".into(),
        params: serde_json::json!({}),
    };

    let result = dispatcher.dispatch(&call, &ctx).await;
    assert!(result.is_success(), "get_robot_state should succeed: {result:?}");

    // The output should be valid JSON
    let output = &result.output;
    assert!(output.is_object(), "get_robot_state should return a JSON object");
}
