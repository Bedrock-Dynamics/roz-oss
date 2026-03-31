//! Local tool dispatcher for client-side tool execution in cloud mode.
//!
//! When the server sends a `ToolRequest`, the CLI executes the tool locally
//! (where hardware daemons are reachable) and sends the result back via
//! `ToolResult`. Tools from `robot.toml`'s `[daemon]` section are discovered,
//! registered with the cloud session, and dispatched to the local daemon.

use std::path::Path;
use std::time::Duration;

use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher};
use roz_core::manifest::RobotManifest;
use roz_core::tools::ToolSchema;

/// Build a local `ToolDispatcher` from `robot.toml` daemon config.
///
/// Returns `None` if no `robot.toml` exists or it has no `[daemon]` section.
pub fn build_local_dispatcher(project_dir: &Path) -> Option<ToolDispatcher> {
    let robot_toml = project_dir.join("robot.toml");
    let manifest = RobotManifest::load(&robot_toml).ok()?;
    let daemon = manifest.daemon.as_ref()?;
    let channels = manifest.channel_manifest();
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    for (tool, category) in roz_local::tools::daemon::daemon_tools(daemon, channels.as_ref()) {
        dispatcher.register_with_category(tool, category);
    }
    Some(dispatcher)
}

/// Build tool schemas for registering with the cloud server.
///
/// Returns an empty vec if no `robot.toml` or no `[daemon]` section.
pub fn build_tool_schemas(project_dir: &Path) -> Vec<ToolSchema> {
    let robot_toml = project_dir.join("robot.toml");
    let Some(manifest) = RobotManifest::load(&robot_toml).ok() else {
        return vec![];
    };
    let Some(daemon) = manifest.daemon.as_ref() else {
        return vec![];
    };
    let channels = manifest.channel_manifest();
    let tools = roz_local::tools::daemon::daemon_tools(daemon, channels.as_ref());
    tools.into_iter().map(|(t, _)| t.schema()).collect()
}

/// Default `ToolContext` for local execution (no tenant/task context).
pub fn default_context() -> ToolContext {
    ToolContext {
        task_id: "local".into(),
        tenant_id: "local".into(),
        call_id: String::new(),
        extensions: Extensions::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const MINIMAL_ROBOT_TOML: &str = r#"
[robot]
name = "test-bot"
description = "A test robot"

[daemon]
base_url = "http://localhost:8000"

[daemon.get_state]
method = "GET"
path = "/api/state/full"

[daemon.set_motors]
method = "POST"
path = "/api/motors/set_mode/{{mode}}"

[daemon.play_animation]
method = "POST"
path_prefix = "/api/move/play"
available_moves = ["wake_up", "goto_sleep"]
"#;

    const ROBOT_TOML_WITH_CHANNELS: &str = r#"
[robot]
name = "test-bot"
description = "A test robot"

[channels]
robot_id = "test"
robot_class = "expressive"
control_rate_hz = 50

[[channels.commands]]
name = "head/pitch"
type = "position"
unit = "rad"
limits = [-0.35, 0.17]

[[channels.states]]
name = "head/pitch"
type = "position"
unit = "rad"
limits = [-0.35, 0.17]

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
body = '{"pitch": {{head/pitch}}, "duration": {{duration}}}'

[daemon.play_animation]
method = "POST"
path_prefix = "/api/move/play"
available_moves = ["wake_up", "goto_sleep"]
"#;

    #[test]
    fn build_dispatcher_from_robot_toml() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("robot.toml"), MINIMAL_ROBOT_TOML).unwrap();

        let dispatcher = build_local_dispatcher(dir.path()).unwrap();
        let schemas = dispatcher.schemas();
        assert!(schemas.iter().any(|s| s.name == "get_robot_state"));
        assert!(schemas.iter().any(|s| s.name == "set_motors"));
        assert!(schemas.iter().any(|s| s.name == "play_animation"));
    }

    #[test]
    fn build_dispatcher_none_when_no_robot_toml() {
        let dir = TempDir::new().unwrap();
        assert!(build_local_dispatcher(dir.path()).is_none());
    }

    #[test]
    fn build_dispatcher_none_when_no_daemon_section() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("robot.toml"),
            "[robot]\nname = \"test\"\ndescription = \"test\"\n",
        )
        .unwrap();
        assert!(build_local_dispatcher(dir.path()).is_none());
    }

    #[test]
    fn build_schemas_from_robot_toml() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("robot.toml"), MINIMAL_ROBOT_TOML).unwrap();

        let schemas = build_tool_schemas(dir.path());
        assert!(schemas.iter().any(|s| s.name == "get_robot_state"));
        assert!(schemas.iter().any(|s| s.name == "set_motors"));
        assert!(schemas.iter().any(|s| s.name == "play_animation"));
        // No move_to without channels section
        assert!(!schemas.iter().any(|s| s.name == "move_to"));
    }

    #[test]
    fn build_schemas_includes_move_to_with_channels() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("robot.toml"), ROBOT_TOML_WITH_CHANNELS).unwrap();

        let schemas = build_tool_schemas(dir.path());
        assert!(
            schemas.iter().any(|s| s.name == "move_to"),
            "move_to should be present when channels are defined"
        );
        assert_eq!(schemas.len(), 4, "get_state + set_motors + move_to + play_animation");

        // Verify move_to schema has channel properties
        let move_to = schemas.iter().find(|s| s.name == "move_to").unwrap();
        let props = move_to.parameters["properties"].as_object().unwrap();
        assert!(
            props.contains_key("head/pitch"),
            "move_to should have head/pitch property"
        );
        assert!(
            props.contains_key("duration_secs"),
            "move_to should have duration_secs property"
        );
    }

    #[test]
    fn build_schemas_empty_when_no_robot_toml() {
        let dir = TempDir::new().unwrap();
        assert!(build_tool_schemas(dir.path()).is_empty());
    }

    #[test]
    fn default_context_has_local_ids() {
        let ctx = default_context();
        assert_eq!(ctx.task_id, "local");
        assert_eq!(ctx.tenant_id, "local");
    }
}
