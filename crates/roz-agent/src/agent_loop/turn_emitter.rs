//! Write-behind session turn persistence (DEBT-03).
//!
//! The `AgentLoop` hot path MUST NOT block on DB writes. This module provides:
//!
//! 1. A bounded `tokio::sync::mpsc` channel with **drop-newest on full**
//!    (industry-standard tokio pattern via `try_send` + `TrySendError::Full`).
//! 2. A dedicated flush task that writes turns to `roz_session_turns` inside a
//!    per-write transaction (RLS requires `set_config(..., true)` to be bound
//!    to the current tx — see `roz_db::set_tenant_context`).
//! 3. A per-session `MAX(turn_index)+1` base-offset cache so resumed sessions
//!    continue monotonically without violating `UNIQUE(session_id, turn_index)`.
//!
//! Callers attach a `TurnEmitter` to the agent loop via
//! [`AgentLoop::with_turn_emitter`](super::AgentLoop::with_turn_emitter), then
//! spawn [`run_flush_task`] on the receiver with the DB pool and a
//! `CancellationToken` tied to session lifetime.

use std::collections::HashMap;

use tokio::sync::mpsc::{self, Receiver, error::TrySendError};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Default capacity for the write-behind channel.
///
/// Sized to buffer ~1024 turns per worker process before drop-newest kicks
/// in — large enough to ride out transient DB slowness, small enough to
/// bound memory on sustained outage.
pub const TURN_BUFFER_CAPACITY: usize = 1024;

/// One persistable session turn, produced by `run_streaming_core` and
/// consumed by [`run_flush_task`].
///
/// `turn_index` is the LOCAL index assigned by the agent loop (starts at 0
/// per run). The flush task rewrites it to an ABSOLUTE index via its
/// per-session `MAX+1` base offset before INSERT.
#[derive(Debug, Clone)]
pub struct TurnEnvelope {
    pub session_id: Uuid,
    pub tenant_id: Uuid,
    pub turn_index: i32,
    pub role: &'static str, // "user" | "assistant" | "tool"
    pub content: serde_json::Value,
    pub token_usage: Option<serde_json::Value>,
}

/// Cloneable handle used by the agent loop to enqueue turns non-blockingly.
///
/// On full buffer, [`emit`](Self::emit) logs `tracing::warn!` and DROPS the
/// NEW envelope (drop-newest policy, locked by Phase 13 CONTEXT.md D-01).
/// Earlier envelopes already in the channel are preserved.
#[derive(Clone)]
pub struct TurnEmitter {
    tx: mpsc::Sender<TurnEnvelope>,
}

impl TurnEmitter {
    /// Create a new emitter with [`TURN_BUFFER_CAPACITY`] slots.
    #[must_use]
    pub fn new() -> (Self, Receiver<TurnEnvelope>) {
        Self::with_capacity(TURN_BUFFER_CAPACITY)
    }

    /// Create an emitter with a custom capacity (primarily for overflow tests).
    #[must_use]
    pub fn with_capacity(cap: usize) -> (Self, Receiver<TurnEnvelope>) {
        let (tx, rx) = mpsc::channel(cap);
        (Self { tx }, rx)
    }

    /// Best-effort enqueue. Never blocks.
    ///
    /// On `TrySendError::Full` — emits `tracing::warn!` and drops the NEW
    /// envelope (drop-newest policy). On `TrySendError::Closed` — logs
    /// `tracing::debug!`; the flush task has already exited, so silent drop
    /// is the correct shutdown behavior.
    pub fn emit(&self, envelope: TurnEnvelope) {
        match self.tx.try_send(envelope) {
            Ok(()) => {}
            Err(TrySendError::Full(dropped)) => {
                tracing::warn!(
                    session_id = %dropped.session_id,
                    turn_index = dropped.turn_index,
                    role = dropped.role,
                    capacity = self.tx.max_capacity(),
                    "session turn dropped: write-behind buffer full"
                );
            }
            Err(TrySendError::Closed(dropped)) => {
                tracing::debug!(
                    session_id = %dropped.session_id,
                    turn_index = dropped.turn_index,
                    role = dropped.role,
                    "session turn dropped: flush task terminated"
                );
            }
        }
    }
}

