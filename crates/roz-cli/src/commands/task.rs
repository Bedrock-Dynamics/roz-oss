use std::path::Path;
use std::time::Duration;

use clap::{Args, Subcommand};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite;

use crate::config::CliConfig;

/// Task management commands.
#[derive(Debug, Args)]
pub struct TaskArgs {
    /// The task subcommand to execute.
    #[command(subcommand)]
    pub command: TaskCommands,
}

/// Available task subcommands.
#[derive(Debug, Subcommand)]
pub enum TaskCommands {
    /// Run a task from a spec file or inline definition.
    Run {
        /// Path to the task spec or inline task definition.
        spec: String,
        /// Follow task output in real time.
        #[arg(short, long)]
        follow: bool,
        /// Task phases as JSON (e.g., '[{"mode":"react","tools":"all","trigger":"immediate"}]').
        #[arg(long)]
        phases: Option<String>,
    },
    /// List all tasks.
    List,
    /// Show status of a specific task.
    Status {
        /// Task identifier.
        id: String,
    },
    /// Watch a task in real time.
    Watch {
        /// Task identifier.
        id: String,
    },
    /// Wait for a task to complete.
    Wait {
        /// Task identifier.
        id: String,
        /// Timeout in seconds.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Cancel a running task.
    Cancel {
        /// Task identifier.
        id: String,
    },
    /// View task logs.
    Logs {
        /// Task identifier.
        id: String,
        /// Follow log output in real time.
        #[arg(short, long)]
        follow: bool,
    },
}

/// Execute a task subcommand.
pub async fn execute(cmd: &TaskCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        TaskCommands::Run { spec, follow, phases } => run(config, spec, *follow, phases.as_deref()).await,
        TaskCommands::List => list(config).await,
        TaskCommands::Status { id } => status(config, id).await,
        TaskCommands::Watch { id } => watch(config, id).await,
        TaskCommands::Wait { id, timeout } => wait(config, id, *timeout).await,
        TaskCommands::Cancel { id } => cancel(config, id).await,
        TaskCommands::Logs { id, follow } => logs(config, id, *follow).await,
    }
}

/// Returns true if the file extension (case-insensitive) is YAML.
fn is_yaml_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
}

async fn run(config: &CliConfig, spec: &str, follow: bool, phases: Option<&str>) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let spec_path = Path::new(spec);

    // Try to read spec as a file first, then treat as inline JSON.
    let mut body: serde_json::Value = if spec_path.exists() {
        let contents = std::fs::read_to_string(spec)?;
        if is_yaml_extension(spec_path) {
            serde_yaml::from_str(&contents)?
        } else {
            serde_json::from_str(&contents)?
        }
    } else {
        serde_json::from_str(spec)?
    };

    // Merge --phases into the request body if provided.
    if let Some(phases_json) = phases {
        let parsed: Vec<roz_core::phases::PhaseSpec> =
            serde_json::from_str(phases_json).map_err(|e| anyhow::anyhow!("invalid --phases JSON: {e}"))?;
        body["phases"] = serde_json::to_value(&parsed)?;
    }

    let resp: serde_json::Value = client
        .post(format!("{}/v1/tasks", config.api_url))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if follow && let Some(task_id) = resp["data"]["id"].as_str().or_else(|| resp["id"].as_str()) {
        eprintln!("Task created: {task_id}");
        return watch(config, task_id).await;
    }

    crate::output::render_json(&resp)?;
    Ok(())
}

async fn list(config: &CliConfig) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/tasks", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn status(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/tasks/{id}", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

/// Derive the WebSocket URL from the HTTP API URL.
fn ws_url(config: &CliConfig) -> String {
    config
        .api_url
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1)
}

/// Build the full WebSocket connection URL with auth token as query param.
fn ws_connect_url(config: &CliConfig) -> anyhow::Result<String> {
    let base = ws_url(config);
    let token = config
        .access_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("not authenticated — run `roz auth login` first"))?;
    Ok(format!("{base}/v1/ws?token={token}&vsn=2.0.0"))
}

