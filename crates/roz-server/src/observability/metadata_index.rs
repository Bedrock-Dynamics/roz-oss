//! Phase 26.4: Session metadata + tool-call indexing from MCAP archives.
//!
//! Public entry point: [`index_session`] reads every rollover MCAP archive
//! for a `(tenant_id, session_id)` pair, decodes `SessionEventEnvelope`
//! messages on `/roz/session/events`, correlates the
//! `ToolCallRequested → ToolCallStarted → ToolCallFinished` triplet via
//! an in-memory `HashMap<call_id, PartialToolCall>` (CONTEXT D-02), and
//! upserts one summary row into `roz_session_metadata` + N tool-call rows
//! into `roz_session_tool_calls` inside a single Postgres transaction
//! (D-06).
//!
//! Fire-and-forget: callers from `WriterActor::finalize_file` spawn this
//! function detached (`tokio::spawn`) and log any `Err` — never propagate.
//!
//! # Chunk offset resolution (CONTEXT D-03, RESEARCH Blocker 1 Option A)
//!
//! `mcap::Message` exposes no chunk identifier, so we pre-read
//! `mcap::read::Summary::read(&data)` once per rollover file and build a
//! sorted `Vec<(message_start_time_ns, message_end_time_ns,
//! chunk_start_offset)>` from `summary.chunk_indexes`. For every
//! `ToolCallFinished` (or `ToolCallStarted` fallback) we binary-search the
//! vec by the message's `log_time` via `Vec::partition_point` and store
//! the covering chunk's `chunk_start_offset` on the `PartialToolCall`.
//! Substrate drill-down reads the chunk (bounded size) and walks forward
//! to the matching `call_id`.

#![allow(
    clippy::result_large_err,
    reason = "MetadataIndexError carries a sqlx::Error; boxing loses chain context"
)]

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use mcap::read::Summary;
use serde::Serialize;
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::observability::CHANNEL_SESSION_EVENTS;

/// Summary returned by [`index_session`] for caller-side logs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IndexSummary {
    pub session_id: Uuid,
    pub tool_calls_indexed: u32,
    pub turn_count: u32,
    pub outcome: &'static str,
    pub rollover_files_read: u32,
}

/// Distinct error surface for the indexer. Kept separate from
/// [`crate::observability::McapArchiveError`] so the error vocabularies
/// stay narrow per boundary.
#[derive(Debug, thiserror::Error)]
pub enum MetadataIndexError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("mcap read: {0}")]
    Mcap(#[from] mcap::McapError),
    #[error("prost decode on {variant}: {source}")]
    Decode {
        variant: &'static str,
        #[source]
        source: prost::DecodeError,
    },
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("archive not found for session {0}")]
    ArchiveNotFound(Uuid),
}

/// Partial tool-call row accumulated across the Requested/Started/Finished triplet.
/// Finalized into a [`roz_db::session_metadata::ToolCallRow`] at stream end.
#[derive(Debug, Default, Clone)]
pub(crate) struct PartialToolCall {
    pub tool_name: Option<String>,
    pub category: Option<String>,
    pub requested_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub had_approval: bool,
    /// Set when a ToolUnavailable event in the same turn window paired with
    /// this tool_name — drives `outcome='failed'` (CONTEXT D-09).
    pub unavailable_paired: bool,
    pub trace_id: Option<Vec<u8>>,
    pub mcap_chunk_offset: Option<u64>,
    pub rollover_index: i32,
    /// Turn index at the time the first Requested or Started landed —
    /// used to match ToolUnavailable by (turn_index, tool_name).
    pub turn_index: Option<u32>,
}

/// Aggregate counters accumulated across one session's rollover files.
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionCounters {
    pub turn_count: u32,
    pub approval_count: u32,
    pub intervention_count: u32,
    // DEFERRED SafetyViolation (2026-04-23 discuss-phase, CONTEXT.md Deferred Ideas):
    // violation_count stays at 0 this phase — SessionEventEnvelope.typed_event oneof
    // has no SafetyViolation variant (proto gap). Unlock: add proto variant + reindex.
    pub violation_count: u32,
    pub model_ids: std::collections::BTreeSet<String>,
    // DEFERRED SafetyViolation: policy_ids stays empty this phase — same proto gap
    // as violation_count above. See CONTEXT.md Deferred Ideas for unlock path.
    pub policy_ids: std::collections::BTreeSet<String>,
    pub controller_artifact_ids: std::collections::BTreeSet<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: &'static str,
    pub error_summary: Option<String>,
    pub first_trace_id: Option<Vec<u8>>,
    pub current_turn_index: u32,
    /// Set of (turn_index, tool_name) tuples that saw a ToolUnavailable in
    /// that turn — checked at PartialToolCall finalization time.
    pub unavailable_in_turn: HashSet<(u32, String)>,
}

