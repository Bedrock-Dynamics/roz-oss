use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, ClientTlsConfig};

use crate::config::CliConfig;
use crate::tui::proto::roz_v1::{
    CreateScheduledTaskRequest, DeleteScheduledTaskRequest, ListScheduledTasksRequest, PreviewScheduleRequest,
    ScheduledTaskTemplate, task_service_client::TaskServiceClient,
};

type Bearer = tonic::metadata::MetadataValue<tonic::metadata::Ascii>;
type AuthedClient = TaskServiceClient<
    InterceptedService<Channel, Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send>>,
>;

#[derive(Debug, Args)]
pub struct ScheduleArgs {
    #[command(subcommand)]
    pub command: ScheduleCommands,
}

#[derive(Debug, Subcommand)]
pub enum ScheduleCommands {
    /// Preview the next five fire times for a natural-language schedule.
    Preview {
        /// Natural-language schedule, e.g. `every weekday at 9am Eastern`.
        schedule: String,
        /// IANA timezone name, e.g. `America/New_York`.
        #[arg(long)]
        timezone: String,
    },
    /// Create a scheduled task from a natural-language schedule and task template file.
    Create {
        /// Natural-language schedule, e.g. `every weekday at 9am Eastern`.
        schedule: String,
        /// Human-readable name for the scheduled task. Defaults to the NL schedule.
        #[arg(long)]
        name: Option<String>,
        /// IANA timezone name, e.g. `America/New_York`.
        #[arg(long)]
        timezone: String,
        /// YAML or JSON file describing the task template.
        #[arg(long = "task-template")]
        task_template: PathBuf,
        /// Catch-up behavior when the server misses one or more fire windows.
        #[arg(long, value_enum, default_value_t = CatchUpPolicyArg::RunLatest)]
        catch_up_policy: CatchUpPolicyArg,
        /// Persist the schedule disabled.
        #[arg(long)]
        disabled: bool,
    },
    /// List scheduled tasks.
    List {
        /// Page size to request from the server.
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Offset into the scheduled-task list.
        #[arg(long, default_value_t = 0)]
        offset: i64,
    },
    /// Show a single scheduled task by ID.
    Show {
        /// Scheduled task identifier.
        id: String,
    },
    /// Delete a scheduled task by ID.
    Delete {
        /// Scheduled task identifier.
        id: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CatchUpPolicyArg {
    SkipMissed,
    RunLatest,
    RunAll,
}

impl CatchUpPolicyArg {
    fn as_rpc(self) -> &'static str {
        match self {
            Self::SkipMissed => "skip_missed",
            Self::RunLatest => "run_latest",
            Self::RunAll => "run_all",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskTemplateFile {
    pub prompt: String,
    pub environment_id: String,
    pub host_id: String,
    #[serde(default)]
    pub timeout_secs: Option<u32>,
    #[serde(default)]
    pub control_interface_manifest: Option<serde_json::Value>,
    #[serde(default)]
    pub delegation_scope: Option<serde_json::Value>,
    #[serde(default)]
    pub phases: Vec<roz_core::phases::PhaseSpec>,
    #[serde(default)]
    pub parent_task_id: Option<String>,
}

impl TaskTemplateFile {
    fn into_proto(self) -> anyhow::Result<ScheduledTaskTemplate> {
        Ok(ScheduledTaskTemplate {
            prompt: self.prompt,
            environment_id: self.environment_id,
            host_id: self.host_id,
            timeout_secs: self.timeout_secs,
            control_interface_manifest: self.control_interface_manifest.and_then(json_to_prost_struct),
            delegation_scope: self.delegation_scope.and_then(json_to_prost_struct),
            phases: self
                .phases
                .into_iter()
                .map(serde_json::to_value)
                .map(|value| value.context("serialize phase spec"))
                .collect::<anyhow::Result<Vec<_>>>()?
                .into_iter()
                .filter_map(json_to_prost_struct)
                .collect(),
            parent_task_id: self.parent_task_id,
        })
    }
}

pub async fn execute(cmd: &ScheduleCommands, config: &CliConfig) -> anyhow::Result<()> {
    let (channel, bearer) = schedule_channel(config).await?;
    let mut client = build_client(channel, bearer);
    match cmd {
        ScheduleCommands::Preview { schedule, timezone } => preview_cmd(&mut client, schedule, timezone).await,
        ScheduleCommands::Create {
            schedule,
            name,
            timezone,
            task_template,
            catch_up_policy,
            disabled,
        } => {
            create_cmd(
                &mut client,
                name.as_deref(),
                schedule,
                timezone,
                task_template,
                *catch_up_policy,
                *disabled,
            )
            .await
        }
        ScheduleCommands::List { limit, offset } => list_cmd(&mut client, *limit, *offset).await,
        ScheduleCommands::Show { id } => show_cmd(&mut client, id).await,
        ScheduleCommands::Delete { id } => delete_cmd(&mut client, id).await,
    }
}

async fn schedule_channel(config: &CliConfig) -> anyhow::Result<(Channel, Bearer)> {
    let token_str = config
        .access_token
        .as_deref()
        .ok_or_else(|| anyhow!("No credentials. Run `roz auth login`."))?
        .to_string();

    let tls = ClientTlsConfig::new().with_native_roots();
    let channel = Channel::from_shared(config.api_url.clone())?
        .tls_config(tls)?
        .connect()
        .await?;
    let bearer: Bearer = format!("Bearer {token_str}").parse()?;
    Ok((channel, bearer))
}

fn build_client(channel: Channel, bearer: Bearer) -> AuthedClient {
    let interceptor: Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send> =
        Box::new(move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        });
    TaskServiceClient::with_interceptor(channel, interceptor)
}

pub fn build_preview_request(schedule: String, timezone: String) -> PreviewScheduleRequest {
    PreviewScheduleRequest {
        nl_schedule: Some(schedule),
        parsed_cron: None,
        timezone,
    }
}

pub fn build_create_request(
    name: Option<&str>,
    schedule: String,
    timezone: String,
    catch_up_policy: CatchUpPolicyArg,
    disabled: bool,
    task_template: ScheduledTaskTemplate,
) -> CreateScheduledTaskRequest {
    CreateScheduledTaskRequest {
        name: name.map(str::to_string).unwrap_or_else(|| schedule.clone()),
        nl_schedule: schedule,
        parsed_cron: String::new(),
        timezone,
        task_template: Some(task_template),
        enabled: !disabled,
        catch_up_policy: catch_up_policy.as_rpc().to_string(),
    }
}

pub fn load_task_template_file(path: &Path) -> anyhow::Result<ScheduledTaskTemplate> {
    let contents = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let parsed: TaskTemplateFile = if is_yaml_extension(path) {
        serde_yaml::from_str(&contents).with_context(|| format!("parse YAML {}", path.display()))?
    } else {
        serde_json::from_str(&contents).with_context(|| format!("parse JSON {}", path.display()))?
    };
    parsed.into_proto()
}

async fn preview_cmd(client: &mut AuthedClient, schedule: &str, timezone: &str) -> anyhow::Result<()> {
    let resp = client
        .preview_schedule(build_preview_request(schedule.to_string(), timezone.to_string()))
        .await
        .map_err(|status| anyhow!("gRPC {}: {}", status.code(), status.message()))?
        .into_inner();

    println!("Schedule: {}", resp.nl_schedule.unwrap_or_default());
    println!("Cron:     {}", resp.parsed_cron);
    println!("Timezone: {}", resp.timezone);
    for fire in resp.next_fires {
        println!(" - {}", fire.local_time);
    }
    Ok(())
}

async fn create_cmd(
    client: &mut AuthedClient,
    name: Option<&str>,
    schedule: &str,
    timezone: &str,
    task_template_path: &Path,
    catch_up_policy: CatchUpPolicyArg,
    disabled: bool,
) -> anyhow::Result<()> {
    let task_template = load_task_template_file(task_template_path)?;
    let resp = client
        .create_scheduled_task(build_create_request(
            name,
            schedule.to_string(),
            timezone.to_string(),
            catch_up_policy,
            disabled,
            task_template,
        ))
        .await
        .map_err(|status| anyhow!("gRPC {}: {}", status.code(), status.message()))?
        .into_inner();

    println!("Created scheduled task {}", resp.id);
    println!("Name:      {}", resp.name);
    println!("Schedule:  {}", resp.nl_schedule);
    println!("Cron:      {}", resp.parsed_cron);
    println!("Timezone:  {}", resp.timezone);
    println!("Enabled:   {}", resp.enabled);
    if let Some(next_fire_at) = resp.next_fire_at {
        println!("Next fire: {}.{:09} UTC", next_fire_at.seconds, next_fire_at.nanos);
    }
    Ok(())
}

async fn list_cmd(client: &mut AuthedClient, limit: i64, offset: i64) -> anyhow::Result<()> {
    let resp = client
        .list_scheduled_tasks(ListScheduledTasksRequest { limit, offset })
        .await
        .map_err(|status| anyhow!("gRPC {}: {}", status.code(), status.message()))?
        .into_inner();
    if resp.data.is_empty() {
        eprintln!("(no scheduled tasks found)");
        return Ok(());
    }
    println!("{:<38} {:<24} CRON", "ID", "NAME");
    for task in resp.data {
        println!("{:<38} {:<24} {}", task.id, task.name, task.parsed_cron);
    }
    Ok(())
}

async fn show_cmd(client: &mut AuthedClient, id: &str) -> anyhow::Result<()> {
    let resp = client
        .list_scheduled_tasks(ListScheduledTasksRequest { limit: 200, offset: 0 })
        .await
        .map_err(|status| anyhow!("gRPC {}: {}", status.code(), status.message()))?
        .into_inner();
    let task = resp
        .data
        .into_iter()
        .find(|task| task.id == id)
        .ok_or_else(|| anyhow!("scheduled task {id} not found"))?;

    println!("ID:         {}", task.id);
    println!("Name:       {}", task.name);
    println!("Schedule:   {}", task.nl_schedule);
    println!("Cron:       {}", task.parsed_cron);
    println!("Timezone:   {}", task.timezone);
    println!("Enabled:    {}", task.enabled);
    println!("Catch-up:   {}", task.catch_up_policy);
    if let Some(next_fire_at) = task.next_fire_at {
        println!("Next fire:  {}.{:09} UTC", next_fire_at.seconds, next_fire_at.nanos);
    }
    Ok(())
}

async fn delete_cmd(client: &mut AuthedClient, id: &str) -> anyhow::Result<()> {
    client
        .delete_scheduled_task(DeleteScheduledTaskRequest { id: id.to_string() })
        .await
        .map_err(|status| anyhow!("gRPC {}: {}", status.code(), status.message()))?;
    println!("Deleted scheduled task {id}");
    Ok(())
}

fn is_yaml_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
}

fn json_to_prost_struct(value: serde_json::Value) -> Option<prost_types::Struct> {
    let serde_json::Value::Object(map) = value else {
        return None;
    };
    Some(prost_types::Struct {
        fields: map
            .into_iter()
            .map(|(key, value)| (key, json_to_prost_value(value)))
            .collect::<BTreeMap<_, _>>(),
    })
}

fn json_to_prost_value(value: serde_json::Value) -> prost_types::Value {
    let kind = match value {
        serde_json::Value::Null => Some(prost_types::value::Kind::NullValue(0)),
        serde_json::Value::Bool(value) => Some(prost_types::value::Kind::BoolValue(value)),
        serde_json::Value::Number(value) => Some(prost_types::value::Kind::NumberValue(
            value.as_f64().unwrap_or_default(),
        )),
        serde_json::Value::String(value) => Some(prost_types::value::Kind::StringValue(value)),
        serde_json::Value::Array(values) => Some(prost_types::value::Kind::ListValue(prost_types::ListValue {
            values: values.into_iter().map(json_to_prost_value).collect(),
        })),
        serde_json::Value::Object(map) => Some(prost_types::value::Kind::StructValue(prost_types::Struct {
            fields: map
                .into_iter()
                .map(|(key, value)| (key, json_to_prost_value(value)))
                .collect::<BTreeMap<_, _>>(),
        })),
    };
    prost_types::Value { kind }
}