/// Run the write-behind flush task until cancellation.
///
/// Drains the receiver, opens a fresh transaction per turn
/// (`pool.begin()` → `set_tenant_context` → `insert_turn` → `commit`), and
/// maintains a per-session `MAX(turn_index)+1` cache so resumed sessions
/// continue monotonically without violating `UNIQUE(session_id, turn_index)`.
///
/// Cancellation: on `cancel.cancelled()`, drains any remaining envelopes
/// already in the channel via `try_recv` then exits. Any envelopes still in
/// flight after cancel are silently dropped (acceptable per Phase 13
/// CONTEXT.md — persistence is best-effort, not agent-critical).
pub async fn run_flush_task(mut rx: Receiver<TurnEnvelope>, pool: sqlx::PgPool, cancel: CancellationToken) {
    // Keyed by (tenant_id, session_id) for defense-in-depth against any
    // upstream bug that could route an envelope with a mismatched tenant to a
    // session the tenant does not own. Even though `session_id` is a v4 UUID
    // (collision-free in practice), this tuple key also hardens against test
    // fixtures that reuse `Uuid::nil()` and future non-UUID session IDs.
    let mut base_offsets: HashMap<(Uuid, Uuid), i32> = HashMap::new();

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                // Drain remaining envelopes before exiting.
                while let Ok(env) = rx.try_recv() {
                    if let Err(e) = flush_one(&pool, &mut base_offsets, &env).await {
                        if is_unique_violation(&e) {
                            // Reseed + retry once during drain as well.
                            if let Err(e2) = flush_one(&pool, &mut base_offsets, &env).await {
                                tracing::warn!(
                                    session_id = %env.session_id,
                                    turn_index = env.turn_index,
                                    role = env.role,
                                    error = %e2,
                                    "failed to flush session turn during drain after reseed retry"
                                );
                            }
                        } else {
                            tracing::warn!(
                                session_id = %env.session_id,
                                turn_index = env.turn_index,
                                role = env.role,
                                error = %e,
                                "failed to flush session turn during drain"
                            );
                        }
                    }
                }
                break;
            }
            maybe_env = rx.recv() => {
                match maybe_env {
                    Some(env) => {
                        if let Err(e) = flush_one(&pool, &mut base_offsets, &env).await {
                            // On UNIQUE(session_id, turn_index) violation (23505) the
                            // cached base offset is stale (concurrent writer, or TOCTOU
                            // between seed and first INSERT). `flush_one` has already
                            // invalidated the entry before returning; retry once so the
                            // next call re-reads MAX+1 and self-heals.
                            if is_unique_violation(&e) {
                                tracing::warn!(
                                    session_id = %env.session_id,
                                    turn_index = env.turn_index,
                                    role = env.role,
                                    "retrying session turn after unique-violation; base offset reseed"
                                );
                                if let Err(e2) = flush_one(&pool, &mut base_offsets, &env).await {
                                    tracing::warn!(
                                        session_id = %env.session_id,
                                        turn_index = env.turn_index,
                                        role = env.role,
                                        error = %e2,
                                        "failed to flush session turn after reseed retry"
                                    );
                                }
                            } else {
                                tracing::warn!(
                                    session_id = %env.session_id,
                                    turn_index = env.turn_index,
                                    role = env.role,
                                    error = %e,
                                    "failed to flush session turn"
                                );
                            }
                        }
                    }
                    None => break, // All senders dropped.
                }
            }
        }
    }
}

/// Per-write transaction: open → `set_tenant_context` → `insert_turn` → commit.
///
/// The first time a `session_id` is seen, queries `MAX(turn_index)+1` from
/// the DB and caches it as the session's base offset. All subsequent inserts
/// for that session use `env.turn_index + base_offset` for the absolute
/// `turn_index`, preserving `UNIQUE(session_id, turn_index)` across resumes.
async fn flush_one(
    pool: &sqlx::PgPool,
    base_offsets: &mut HashMap<(Uuid, Uuid), i32>,
    env: &TurnEnvelope,
) -> Result<(), sqlx::Error> {
    let key = (env.tenant_id, env.session_id);
    let mut tx = pool.begin().await?;
    roz_db::set_tenant_context(&mut *tx, &env.tenant_id).await?;

    // Seed the base offset for this session if not cached yet.
    if let std::collections::hash_map::Entry::Vacant(slot) = base_offsets.entry(key) {
        let base = match roz_db::session_turns::max_turn_index(&mut *tx, env.session_id).await {
            Ok(max) => max.map_or(0, |m| m + 1),
            Err(e) => {
                tracing::warn!(
                    session_id = %env.session_id,
                    error = %e,
                    "failed to seed turn_index base offset; starting at 0 — duplicate-key may follow"
                );
                0
            }
        };
        slot.insert(base);
    }

    let base = base_offsets.get(&key).copied().unwrap_or(0);
    let absolute_index = env.turn_index.saturating_add(base);

    match roz_db::session_turns::insert_turn(
        &mut *tx,
        env.session_id,
        absolute_index,
        env.role,
        &env.content,
        env.token_usage.as_ref(),
    )
    .await
    {
        Ok(()) => {}
        Err(e) => {
            if is_unique_violation(&e) {
                // Base offset is stale. Invalidate the cache entry so the next
                // attempt for this session re-reads MAX(turn_index)+1 from the
                // DB. The caller ([`run_flush_task`]) handles the retry so the
                // pool tx drop here is intentional.
                base_offsets.remove(&key);
            }
            return Err(e);
        }
    }

    tx.commit().await?;
    Ok(())
}

/// Returns `true` if the given `sqlx::Error` is a `PostgreSQL` unique-violation
/// (SQLSTATE `23505`).
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_envelope(idx: i32) -> TurnEnvelope {
        TurnEnvelope {
            session_id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            turn_index: idx,
            role: "user",
            content: json!({ "i": idx }),
            token_usage: None,
        }
    }

    #[tokio::test]
    async fn emit_sends_envelope() {
        let (emitter, mut rx) = TurnEmitter::new();
        emitter.emit(sample_envelope(0));
        let got = rx.recv().await.expect("recv");
        assert_eq!(got.turn_index, 0);
        assert_eq!(got.role, "user");
    }

    #[tokio::test]
    async fn emit_on_full_drops_newest() {
        let (emitter, mut rx) = TurnEmitter::with_capacity(2);
        // Fill the channel without draining.
        emitter.emit(sample_envelope(0));
        emitter.emit(sample_envelope(1));
        // Third emit must be dropped (NOT block, NOT panic).
        emitter.emit(sample_envelope(2));
        emitter.emit(sample_envelope(3));

        // The first two envelopes are preserved — new ones were dropped.
        let a = rx.recv().await.expect("recv 0");
        assert_eq!(a.turn_index, 0);
        let b = rx.recv().await.expect("recv 1");
        assert_eq!(b.turn_index, 1);
        // No more envelopes (the newer ones were dropped).
        assert!(
            rx.try_recv().is_err(),
            "capacity-2 buffer should hold exactly 2 envelopes"
        );
    }

    #[tokio::test]
    async fn emit_after_receiver_dropped_is_silent() {
        let (emitter, rx) = TurnEmitter::new();
        drop(rx);
        // Must not panic.
        emitter.emit(sample_envelope(0));
    }
}
