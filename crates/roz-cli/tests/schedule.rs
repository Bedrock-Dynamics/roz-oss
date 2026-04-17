use std::fs;

use clap::Parser;
use tempfile::TempDir;

use roz_cli::cli::{Cli, Commands};
use roz_cli::commands::schedule::{
    CatchUpPolicyArg, build_create_request, build_preview_request, load_task_template_file,
};
use roz_cli::tui::proto::roz_v1::ScheduledTaskTemplate;

fn write_template_file(contents: &str, extension: &str) -> std::path::PathBuf {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join(format!("task-template.{extension}"));
    fs::write(&path, contents).expect("write template file");
    let leaked_dir = dir.keep();
    leaked_dir.join(format!("task-template.{extension}"))
}

#[test]
fn parse_schedule_subcommands() {
    let preview = Cli::parse_from([
        "roz",
        "schedule",
        "preview",
        "every weekday at 9am Eastern",
        "--timezone",
        "America/New_York",
    ]);
    assert!(matches!(preview.command, Some(Commands::Schedule(_))));

    let create = Cli::parse_from([
        "roz",
        "schedule",
        "create",
        "every 15 minutes",
        "--timezone",
        "UTC",
        "--task-template",
        "task.yaml",
    ]);
    assert!(matches!(create.command, Some(Commands::Schedule(_))));

    let list = Cli::parse_from(["roz", "schedule", "list", "--limit", "10"]);
    assert!(matches!(list.command, Some(Commands::Schedule(_))));

    let show = Cli::parse_from(["roz", "schedule", "show", "sched-123"]);
    assert!(matches!(show.command, Some(Commands::Schedule(_))));

    let delete = Cli::parse_from(["roz", "schedule", "delete", "sched-123"]);
    assert!(matches!(delete.command, Some(Commands::Schedule(_))));
}

#[test]
fn schedule_preview_and_create_requests_match_proto_contract() {
    let path = write_template_file(
        r"
prompt: run diagnostics
environment_id: 11111111-1111-1111-1111-111111111111
host_id: 22222222-2222-2222-2222-222222222222
timeout_secs: 300
phases: []
",
        "yaml",
    );
    let task_template: ScheduledTaskTemplate = load_task_template_file(&path).expect("load task template");
    let preview = build_preview_request("every weekday at 9am Eastern".into(), "America/New_York".into());
    let create = build_create_request(
        Some("ops-daily"),
        "every weekday at 9am Eastern".into(),
        "America/New_York".into(),
        CatchUpPolicyArg::RunLatest,
        false,
        task_template,
    );

    assert_eq!(preview.nl_schedule.as_deref(), Some("every weekday at 9am Eastern"));
    assert_eq!(preview.parsed_cron, None);
    assert_eq!(preview.timezone, "America/New_York");

    assert_eq!(create.name, "ops-daily");
    assert_eq!(create.nl_schedule, "every weekday at 9am Eastern");
    assert!(
        create.parsed_cron.is_empty(),
        "CLI should let the server stay authoritative"
    );
    assert_eq!(create.timezone, "America/New_York");
    assert_eq!(create.catch_up_policy, "run_latest");
    assert!(create.enabled);
    assert_eq!(
        create.task_template.as_ref().expect("task template").host_id,
        "22222222-2222-2222-2222-222222222222"
    );
}

#[test]
fn schedule_command_never_references_legacy_trigger_rest_path() {
    let source = fs::read_to_string(format!("{}/src/commands/schedule.rs", env!("CARGO_MANIFEST_DIR")))
        .expect("read schedule command source");

    assert!(
        source.contains("TaskServiceClient"),
        "schedule command must use TaskService gRPC"
    );
    assert!(
        !source.contains("/v1/triggers"),
        "schedule command must not fall back to the legacy trigger REST path"
    );
}
