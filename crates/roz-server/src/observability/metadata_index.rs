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

#[allow(
    unused_imports,
    reason = "HashMap + PgPool + logging + CHANNEL_SESSION_EVENTS land in Task 2's index_session"
)]
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
#[allow(unused_imports, reason = "PgPool is consumed by index_session in Task 2")]
use sqlx::PgPool;
#[allow(unused_imports, reason = "info!/warn! consumed by index_session in Task 2")]
use tracing::{info, warn};
use uuid::Uuid;

#[allow(
    unused_imports,
    reason = "CHANNEL_SESSION_EVENTS consumed by index_one_rollover_file in Task 2"
)]
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
#[allow(dead_code, reason = "constructed by index_one_rollover_file in Task 2")]
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
#[allow(dead_code, reason = "constructed by index_session in Task 2")]
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
    #[allow(dead_code, reason = "called by index_session in Task 2")]
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
/// `build_chunk_time_index` in Task 2 which does the sort).
#[allow(dead_code, reason = "called by index_one_rollover_file in Task 2")]
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
}
