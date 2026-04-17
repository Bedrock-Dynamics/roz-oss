//! Worker-side turn persistence helper (DEBT-03).
//!
//! `execute_task` calls [`build_turn_flush`] to optionally construct a
//! [`TurnEmitter`] + spawned flush task when the worker is configured with
//! `ROZ_DATABASE_URL`. When the env var is unset (or the pool fails to
//! open), this returns `(None, None, CancellationToken::new())` and the
//! agent loop runs without DB persistence — fail-closed.
//!
//! The caller is responsible for:
//! 1. Chaining `.with_turn_emitter_opt(emitter)` onto its `AgentLoop`.
//! 2. Calling `cancel.cancel()` + `handle.await` on every exit path to
//!    drain the flush task before `execute_task` returns.

use roz_agent::agent_loop::turn_emitter::TurnEnvelope;
use roz_agent::agent_loop::{TurnEmitter, run_flush_task};
use tokio::sync::mpsc::Receiver;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::WorkerConfig;

/// Bundle of handles returned by [`build_turn_flush`]. On a fail-closed path
/// (DB unset or pool open failed) all three are `None`/fresh-token/None.
pub struct TurnFlushBundle {
    pub emitter: Option<TurnEmitter>,
    pub handle: Option<JoinHandle<()>>,
    pub cancel: CancellationToken,
    /// Shared PgPool used by the flush task. Cloned into
    /// `ToolContext::extensions` so the MEM-07 Pure tools (session_search,
    /// memory_read, memory_write, user_model_query) reach Postgres without
    /// opening a parallel pool. `None` on fail-closed paths.
    pub pool: Option<sqlx::PgPool>,
    /// MEM-03: fan-out receiver for the fact extractor. `None` when
    /// persistence is off (no DB). Worker bootstrap (`main.rs`) spawns
    /// `run_fact_extractor_task` against this receiver when an aux-LLM is
    /// available.
    pub fact_rx: Option<Receiver<TurnEnvelope>>,
}

impl TurnFlushBundle {
    /// Cancel the flush task and await its completion. Safe to call multiple
    /// times (cancel is idempotent; a consumed handle is skipped).
    pub async fn drain(mut self) {
        self.cancel.cancel();
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

/// Construct the worker's write-behind plumbing for one task execution.
///
/// Returns a fresh [`TurnFlushBundle`] regardless of DB config — when
/// `database_url` is unset, the bundle is inert (no pool, no flush task,
/// no emitter) and the agent loop simply runs without persistence.
pub async fn build_turn_flush(config: &WorkerConfig) -> TurnFlushBundle {
    let cancel = CancellationToken::new();
    let Some(url) = config.database_url.as_deref() else {
        return TurnFlushBundle {
            emitter: None,
            handle: None,
            cancel,
            pool: None,
            fact_rx: None,
        };
    };

    let pool = match roz_db::create_pool(url).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "worker: failed to open PgPool for turn persistence; continuing without DEBT-03 persistence"
            );
            return TurnFlushBundle {
                emitter: None,
                handle: None,
                cancel,
                pool: None,
                fact_rx: None,
            };
        }
    };

    // MEM-03: fan out the emitter so the fact-extractor (spawned by the
    // worker bootstrap) receives an independent stream of envelopes.
    let (emitter, rx, fact_rx) = TurnEmitter::with_fact_extractor(roz_agent::agent_loop::TURN_BUFFER_CAPACITY);
    let flush_cancel = cancel.clone();
    let flush_pool = pool.clone();
    let handle = tokio::spawn(async move {
        run_flush_task(rx, flush_pool, flush_cancel).await;
    });

    TurnFlushBundle {
        emitter: Some(emitter),
        handle: Some(handle),
        cancel,
        pool: Some(pool),
        fact_rx: Some(fact_rx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> WorkerConfig {
        use figment::{Figment, providers::Serialized};
        let figment = Figment::new().merge(Serialized::defaults(serde_json::json!({
            "api_url": "http://localhost:8080",
            "nats_url": "nats://localhost:4222",
            "restate_url": "http://localhost:9080",
            "api_key": "roz_sk_test",
            "gateway_api_key": "paig_test",
        })));
        WorkerConfig::from_figment(&figment).unwrap()
    }

    #[tokio::test]
    async fn worker_skips_turn_emitter_when_db_absent() {
        let cfg = base_config();
        assert!(cfg.database_url.is_none());
        let bundle = build_turn_flush(&cfg).await;
        assert!(bundle.emitter.is_none(), "no emitter without ROZ_DATABASE_URL");
        assert!(bundle.handle.is_none(), "no flush task without ROZ_DATABASE_URL");
        // Draining a no-op bundle must not panic.
        bundle.drain().await;
    }

    #[tokio::test]
    async fn worker_skips_turn_emitter_when_pool_open_fails() {
        let mut cfg = base_config();
        // Unreachable host — create_pool will fail.
        cfg.database_url = Some("postgres://invalid:5432/nope?connect_timeout=1".to_string());
        let bundle = build_turn_flush(&cfg).await;
        assert!(bundle.emitter.is_none(), "emitter must be None when pool open fails");
        assert!(bundle.handle.is_none(), "no flush task when pool open fails");
        bundle.drain().await;
    }
}