impl SessionCounters {
    pub(crate) fn new() -> Self {
        Self {
            outcome: "abandoned", // overwritten by terminal event
            ..Self::default()
        }
    }
}

/// Resolve the `chunk_start_offset` for a message whose `log_time` is `ts`.
/// Returns `None` if `ts` falls before the first chunk or in an inter-chunk
/// gap (malformed archive). The `ranges` slice MUST be sorted ascending by
/// `message_start_time` — the caller is responsible for that (see
/// `build_chunk_time_index` which does the sort).
pub(crate) fn lookup_chunk_offset(ranges: &[(u64, u64, u64)], ts: u64) -> Option<u64> {
    if ranges.is_empty() {
        return None;
    }
    // Rightmost chunk whose message_start_time <= ts.
    let pos = ranges.partition_point(|(start, _, _)| *start <= ts);
    if pos == 0 {
        return None;
    }
    let (_, end, offset) = ranges[pos - 1];
    if ts <= end { Some(offset) } else { None }
}

/// Build the sorted chunk-time-range index from an MCAP summary. Applied
/// once per rollover file before the message iteration.
fn build_chunk_time_index(summary: &Summary) -> Vec<(u64, u64, u64)> {
    let mut ranges: Vec<(u64, u64, u64)> = summary
        .chunk_indexes
        .iter()
        .map(|ci| (ci.message_start_time, ci.message_end_time, ci.chunk_start_offset))
        .collect();
    // Defense-in-depth against malformed files (Assumption A1 safety).
    ranges.sort_by_key(|(start, _, _)| *start);
    ranges
}

impl PartialToolCall {
    /// Derive the tool-call outcome per CONTEXT D-09.
    /// * `succeeded` — finished_at present, no paired ToolUnavailable.
    /// * `failed`    — finished_at present AND paired ToolUnavailable in same turn.
    /// * `unfinished`— finished_at absent (session aborted mid-tool).
    pub(crate) fn derive_outcome(&self) -> &'static str {
        match (self.finished_at.is_some(), self.unavailable_paired) {
            (true, false) => "succeeded",
            (true, true) => "failed",
            (false, _) => "unfinished",
        }
    }
}

fn proto_ts_to_chrono(ts: Option<&prost_types::Timestamp>) -> Option<DateTime<Utc>> {
    let ts = ts?;
    let nanos = u32::try_from(ts.nanos).ok()?;
    DateTime::<Utc>::from_timestamp(ts.seconds, nanos)
}

