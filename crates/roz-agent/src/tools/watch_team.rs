//! `watch_team` — Pure tool that polls pending team events from `JetStream` for the orchestrator.
//!
//! When called, this tool:
//! 1. Gets or creates a **durable** pull consumer named `watch-{parent_task_id}` on the
//!    `ROZ_TEAM_EVENTS` stream, filtered to `roz.team.{parent_task_id}.worker.>` (all
//!    workers for this orchestrator's team).  The consumer is created once with
//!    `DeliverPolicy::New` and then reused across all subsequent calls in the same session,
//!    so the model never sees the same event twice.
//! 2. Fetches up to `limit` messages with a short timeout (100 ms), returning quickly if
//!    no messages are pending.
//! 3. Deserializes each payload as [`roz_core::team::TeamEvent`], acks the message, and
//!    accumulates the events.
//! 4. Returns the events as a JSON array. Returns `[]` (not an error) when no events are
//!    pending — the model should simply check again later.
//!
//! This tool is **not** registered by default. The orchestrator session must register it
//! explicitly after constructing it with the required runtime handles.

use async_nats::jetstream::{Context as JetStreamContext, consumer::pull};
use async_trait::async_trait;
use futures::StreamExt as _;
use roz_core::team::{SequencedTeamEvent, TeamEvent};
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::dispatch::{ToolContext, TypedToolExecutor};

/// The canonical name of the watch-team tool.
pub const WATCH_TEAM_TOOL_NAME: &str = "watch_team";

/// Human-readable description returned by [`WatchTeamTool::description`].
pub(crate) const WATCH_TEAM_DESCRIPTION: &str = "Poll pending team events for the current worker team. Returns up to `limit` events \
     from the team event stream. Returns an empty array if no events are pending.";

/// Maximum number of events the caller may request in a single invocation.
const MAX_LIMIT: u32 = 50;

/// Short timeout used for the `JetStream` fetch so the tool returns quickly when
/// the queue is empty. `fetch()` (with `no_wait = true`) does not wait at all
/// — it returns whatever is available immediately — so this timeout is only a
/// safety bound in case the consumer setup itself is slow.
const FETCH_TIMEOUT_MS: u64 = 100;

// ---------------------------------------------------------------------------
// Input schema
// ---------------------------------------------------------------------------

const fn default_limit() -> u32 {
    10
}

/// Input for `watch_team`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WatchTeamInput {
    /// Maximum number of events to return (default 10, max 50).
    #[serde(default = "default_limit")]
    pub limit: u32,
}

// ---------------------------------------------------------------------------
// WatchTeamTool
// ---------------------------------------------------------------------------

/// An orchestrator tool that polls pending team events from `JetStream`.
///
/// Holds the runtime handles (NATS `JetStream` context, current orchestrator
/// task ID) that are not available at static/compile time — callers must
/// construct this explicitly and register it with the dispatcher when setting
/// up an orchestrator session.
pub struct WatchTeamTool {
    /// Active `JetStream` context.
    jetstream: JetStreamContext,
    /// This orchestrator's own task ID, used to filter the team event subject.
    parent_task_id: Uuid,
}

impl WatchTeamTool {
    /// Construct a new `WatchTeamTool`.
    ///
    /// # Arguments
    /// - `jetstream` — Active `JetStream` context.
    /// - `parent_task_id` — This orchestrator's own task ID. Events are
    ///   filtered to `roz.team.{parent_task_id}.worker.>`.
    #[must_use]
    pub const fn new(jetstream: JetStreamContext, parent_task_id: Uuid) -> Self {
        Self {
            jetstream,
            parent_task_id,
        }
    }
}

