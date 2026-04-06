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
    /// Run a task from a spec file or inline definition. Use global `--host` or include `host_id` in the spec. If host names collide, pass the host UUID.
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
pub async fn execute(cmd: &TaskCommands, config: &CliConfig, host_flag: Option<&str>) -> anyhow::Result<()> {
    match cmd {
        TaskCommands::Run { spec, follow, phases } => run(config, spec, *follow, phases.as_deref(), host_flag).await,
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

fn task_body_object_mut(
    body: &mut serde_json::Value,
) -> anyhow::Result<&mut serde_json::Map<String, serde_json::Value>> {
    body.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("task spec must deserialize to a JSON/YAML object"))
}

fn body_host_id(body: &serde_json::Value) -> Option<&str> {
    body.get("host_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

async fn ensure_task_host_id<F, Fut>(
    body: &mut serde_json::Value,
    host_flag: Option<&str>,
    mut resolve_host_id: F,
) -> anyhow::Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    if body_host_id(body).is_some() {
        return Ok(());
    }

    let host = host_flag.ok_or_else(|| {
        anyhow::anyhow!("task creation requires a target host; pass global `--host <name-or-uuid>` or include `host_id` in the task spec")
    })?;
    let resolved = resolve_host_id(host.to_string()).await?;
    task_body_object_mut(body)?.insert("host_id".to_string(), serde_json::Value::String(resolved));
    Ok(())
}

fn task_status_from_response(resp: &serde_json::Value) -> Option<&str> {
    resp.get("data")
        .and_then(|data| data.get("status"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| resp.get("status").and_then(serde_json::Value::as_str))
        .or_else(|| resp.get("state").and_then(serde_json::Value::as_str))
}

async fn run(
    config: &CliConfig,
    spec: &str,
    follow: bool,
    phases: Option<&str>,
    host_flag: Option<&str>,
) -> anyhow::Result<()> {
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
        task_body_object_mut(&mut body)?.insert("phases".to_string(), serde_json::to_value(&parsed)?);
    }

    let resolve_client = client.clone();
    let resolve_api_url = config.api_url.clone();
    ensure_task_host_id(&mut body, host_flag, move |host| {
        let client = resolve_client.clone();
        let api_url = resolve_api_url.clone();
        async move { super::estop::resolve_host_id(&client, &api_url, &host).await }
    })
    .await?;

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
const TERMINAL_STATES: &[&str] = &["succeeded", "failed", "timed_out", "cancelled", "safety_stop"];

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

        if let Some(state) = task_status_from_response(&resp)
            && TERMINAL_STATES.contains(&state)
        {
            spinner.finish_with_message(format!("Task {id}: {state}"));
            crate::output::render_json(&resp)?;
            if state == "succeeded" {
                return Ok(());
            }
            anyhow::bail!("Task {id} reached terminal state: {state}");
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
    use std::sync::{Arc, Mutex};

    use super::*;
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        body: String,
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> anyhow::Result<RecordedRequest> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];

        let header_end = loop {
            let bytes_read = stream.read(&mut chunk).await?;
            anyhow::ensure!(bytes_read > 0, "connection closed before request headers were received");
            buffer.extend_from_slice(&chunk[..bytes_read]);

            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };

        let headers = std::str::from_utf8(&buffer[..header_end])?;
        let request_line = headers
            .lines()
            .next()
            .ok_or_else(|| anyhow::anyhow!("request line missing"))?;
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("request method missing"))?
            .to_string();
        let path = request_parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("request path missing"))?
            .to_string();

        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);

        while buffer.len() - header_end < content_length {
            let bytes_read = stream.read(&mut chunk).await?;
            anyhow::ensure!(
                bytes_read > 0,
                "connection closed before request body was fully received"
            );
            buffer.extend_from_slice(&chunk[..bytes_read]);
        }

        let body = String::from_utf8(buffer[header_end..header_end + content_length].to_vec())?;
        Ok(RecordedRequest { method, path, body })
    }

    async fn spawn_task_api(
        hosts_response: serde_json::Value,
        task_response: serde_json::Value,
    ) -> anyhow::Result<(
        String,
        Arc<Mutex<Vec<RecordedRequest>>>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded_requests = Arc::clone(&requests);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept_result = listener.accept() => {
                        let (mut stream, _) = match accept_result {
                            Ok(pair) => pair,
                            Err(_) => break,
                        };

                        let request = match read_http_request(&mut stream).await {
                            Ok(request) => request,
                            Err(_) => continue,
                        };

                        recorded_requests.lock().expect("request log mutex poisoned").push(request.clone());

                        let (status_line, response_body) = if request.path.starts_with("/v1/hosts?") {
                            ("200 OK", hosts_response.to_string())
                        } else if request.method == "POST" && request.path == "/v1/tasks" {
                            ("200 OK", task_response.to_string())
                        } else {
                            ("404 Not Found", serde_json::json!({"error": "not found"}).to_string())
                        };

                        let response = format!(
                            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            response_body.as_bytes().len(),
                            response_body
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    }
                }
            }
        });

        Ok((format!("http://{address}"), requests, shutdown_tx, server))
    }

    fn test_cli_config(api_url: String) -> CliConfig {
        CliConfig {
            api_url,
            profile: "default".to_string(),
            access_token: None,
        }
    }

    #[test]
    fn task_status_from_response_prefers_nested_data_status() {
        let resp = serde_json::json!({
            "data": { "status": "succeeded" },
            "status": "failed"
        });
        assert_eq!(task_status_from_response(&resp), Some("succeeded"));
    }

    #[test]
    fn terminal_states_match_current_task_model() {
        assert!(TERMINAL_STATES.contains(&"succeeded"));
        assert!(TERMINAL_STATES.contains(&"timed_out"));
        assert!(TERMINAL_STATES.contains(&"safety_stop"));
        assert!(!TERMINAL_STATES.contains(&"completed"));
    }

    #[tokio::test]
    async fn ensure_task_host_id_injects_resolved_host_when_missing() {
        let mut body = serde_json::json!({
            "prompt": "scan bay",
            "environment_id": "env-1"
        });
        ensure_task_host_id(&mut body, Some("robot-alpha"), |host| async move {
            assert_eq!(host, "robot-alpha");
            Ok("00000000-0000-0000-0000-000000000123".to_string())
        })
        .await
        .expect("host should resolve");
        assert_eq!(
            body.get("host_id").and_then(serde_json::Value::as_str),
            Some("00000000-0000-0000-0000-000000000123")
        );
    }

    #[tokio::test]
    async fn ensure_task_host_id_preserves_explicit_host_id() {
        let mut body = serde_json::json!({
            "prompt": "scan bay",
            "environment_id": "env-1",
            "host_id": "existing-host-id"
        });
        ensure_task_host_id(&mut body, Some("robot-alpha"), |_host| async move {
            anyhow::bail!("resolver should not be called when host_id is already present")
        })
        .await
        .expect("existing host_id should win");
        assert_eq!(
            body.get("host_id").and_then(serde_json::Value::as_str),
            Some("existing-host-id")
        );
    }

    #[tokio::test]
    async fn ensure_task_host_id_requires_host_target() {
        let mut body = serde_json::json!({
            "prompt": "scan bay",
            "environment_id": "env-1"
        });
        let error = ensure_task_host_id(&mut body, None, |_host| async move { Ok(String::new()) })
            .await
            .expect_err("missing host target should fail");
        assert!(error.to_string().contains("requires a target host"));
    }

    #[tokio::test]
    async fn ensure_task_host_id_propagates_duplicate_host_errors() {
        let mut body = serde_json::json!({
            "prompt": "scan bay",
            "environment_id": "env-1"
        });
        let error = ensure_task_host_id(&mut body, Some("reachy-dev"), |_host| async move {
            anyhow::bail!("host name 'reachy-dev' is ambiguous")
        })
        .await
        .expect_err("duplicate host names should fail locally");
        assert!(error.to_string().contains("ambiguous"));
        assert!(body.get("host_id").is_none());
    }

    #[tokio::test]
    async fn run_rejects_ambiguous_host_names_before_creating_task() {
        let (api_url, requests, shutdown_tx, server) = spawn_task_api(
            serde_json::json!({
                "data": [
                    {
                        "id": "00000000-0000-0000-0000-000000000111",
                        "name": "reachy-dev",
                        "status": "online"
                    },
                    {
                        "id": "00000000-0000-0000-0000-000000000222",
                        "name": "reachy-dev",
                        "status": "offline"
                    }
                ]
            }),
            serde_json::json!({"data": {"id": "task-1"}}),
        )
        .await
        .expect("test API should start");

        let config = test_cli_config(api_url);
        let error = run(
            &config,
            r#"{"prompt":"scan bay","environment_id":"env-1"}"#,
            false,
            None,
            Some("reachy-dev"),
        )
        .await
        .expect_err("duplicate host names should fail before task creation");

        let _ = shutdown_tx.send(());
        server.await.expect("server task should stop cleanly");

        assert!(error.to_string().contains("ambiguous"));
        let requests = requests.lock().expect("request log mutex poisoned");
        assert_eq!(
            requests.len(),
            1,
            "ambiguous host resolution should stop before task creation"
        );
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].path.starts_with("/v1/hosts?"));
    }

    #[tokio::test]
    async fn run_injects_resolved_host_id_into_task_creation_request() {
        let resolved_host_id = "00000000-0000-0000-0000-000000000123";
        let (api_url, requests, shutdown_tx, server) = spawn_task_api(
            serde_json::json!({
                "data": [
                    {
                        "id": resolved_host_id,
                        "name": "reachy-dev",
                        "status": "online"
                    }
                ]
            }),
            serde_json::json!({"data": {"id": "task-1"}}),
        )
        .await
        .expect("test API should start");

        let config = test_cli_config(api_url);
        run(
            &config,
            r#"{"prompt":"scan bay","environment_id":"env-1"}"#,
            false,
            None,
            Some("reachy-dev"),
        )
        .await
        .expect("task creation should succeed");

        let _ = shutdown_tx.send(());
        server.await.expect("server task should stop cleanly");

        let requests = requests.lock().expect("request log mutex poisoned");
        assert_eq!(
            requests.len(),
            2,
            "unique host resolution should query hosts and then create the task"
        );
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/v1/tasks");

        let posted_body: serde_json::Value =
            serde_json::from_str(&requests[1].body).expect("task create request body should be valid JSON");
        assert_eq!(
            posted_body.get("host_id").and_then(serde_json::Value::as_str),
            Some(resolved_host_id)
        );
        assert_eq!(
            posted_body.get("prompt").and_then(serde_json::Value::as_str),
            Some("scan bay")
        );
    }

    #[tokio::test]
    async fn run_accepts_host_uuid_without_querying_host_list() {
        let host_id = "00000000-0000-0000-0000-000000000123";
        let (api_url, requests, shutdown_tx, server) = spawn_task_api(
            serde_json::json!({"data": []}),
            serde_json::json!({"data": {"id": "task-1"}}),
        )
        .await
        .expect("test API should start");

        let config = test_cli_config(api_url);
        run(
            &config,
            r#"{"prompt":"scan bay","environment_id":"env-1"}"#,
            false,
            None,
            Some(host_id),
        )
        .await
        .expect("task creation should succeed");

        let _ = shutdown_tx.send(());
        server.await.expect("server task should stop cleanly");

        let requests = requests.lock().expect("request log mutex poisoned");
        assert_eq!(
            requests.len(),
            1,
            "UUID host targets should bypass host list resolution"
        );
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/v1/tasks");

        let posted_body: serde_json::Value =
            serde_json::from_str(&requests[0].body).expect("task create request body should be valid JSON");
        assert_eq!(
            posted_body.get("host_id").and_then(serde_json::Value::as_str),
            Some(host_id)
        );
    }

    #[test]
    fn parse_phases_json() {
        let json = r#"[
            {"mode":"react","tools":"all","trigger":"immediate"},
            {"mode":"ooda_react","tools":{"named":["goto"]},"trigger":{"after_cycles":5}}
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
        let json = r#"[{"mode":"ooda_react","tools":"all","trigger":"immediate"}]"#;
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