/// Index a single session's MCAP archives into `roz_session_metadata` and
/// `roz_session_tool_calls`. Idempotent via `ON CONFLICT DO UPDATE` in the
/// DB helpers.
///
/// `tenant_id` is the archive-owning tenant; the caller MUST verify this
/// matches the invoker's authority (gRPC handler path) or pass the
/// `WriterActor`'s `self.tenant_id` (spawn-site path).
///
/// # Errors
/// * `ArchiveNotFound` — no rollover rows for this session.
/// * `Io`/`Mcap`       — file-read or MCAP-decode failure on ANY rollover file.
///   Partial progress is discarded (single transaction is rolled back).
/// * `Decode`          — SessionEventEnvelope failed to decode; we bail
///   rather than upsert partial data.
/// * `Sqlx`            — transaction commit failed.
///
/// # Cancellation safety
/// If the future is dropped, the sqlx transaction rolls back automatically;
/// the filesystem reads are side-effect-free.
pub async fn index_session(
    pool: &PgPool,
    tenant_id: Uuid,
    session_id: Uuid,
) -> Result<IndexSummary, MetadataIndexError> {
    let archives = roz_db::mcap_archives::list_by_session(pool, tenant_id, session_id).await?;
    if archives.is_empty() {
        return Err(MetadataIndexError::ArchiveNotFound(session_id));
    }

    let mut tool_calls: HashMap<String, PartialToolCall> = HashMap::new();
    let mut counters = SessionCounters::new();
    let mut rollover_files_read: u32 = 0;

    for archive in &archives {
        index_one_rollover_file(archive, &mut tool_calls, &mut counters)?;
        rollover_files_read += 1;
    }

    // Apply turn-window ToolUnavailable pairing (CONTEXT D-09, RESEARCH Pitfall 4).
    for partial in tool_calls.values_mut() {
        if let (Some(turn_idx), Some(name)) = (partial.turn_index, partial.tool_name.as_ref())
            && counters.unavailable_in_turn.contains(&(turn_idx, name.clone()))
        {
            partial.unavailable_paired = true;
        }
    }

    // Derive duration_ms. `chrono::Duration::num_milliseconds` returns `i64`.
    let duration_ms = match (counters.started_at, counters.ended_at) {
        (Some(s), Some(e)) => Some((e - s).num_milliseconds()),
        _ => None,
    };

    // Finalize SessionMetadataRow.
    let metadata_row = roz_db::session_metadata::SessionMetadataRow {
        session_id,
        tenant_id,
        started_at: counters.started_at.unwrap_or_else(Utc::now),
        ended_at: counters.ended_at,
        duration_ms,
        turn_count: i32::try_from(counters.turn_count).unwrap_or(i32::MAX),
        tool_call_count: i32::try_from(tool_calls.len()).unwrap_or(i32::MAX),
        approval_count: i32::try_from(counters.approval_count).unwrap_or(i32::MAX),
        intervention_count: i32::try_from(counters.intervention_count).unwrap_or(i32::MAX),
        violation_count: i32::try_from(counters.violation_count).unwrap_or(i32::MAX),
        model_ids: counters.model_ids.iter().cloned().collect(),
        policy_ids: counters.policy_ids.iter().cloned().collect(),
        controller_artifact_ids: counters.controller_artifact_ids.iter().cloned().collect(),
        first_trace_id: counters.first_trace_id.clone(),
        outcome: counters.outcome.to_string(),
        error_summary: counters.error_summary.clone(),
        indexed_at: Utc::now(), // server-side now() overrides this
    };

    // Finalize ToolCallRow batch.
    let tool_rows: Vec<roz_db::session_metadata::ToolCallRow> = tool_calls
        .into_iter()
        .map(|(call_id, p)| {
            let outcome = p.derive_outcome();
            let latency_ms = match (p.started_at, p.finished_at) {
                (Some(s), Some(e)) => Some((e - s).num_milliseconds()),
                _ => None,
            };
            let mcap_offset = p.mcap_chunk_offset.and_then(|v| i64::try_from(v).ok());
            roz_db::session_metadata::ToolCallRow {
                session_id,
                call_id,
                tenant_id,
                tool_name: p.tool_name.unwrap_or_default(),
                category: p.category,
                requested_at: p.requested_at.or(p.started_at).unwrap_or_else(Utc::now),
                finished_at: p.finished_at,
                latency_ms,
                had_approval: p.had_approval,
                outcome: outcome.to_string(),
                trace_id: p.trace_id,
                mcap_offset,
                rollover_index: p.rollover_index,
            }
        })
        .collect();
    let tool_calls_indexed = u32::try_from(tool_rows.len()).unwrap_or(u32::MAX);

    // Single transaction per D-06.
    let mut tx = pool.begin().await?;
    let _ = roz_db::session_metadata::upsert_metadata(&mut *tx, &metadata_row).await?;
    let _ = roz_db::session_metadata::upsert_tool_calls_batch(&mut *tx, &tool_rows).await?;
    tx.commit().await?;

    info!(
        %session_id,
        %tenant_id,
        tool_calls_indexed,
        turn_count = counters.turn_count,
        outcome = counters.outcome,
        rollover_files_read,
        "session metadata indexed"
    );

    Ok(IndexSummary {
        session_id,
        tool_calls_indexed,
        turn_count: counters.turn_count,
        outcome: counters.outcome,
        rollover_files_read,
    })
}