/// Send a Phoenix v2 message: `[join_ref, ref, topic, event, payload]`
async fn send_phoenix<S>(
    sink: &mut S,
    join_ref: Option<&str>,
    msg_ref: &str,
    topic: &str,
    event: &str,
    payload: serde_json::Value,
) -> anyhow::Result<()>
where
    S: SinkExt<tungstenite::Message, Error = tungstenite::Error> + Unpin,
{
    let msg = json!([join_ref, msg_ref, topic, event, payload]);
    sink.send(tungstenite::Message::Text(msg.to_string().into())).await?;
    Ok(())
}

/// Connect to WebSocket and stream task events.
///
/// Exit codes per design spec:
/// - 0: Task completed successfully
/// - 1: Task failed
/// - 3: Connection lost
async fn watch(config: &CliConfig, task_id: &str) -> anyhow::Result<()> {
    let url = ws_connect_url(config)?;
    let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
    let (mut write, mut read) = ws.split();

    let topic = format!("task:{task_id}");

    // Join the task channel
    send_phoenix(&mut write, Some("1"), "1", &topic, "phx_join", json!({})).await?;

    let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(30));
    let mut hb_ref: u64 = 100;

    // Exit codes per design spec: 0=success, 1=failed, 3=connection lost
    let exit_code: i32;

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(tungstenite::Message::Text(text))) => {
                        let arr: Vec<serde_json::Value> = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if arr.len() < 5 {
                            continue;
                        }
                        let event = arr[3].as_str().unwrap_or_default();
                        let payload = &arr[4];

                        match event {
                            "status" => {
                                let state = payload["status"].as_str().unwrap_or("unknown");
                                eprintln!("[status] {state}");
                            }
                            "log" => {
                                let line = payload["text"].as_str().unwrap_or("");
                                print!("{line}");
                            }
                            "thinking" => {
                                let delta = payload["delta"].as_str().unwrap_or("");
                                eprint!("{delta}");
                            }
                            "tool_call" => {
                                let tool = payload["tool"].as_str().unwrap_or("unknown");
                                eprintln!("[tool] {tool}");
                            }
                            "complete" => {
                                let reason = payload["reason"].as_str().unwrap_or("done");
                                eprintln!("[complete] {reason}");
                                exit_code = 0;
                                break;
                            }
                            "error" => {
                                let err_msg = payload["message"].as_str().unwrap_or("unknown error");
                                eprintln!("[error] {err_msg}");
                                exit_code = 1;
                                break;
                            }
                            "phx_reply" | "phx_close" => {}
                            _ => {
                                eprintln!("[{event}] {payload}");
                            }
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) | None => {
                        eprintln!("[connection lost]");
                        exit_code = 3;
                        break;
                    }
                    _ => {}
                }
            }
            _ = heartbeat_interval.tick() => {
                hb_ref += 1;
                let ref_str = hb_ref.to_string();
                if send_phoenix(&mut write, None, &ref_str, "phoenix", "heartbeat", json!({})).await.is_err() {
                    eprintln!("[connection lost]");
                    exit_code = 3;
                    break;
                }
            }
        }
    }

    // Leave channel before disconnecting
    let _ = send_phoenix(&mut write, Some("1"), "999", &topic, "phx_leave", json!({})).await;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// Terminal task states that indicate completion.
const TERMINAL_STATES: &[&str] = &["completed", "failed", "cancelled", "error"];