#[async_trait]
impl TypedToolExecutor for WatchTeamTool {
    type Input = WatchTeamInput;

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        WATCH_TEAM_TOOL_NAME
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        WATCH_TEAM_DESCRIPTION
    }

    async fn execute(
        &self,
        input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // Clamp limit to the allowed maximum.
        let limit = input.limit.min(MAX_LIMIT) as usize;

        // 1. Resolve the subject filter for this orchestrator's team.
        let filter_subject = roz_nats::team::team_subject_pattern(self.parent_task_id);

        // 2. Get the team event stream. If it does not exist yet, no events have
        //    been published — return an empty array rather than an error.
        let stream = match self.jetstream.get_stream(roz_nats::team::TEAM_STREAM).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    parent_task_id = %self.parent_task_id,
                    error = %e,
                    "watch_team: team event stream not found — no events published yet"
                );
                return Ok(ToolResult::success(json!([])));
            }
        };

        // 3. Create or resume a durable pull consumer filtered to this team's subjects.
        //    Using a durable named consumer ensures that across multiple `execute()` calls
        //    within the same session the delivery offset is preserved — the model never
        //    sees the same event twice.  `get_or_create_consumer` is idempotent: it
        //    returns the existing consumer (with its current ack floor) when one with
        //    the given name already exists, and creates a new one otherwise.
        //    `DeliverPolicy::New` means a brand-new consumer only sees events published
        //    *after* it was first created — no historical replay on session start.
        let consumer_name = format!("watch-{}", self.parent_task_id);
        let consumer = match stream
            .get_or_create_consumer::<pull::Config>(
                &consumer_name,
                pull::Config {
                    durable_name: Some(consumer_name.clone()),
                    filter_subject,
                    deliver_policy: async_nats::jetstream::consumer::DeliverPolicy::New,
                    ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    parent_task_id = %self.parent_task_id,
                    error = %e,
                    "watch_team: failed to get or create durable pull consumer"
                );
                return Ok(ToolResult::error(format!(
                    "watch_team: failed to get or create pull consumer: {e}"
                )));
            }
        };

        // 4. Fetch up to `limit` messages with `fetch()` (no_wait = true), so
        //    the call returns immediately with whatever is available.
        let mut messages = match consumer
            .fetch()
            .expires(std::time::Duration::from_millis(FETCH_TIMEOUT_MS))
            .max_messages(limit)
            .messages()
            .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    parent_task_id = %self.parent_task_id,
                    error = %e,
                    "watch_team: failed to open fetch stream"
                );
                return Ok(ToolResult::error(format!("watch_team: failed to fetch messages: {e}")));
            }
        };

        // 5. Collect, deserialize, and ack each message.
        let mut events: Vec<serde_json::Value> = Vec::with_capacity(limit);

        while let Some(msg_result) = messages.next().await {
            let msg = match msg_result {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        parent_task_id = %self.parent_task_id,
                        error = %e,
                        "watch_team: error receiving message, stopping fetch"
                    );
                    break;
                }
            };

            // Deserialize the payload (published as SequencedTeamEvent).
            let event: TeamEvent = match serde_json::from_slice::<SequencedTeamEvent>(&msg.payload) {
                Ok(seq_ev) => seq_ev.event,
                Err(e) => {
                    tracing::warn!(
                        parent_task_id = %self.parent_task_id,
                        error = %e,
                        "watch_team: failed to decode SequencedTeamEvent, acking and skipping"
                    );
                    // Ack bad messages so they don't block the consumer.
                    if let Err(ack_err) = msg.ack().await {
                        tracing::warn!(error = %ack_err, "watch_team: failed to ack malformed message");
                    }
                    continue;
                }
            };

            // Ack before accumulating — if serialisation below fails we've at
            // least advanced the consumer position.
            if let Err(ack_err) = msg.ack().await {
                tracing::warn!(error = %ack_err, "watch_team: failed to ack message");
            }

            // Serialize the TeamEvent as a JSON value for the tool result.
            match serde_json::to_value(&event) {
                Ok(v) => events.push(v),
                Err(e) => {
                    tracing::warn!(
                        parent_task_id = %self.parent_task_id,
                        error = %e,
                        "watch_team: failed to serialize TeamEvent, skipping"
                    );
                }
            }
        }

        // 6. Return the events array (possibly empty).
        Ok(ToolResult::success(json!(events)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // -----------------------------------------------------------------------
    // Schema helpers — derive schema directly from WatchTeamInput without
    // constructing a WatchTeamTool (which requires a live NATS connection).
    // -----------------------------------------------------------------------

    fn input_schema() -> serde_json::Value {
        let root: serde_json::Value = schemars::schema_for!(WatchTeamInput).into();
        let properties = root
            .get("properties")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let required = root
            .get("required")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    }

    // -----------------------------------------------------------------------
    // 1. Tool name constant
    // -----------------------------------------------------------------------

    #[test]
    fn watch_team_tool_name_constant() {
        assert_eq!(WATCH_TEAM_TOOL_NAME, "watch_team");
    }

    // -----------------------------------------------------------------------
    // 2. Input schema — limit field with default 10
    // -----------------------------------------------------------------------

    #[test]
    fn watch_team_input_schema_has_limit_field() {
        let schema = input_schema();
        let props = &schema["properties"];
        assert!(props["limit"].is_object(), "schema should have a 'limit' field");
        assert_eq!(
            props["limit"]["type"], "integer",
            "limit should be integer type, got: {}",
            props["limit"]["type"]
        );
    }

    #[test]
    fn watch_team_input_limit_is_optional() {
        let schema = input_schema();
        let required = schema["required"].as_array().expect("required should be an array");
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !required_strs.contains(&"limit"),
            "limit should be optional (has serde default), got required: {required_strs:?}"
        );
    }

    #[test]
    fn watch_team_input_default_limit_is_10() {
        // Verify the serde default gives 10 when limit is omitted.
        let input: WatchTeamInput = serde_json::from_value(json!({})).expect("should deserialise with no fields");
        assert_eq!(input.limit, 10, "default limit should be 10");
    }

    #[test]
    fn watch_team_input_explicit_limit_is_used() {
        let input: WatchTeamInput = serde_json::from_value(json!({"limit": 25})).expect("should deserialise");
        assert_eq!(input.limit, 25);
    }

    #[test]
    fn watch_team_input_limit_max_50_clamp() {
        // The MAX_LIMIT constant should be 50.
        assert_eq!(MAX_LIMIT, 50, "MAX_LIMIT should be 50");
    }

    // -----------------------------------------------------------------------
    // 3. Tool description contains "poll" and "team events"
    // -----------------------------------------------------------------------

    #[test]
    fn watch_team_description_mentions_poll_and_team_events() {
        assert!(
            WATCH_TEAM_DESCRIPTION.to_lowercase().contains("poll"),
            "description should mention 'poll', got: {WATCH_TEAM_DESCRIPTION}"
        );
        assert!(
            WATCH_TEAM_DESCRIPTION.to_lowercase().contains("team event"),
            "description should mention 'team event', got: {WATCH_TEAM_DESCRIPTION}"
        );
    }

    // -----------------------------------------------------------------------
    // 4. Execute with 0 events returns json!([])
    //    (live NATS required — marked #[ignore])
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires live NATS JetStream; \
                run: docker run -d -p 4222:4222 nats -js && cargo test ... -- --ignored"]
    async fn watch_team_execute_with_no_events_returns_empty_array() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
        let nats = async_nats::connect(&nats_url).await.expect("connect to NATS");
        let js = async_nats::jetstream::new(nats);

        // Create the stream so get_stream() succeeds.
        js.get_or_create_stream(async_nats::jetstream::stream::Config {
            name: roz_nats::team::TEAM_STREAM.to_string(),
            subjects: vec!["roz.team.>".to_string()],
            ..Default::default()
        })
        .await
        .expect("create team stream");

        let parent_task_id = Uuid::new_v4();
        let tool = WatchTeamTool::new(js, parent_task_id);
        let ctx = ToolContext {
            task_id: "test-task".to_string(),
            tenant_id: "test-tenant".to_string(),
            call_id: String::new(),
            extensions: crate::dispatch::Extensions::default(),
        };

        let result = crate::dispatch::TypedToolExecutor::execute(&tool, WatchTeamInput { limit: 10 }, &ctx)
            .await
            .expect("execute should not fail");

        assert!(result.is_success(), "should succeed even with no events");
        assert_eq!(
            result.output,
            json!([]),
            "should return empty array when no events pending, got: {}",
            result.output
        );
    }

    // -----------------------------------------------------------------------
    // 5. watch_team returns events that were previously published
    //    (requires live NATS — marked #[ignore])
    // -----------------------------------------------------------------------

    /// Run with:
    /// ```text
    /// NATS_URL=nats://localhost:4222 cargo test -p roz-agent watch_team_tool_returns_published_events -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "requires live NATS JetStream; \
                run: NATS_URL=nats://localhost:4222 cargo test -p roz-agent watch_team_tool_returns_published_events -- --ignored"]
    async fn watch_team_tool_returns_published_events() {
        use roz_core::team::TeamEvent;

        // 1. Connect to NATS (respects NATS_URL env var, defaults to localhost).
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
        let nats = async_nats::connect(&nats_url).await.expect("connect to NATS");
        let js = async_nats::jetstream::new(nats);

        // 2. Create (or get) the ROZ_TEAM_EVENTS stream.
        js.get_or_create_stream(async_nats::jetstream::stream::Config {
            name: roz_nats::team::TEAM_STREAM.to_string(),
            subjects: vec!["roz.team.>".to_string()],
            ..Default::default()
        })
        .await
        .expect("create team stream");

        // 3. Use fresh UUIDs so these events are isolated from other test runs.
        let parent_task_id = Uuid::new_v4();
        let child_task_id_1 = Uuid::new_v4();
        let child_task_id_2 = Uuid::new_v4();

        // 4. Create the tool and call execute() FIRST so the durable pull consumer is
        //    created with DeliverPolicy::New before any events are published.  Events
        //    published after this point will be visible to the consumer; events published
        //    before would be invisible (DeliverPolicy::New only delivers messages that
        //    arrive after the consumer is created).
        let tool = WatchTeamTool::new(js.clone(), parent_task_id);
        let ctx = ToolContext {
            task_id: parent_task_id.to_string(),
            tenant_id: "test-tenant".to_string(),
            call_id: "test-call-watch".to_string(),
            extensions: crate::dispatch::Extensions::default(),
        };

        let _empty = crate::dispatch::TypedToolExecutor::execute(&tool, WatchTeamInput { limit: 5 }, &ctx)
            .await
            .expect("first execute (consumer creation) should not fail");

        // 5. Now publish 2 TeamEvents — these arrive AFTER the consumer was created.
        let event_a = TeamEvent::WorkerStarted {
            worker_id: child_task_id_1,
            host_id: "host-alpha".to_string(),
        };
        let event_b = TeamEvent::WorkerCompleted {
            worker_id: child_task_id_2,
            result: "all done".to_string(),
        };

        roz_nats::team::publish_team_event(&js, parent_task_id, child_task_id_1, &event_a)
            .await
            .expect("publish event_a should succeed");
        roz_nats::team::publish_team_event(&js, parent_task_id, child_task_id_2, &event_b)
            .await
            .expect("publish event_b should succeed");

        // 6. Call execute() a second time — this is the production flow: the orchestrator
        //    calls watch_team periodically and events arrive between calls.
        let result = crate::dispatch::TypedToolExecutor::execute(&tool, WatchTeamInput { limit: 5 }, &ctx)
            .await
            .expect("second execute should not fail");

        assert!(result.is_success(), "watch_team should succeed, got: {}", result.output);

        // 7. Verify the result is a JSON array with exactly 2 events.
        let events = result.output.as_array().expect("output should be a JSON array");
        assert_eq!(
            events.len(),
            2,
            "should have received exactly 2 events, got {} events: {}",
            events.len(),
            result.output
        );

        // 8. Verify the event types match what was published.
        let event_types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
        assert!(
            event_types.contains(&"worker_started"),
            "expected worker_started event, got types: {event_types:?}"
        );
        assert!(
            event_types.contains(&"worker_completed"),
            "expected worker_completed event, got types: {event_types:?}"
        );
    }
}