fn index_one_rollover_file(
    archive: &roz_db::mcap_archives::McapArchiveRow,
    tool_calls: &mut HashMap<String, PartialToolCall>,
    counters: &mut SessionCounters,
) -> Result<(), MetadataIndexError> {
    use crate::grpc::roz_v1;
    use prost::Message as _;

    let data = std::fs::read(&archive.path)?;

    // Pre-read summary for chunk-offset lookup (Blocker 1 Option A).
    // clippy::option_if_let_else is silenced here — the `else` branch emits a
    // structured `warn!` log and returns a fallback Vec, which reads cleaner
    // as an explicit `if let` than as a `map_or_else` closure.
    #[expect(
        clippy::option_if_let_else,
        reason = "else branch has side-effect logging; if-let is clearer than map_or_else"
    )]
    let chunk_ranges: Vec<(u64, u64, u64)> = if let Some(summary) = Summary::read(&data)? {
        build_chunk_time_index(&summary)
    } else {
        warn!(
            path = %archive.path,
            rollover_index = archive.rollover_index,
            "MCAP summary section absent; tool_call mcap_offset values will be NULL for this file"
        );
        Vec::new()
    };

    for msg in mcap::MessageStream::new(&data)? {
        let msg = msg?;
        if msg.channel.topic.as_str() != CHANNEL_SESSION_EVENTS {
            continue;
        }
        let env =
            roz_v1::SessionEventEnvelope::decode(msg.data.as_ref()).map_err(|source| MetadataIndexError::Decode {
                variant: "SessionEventEnvelope",
                source,
            })?;

        // Capture first_trace_id on the first 16-byte trace_id we see.
        if counters.first_trace_id.is_none() && env.trace_id.len() == 16 {
            counters.first_trace_id = Some(env.trace_id.clone());
        } else if !env.trace_id.is_empty() && env.trace_id.len() != 16 {
            warn!(len = env.trace_id.len(), "unexpected trace_id length; ignoring");
        }

        let ts = proto_ts_to_chrono(env.timestamp.as_ref());
        let trace_id = if env.trace_id.len() == 16 {
            Some(env.trace_id.clone())
        } else {
            None
        };
        let chunk_offset = lookup_chunk_offset(&chunk_ranges, msg.log_time);

        use roz_v1::session_event_envelope::TypedEvent as T;
        match env.typed_event {
            Some(T::SessionStarted(_)) => {
                if counters.started_at.is_none() {
                    counters.started_at = ts;
                }
            }
            Some(T::SessionCompleted(_)) => {
                counters.ended_at = ts.or(counters.ended_at);
                counters.outcome = "succeeded";
            }
            Some(T::SessionFailed(p)) => {
                counters.ended_at = ts.or(counters.ended_at);
                counters.outcome = "failed";
                counters.error_summary = Some(p.failure);
            }
            Some(T::SessionRejected(p)) => {
                counters.ended_at = ts.or(counters.ended_at);
                counters.outcome = "rejected";
                counters.error_summary = Some(format!("{}: {}", p.code, p.message));
            }
            Some(T::TurnStarted(p)) => {
                counters.turn_count += 1;
                counters.current_turn_index = p.turn_index;
            }
            Some(T::ApprovalRequested(_)) => counters.approval_count += 1,
            Some(T::SafetyIntervention(_)) => counters.intervention_count += 1,
            // DEFERRED (Phase 26.4, 2026-04-23 discuss-phase decision per CONTEXT.md Deferred Ideas):
            // `SessionEvent::SafetyViolation` has NO `TypedEvent` variant in `SessionEventEnvelope`'s
            // `typed_event` oneof (verified at `proto/roz/v1/agent.proto:359-400`; corroborated by
            // `crates/roz-server/src/grpc/event_mapper.rs:491` which explicitly returns `None` for
            // SafetyViolation). Do NOT add a SafetyViolation match arm — it will not
            // compile. The `_ => {}` catch-all below absorbs any SafetyViolation payload if and when a
            // follow-up phase wires the proto projection. Consequence for THIS phase:
            //   * `counters.violation_count` stays at 0 for every session (D-24 degrades to no-op)
            //   * `counters.policy_ids` stays empty for every session (D-26 degrades to no-op)
            // The migration CHECK constraints + array types remain so nothing breaks when proto wiring
            // lands. Same applies to `RecoveryPending`. Unlock path: extend the oneof with a
            // `SafetyViolationPayload safety_violation = 47` field + update `event_mapper.rs` + wire
            // `SafetyViolation` events through `emit_session_event`; then add the 5-line match arm here
            // and reindex.
            Some(T::ModelCallCompleted(p)) => {
                counters.model_ids.insert(p.model_id);
            }
            Some(T::ControllerLoaded(p)) => {
                counters.controller_artifact_ids.insert(p.artifact_id);
            }
            Some(T::ControllerShadowStarted(p)) => {
                counters.controller_artifact_ids.insert(p.artifact_id);
            }
            Some(T::ControllerPromoted(p)) => {
                counters.controller_artifact_ids.insert(p.artifact_id.clone());
                if let Some(replaced) = p.replaced_id {
                    counters.controller_artifact_ids.insert(replaced);
                }
            }
            Some(T::ControllerRolledBack(p)) => {
                counters.controller_artifact_ids.insert(p.artifact_id.clone());
                counters.controller_artifact_ids.insert(p.restored_id);
            }
            Some(T::ToolCallRequested(p)) => {
                let pt = tool_calls.entry(p.call_id.clone()).or_default();
                pt.tool_name = Some(p.tool_name);
                pt.requested_at = ts;
                pt.trace_id = trace_id.clone().or(pt.trace_id.clone());
                pt.rollover_index = archive.rollover_index;
                pt.turn_index = pt.turn_index.or(Some(counters.current_turn_index));
                if pt.mcap_chunk_offset.is_none() {
                    pt.mcap_chunk_offset = chunk_offset;
                }
            }
            Some(T::ToolCallStarted(p)) => {
                let pt = tool_calls.entry(p.call_id.clone()).or_default();
                if pt.tool_name.is_none() {
                    pt.tool_name = Some(p.tool_name);
                }
                pt.category = Some(p.category);
                pt.started_at = ts;
                pt.rollover_index = archive.rollover_index;
                pt.turn_index = pt.turn_index.or(Some(counters.current_turn_index));
                if pt.mcap_chunk_offset.is_none() {
                    pt.mcap_chunk_offset = chunk_offset;
                }
            }
            Some(T::ToolCallFinished(p)) => {
                let pt = tool_calls.entry(p.call_id.clone()).or_default();
                if pt.tool_name.is_none() {
                    pt.tool_name = Some(p.tool_name);
                }
                pt.finished_at = ts;
                // Prefer the Finished chunk offset per D-03.
                pt.mcap_chunk_offset = chunk_offset.or(pt.mcap_chunk_offset);
            }
            Some(T::ToolUnavailable(p)) => {
                counters
                    .unavailable_in_turn
                    .insert((counters.current_turn_index, p.tool_name));
            }
            _ => {} // Other variants are informational; indexer does not count them.
        }
    }

    Ok(())
}