async fn wait(config: &CliConfig, id: &str, timeout: Option<u64>) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let poll_interval = Duration::from_secs(2);
    let deadline = timeout.map(|secs| std::time::Instant::now() + Duration::from_secs(secs));

    let spinner = indicatif::ProgressBar::new_spinner();
    spinner.set_message(format!("Waiting for task {id}..."));
    spinner.enable_steady_tick(Duration::from_millis(100));

    loop {
        let resp: serde_json::Value = client
            .get(format!("{}/v1/tasks/{id}", config.api_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if let Some(state) = resp["status"].as_str().or_else(|| resp["state"].as_str())
            && TERMINAL_STATES.contains(&state)
        {
            spinner.finish_with_message(format!("Task {id}: {state}"));
            crate::output::render_json(&resp)?;
            return Ok(());
        }

        if let Some(dl) = deadline
            && std::time::Instant::now() >= dl
        {
            spinner.finish_with_message("timed out");
            anyhow::bail!("Timed out waiting for task {id}");
        }

        tokio::time::sleep(poll_interval).await;
    }
}

async fn cancel(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    client
        .delete(format!("{}/v1/tasks/{id}", config.api_url))
        .send()
        .await?
        .error_for_status()?;
    eprintln!("Cancelled task {id}");
    Ok(())
}

async fn logs(config: &CliConfig, id: &str, follow: bool) -> anyhow::Result<()> {
    // Always fetch existing logs first
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/tasks/{id}/logs", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;

    if !follow {
        return Ok(());
    }

    // Stream new log lines via WebSocket
    let url = ws_connect_url(config)?;
    let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
    let (mut write, mut read) = ws.split();

    let topic = format!("task:{id}");
    send_phoenix(&mut write, Some("1"), "1", &topic, "phx_join", json!({})).await?;

    let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(30));
    let mut hb_ref: u64 = 100;

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(tungstenite::Message::Text(text))) => {
                        let arr: Vec<serde_json::Value> = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if arr.len() < 5 {
                            continue;
                        }
                        let event = arr[3].as_str().unwrap_or_default();
                        let payload = &arr[4];

                        match event {
                            "log" => {
                                let text = payload["text"].as_str().unwrap_or("");
                                print!("{text}");
                            }
                            "complete" | "error" => break,
                            _ => {}
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            _ = heartbeat_interval.tick() => {
                hb_ref += 1;
                let ref_str = hb_ref.to_string();
                if send_phoenix(&mut write, None, &ref_str, "phoenix", "heartbeat", json!({})).await.is_err() {
                    break;
                }
            }
        }
    }

    let _ = send_phoenix(&mut write, Some("1"), "999", &topic, "phx_leave", json!({})).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};

    #[test]
    fn parse_phases_json() {
        let json = r#"[
            {"mode":"react","tools":"all","trigger":"immediate"},
            {"mode":"ooda_re_act","tools":{"named":["goto"]},"trigger":{"after_cycles":5}}
        ]"#;
        let phases: Vec<PhaseSpec> = serde_json::from_str(json).unwrap();
        assert_eq!(phases.len(), 2);
        assert!(matches!(phases[0].mode, PhaseMode::React));
        assert!(matches!(phases[0].tools, ToolSetFilter::All));
        assert!(matches!(phases[0].trigger, PhaseTrigger::Immediate));
        assert!(matches!(phases[1].mode, PhaseMode::OodaReAct));
        assert!(matches!(phases[1].tools, ToolSetFilter::Named(ref names) if names == &["goto"]));
        assert!(matches!(phases[1].trigger, PhaseTrigger::AfterCycles(5)));
    }

    #[test]
    fn parse_phases_invalid_json_fails() {
        let bad = r#"[{"mode":"invalid"}]"#;
        let result: Result<Vec<PhaseSpec>, _> = serde_json::from_str(bad);
        assert!(result.is_err());
    }

    #[test]
    fn parse_phases_single_phase() {
        let json = r#"[{"mode":"ooda_re_act","tools":"all","trigger":"immediate"}]"#;
        let phases: Vec<PhaseSpec> = serde_json::from_str(json).unwrap();
        assert_eq!(phases.len(), 1);
        assert!(matches!(phases[0].mode, PhaseMode::OodaReAct));
    }

    #[test]
    fn parse_phases_with_named_tools() {
        let json = r#"[{"mode":"react","tools":{"named":["goto","arm_move"]},"trigger":"on_tool_signal"}]"#;
        let phases: Vec<PhaseSpec> = serde_json::from_str(json).unwrap();
        assert!(matches!(phases[0].tools, ToolSetFilter::Named(ref tools) if tools.len() == 2));
    }
}