// ===========================================================================
// Tests — pure unit tests; no DB / no MCAP file required.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_offset_lookup_finds_covering_chunk() {
        let ranges = &[(100_u64, 200_u64, 1000_u64), (200, 300, 2000), (300, 400, 3000)];
        assert_eq!(lookup_chunk_offset(ranges, 150), Some(1000));
        assert_eq!(lookup_chunk_offset(ranges, 250), Some(2000));
        assert_eq!(lookup_chunk_offset(ranges, 400), Some(3000));
    }

    #[test]
    fn chunk_offset_lookup_returns_none_before_first_chunk() {
        let ranges = &[(100_u64, 200_u64, 1000_u64)];
        assert_eq!(lookup_chunk_offset(ranges, 50), None);
    }

    #[test]
    fn chunk_offset_lookup_handles_empty_ranges() {
        assert_eq!(lookup_chunk_offset(&[], 100), None);
    }

    #[test]
    fn index_summary_is_serializable() {
        let summary = IndexSummary {
            session_id: Uuid::nil(),
            tool_calls_indexed: 60,
            turn_count: 2,
            outcome: "succeeded",
            rollover_files_read: 1,
        };
        let json = serde_json::to_string(&summary).expect("serialize");
        assert!(json.contains("\"tool_calls_indexed\":60"));
    }

    #[test]
    fn session_outcome_defaults_to_abandoned() {
        let counters = SessionCounters::new();
        assert_eq!(counters.outcome, "abandoned");
    }

    #[test]
    fn tool_call_outcome_unfinished_when_no_finished() {
        let p = PartialToolCall {
            requested_at: Some(Utc::now()),
            started_at: Some(Utc::now()),
            finished_at: None,
            unavailable_paired: false,
            ..PartialToolCall::default()
        };
        assert_eq!(p.derive_outcome(), "unfinished");
    }

    #[test]
    fn tool_call_outcome_failed_when_unavailable_paired() {
        let p = PartialToolCall {
            finished_at: Some(Utc::now()),
            unavailable_paired: true,
            ..PartialToolCall::default()
        };
        assert_eq!(p.derive_outcome(), "failed");
    }

    #[test]
    fn tool_call_outcome_succeeded_happy_path() {
        let p = PartialToolCall {
            finished_at: Some(Utc::now()),
            unavailable_paired: false,
            ..PartialToolCall::default()
        };
        assert_eq!(p.derive_outcome(), "succeeded");
    }
}
