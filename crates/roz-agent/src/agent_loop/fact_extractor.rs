//! MEM-03: N-turn batched async fact extractor.
//!
//! Consumes the secondary `TurnEmitter` channel (see `turn_emitter.rs`).
//! Buffers per tenant, flushes every `batch_size` turns (default 5, clamp
//! `[4, 6]`) via an `AuxLlm` call. Dedups against recent facts with the
//! `md5(fact)` index (migration 17.3 `user_model_facts`), inserts
//! non-duplicates into `roz_user_model_facts`.
//!
//! Cancellation mirrors `run_flush_task`: the task flushes any remaining
//! buffered turns on cancel and exits. Channel-closed is treated identically.
//!
//! The extractor NEVER panics on aux-LLM / DB failure — all failures are
//! logged at `warn` and the batch is dropped to bound memory (research
//! §"Rate control" + D-06).

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent_loop::turn_emitter::TurnEnvelope;
use crate::aux_llm::AuxLlm;

/// Default batch size; overridable via `ROZ_FACT_EXTRACTION_BATCH_SIZE`,
/// clamped to `[FACT_EXTRACTION_BATCH_MIN, FACT_EXTRACTION_BATCH_MAX]`.
pub const DEFAULT_FACT_EXTRACTION_BATCH_SIZE: usize = 5;
/// Lower clamp for the batch-size env override.
pub const FACT_EXTRACTION_BATCH_MIN: usize = 4;
/// Upper clamp for the batch-size env override.
pub const FACT_EXTRACTION_BATCH_MAX: usize = 6;

/// Maximum per-tenant buffer size — drop-oldest above this to bound memory
/// if the aux-LLM is slow / rate-limited (research §"Rate control" + T-17-50).
pub const FACT_BUFFER_PER_SESSION_MAX: usize = 50;

/// Minimum confidence (inclusive) for an extracted fact to be persisted.
/// Below this, the aux-LLM response is treated as noise (pitfall 6).
const FACT_CONFIDENCE_FLOOR: f32 = 0.6;

/// How many recent facts to scan when computing exact-match dedup.
const FACT_DEDUP_RECENT_LIMIT: i64 = 50;

/// Default TTL for extracted facts — 90 days.
const DEFAULT_FACT_STALE_AFTER_SECS: i64 = 90 * 24 * 60 * 60;

/// Configuration injected at session bootstrap.
#[derive(Debug, Clone)]
pub struct FactExtractorConfig {
    /// Opaque observed-peer id (the end-user / human). Stored verbatim on
    /// every fact row.
    pub observed_peer_id: String,
    /// Opaque observer-peer id (the assistant / agent). Stored verbatim.
    pub observer_peer_id: String,
    /// Batch size (env-clamped).
    pub batch_size: usize,
    /// Default `stale_after` delta (seconds) applied to every fact.
    /// `None` disables the TTL.
    pub default_stale_after_secs: Option<i64>,
}

impl Default for FactExtractorConfig {
    fn default() -> Self {
        let batch_size = std::env::var("ROZ_FACT_EXTRACTION_BATCH_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_FACT_EXTRACTION_BATCH_SIZE)
            .clamp(FACT_EXTRACTION_BATCH_MIN, FACT_EXTRACTION_BATCH_MAX);
        Self {
            observed_peer_id: "user".to_string(),
            observer_peer_id: "roz".to_string(),
            batch_size,
            default_stale_after_secs: Some(DEFAULT_FACT_STALE_AFTER_SECS),
        }
    }
}

/// Fact shape expected from the aux-LLM JSON response.
#[derive(Debug, Deserialize)]
struct ExtractedFact {
    fact: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
}

fn default_confidence() -> f32 {
    0.7
}

/// Build the naive extraction prompt — a transcript + an instruction to emit
/// durable user-model facts in JSON. Format is intentionally simple; PLAN-10
/// integration tests cover the aux-LLM interaction end-to-end.
fn extraction_prompt(turns: &[TurnEnvelope]) -> String {
    let transcript = turns
        .iter()
        .map(|t| {
            // Cheap projection of content JSON to a string. Nested structure
            // is irrelevant — the aux-LLM only needs readable context.
            let content = match &t.content {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("[{}] {}", t.role, content)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Conversation transcript:\n{transcript}\n\n\
         Task: Extract 0 to 5 DURABLE facts about the user that would be useful \
         in future sessions (preferences, constraints, calibration quirks, \
         stated goals). Ignore transient conversational content. Return ONLY \
         valid JSON in the shape: [{{\"fact\": \"...\", \"confidence\": 0.0..1.0}}]"
    )
}

/// Flush one tenant's buffered turns through the aux-LLM and persist
/// non-duplicate high-confidence facts. All failures are logged + swallowed —
/// the caller's event loop continues.
async fn flush_batch(
    pool: &PgPool,
    aux: &dyn AuxLlm,
    config: &FactExtractorConfig,
    tenant_id: Uuid,
    buffered: Vec<TurnEnvelope>,
) {
    if buffered.is_empty() {
        return;
    }
    let turn_count = buffered.len();
    let system = "You extract durable user-model facts from conversation transcripts.";
    let user = extraction_prompt(&buffered);
    let raw = match aux.complete_text(system, &user).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(
                %err,
                turn_count,
                "fact_extractor: aux-llm call failed; batch dropped"
            );
            return;
        }
    };

    let facts: Vec<ExtractedFact> = match serde_json::from_str::<Vec<ExtractedFact>>(&raw) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                %err,
                raw = %raw,
                "fact_extractor: aux-llm response was not valid JSON; batch dropped"
            );
            return;
        }
    };

    for fact in facts {
        let trimmed = fact.fact.trim();
        if trimmed.is_empty() || trimmed.chars().count() > roz_db::user_model_facts::USER_MODEL_FACT_CHAR_CAP {
            continue;
        }
        if fact.confidence < FACT_CONFIDENCE_FLOOR {
            continue; // low-confidence noise filter (research pitfall 6)
        }

        let stale_after = config
            .default_stale_after_secs
            .and_then(chrono::Duration::try_seconds)
            .map(|d| chrono::Utc::now() + d);

        let res: Result<(), sqlx::Error> = async {
            let mut tx = pool.begin().await?;
            roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
            let dup = roz_db::user_model_facts::is_duplicate(
                &mut *tx,
                tenant_id,
                &config.observed_peer_id,
                trimmed,
                FACT_DEDUP_RECENT_LIMIT,
            )
            .await?;
            if !dup {
                roz_db::user_model_facts::insert_fact(
                    &mut *tx,
                    tenant_id,
                    &config.observed_peer_id,
                    &config.observer_peer_id,
                    trimmed,
                    None, // source_turn_id: future follow-up — link to first turn in batch
                    fact.confidence,
                    stale_after,
                )
                .await?;
            }
            tx.commit().await?;
            Ok(())
        }
        .await;

        if let Err(err) = res {
            tracing::warn!(%err, "fact_extractor: DB write failed; continuing");
        }
    }
}

/// Run the fact-extraction task until `cancel` fires or the channel closes.
///
/// Mirrors `run_flush_task` semantics: spawn at session start, cancel at
/// session end. On cancel OR channel-close, flushes all buffered turns
/// before exit (D-06 session-end flush).
///
/// Per-tenant buffers are bounded by [`FACT_BUFFER_PER_SESSION_MAX`] with
/// drop-oldest eviction to tolerate sustained aux-LLM outages (T-17-50).
pub async fn run_fact_extractor_task(
    mut rx: Receiver<TurnEnvelope>,
    pool: PgPool,
    aux: Arc<dyn AuxLlm>,
    config: FactExtractorConfig,
    cancel: CancellationToken,
) {
    let mut buffers: HashMap<Uuid, Vec<TurnEnvelope>> = HashMap::new();

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                // Session-end flush (D-06): drain all per-tenant buffers.
                for (tenant_id, buffered) in buffers.drain() {
                    flush_batch(&pool, aux.as_ref(), &config, tenant_id, buffered).await;
                }
                // Drain any remaining envelopes already in the channel.
                while let Ok(env) = rx.try_recv() {
                    let tid = env.tenant_id;
                    flush_batch(&pool, aux.as_ref(), &config, tid, vec![env]).await;
                }
                tracing::debug!("fact_extractor: cancelled + drained; exiting");
                return;
            }
            maybe_env = rx.recv() => {
                let Some(env) = maybe_env else {
                    // Channel closed — same treatment as cancel.
                    for (tenant_id, buffered) in buffers.drain() {
                        flush_batch(&pool, aux.as_ref(), &config, tenant_id, buffered).await;
                    }
                    tracing::debug!("fact_extractor: channel closed; exiting");
                    return;
                };
                let tenant_id = env.tenant_id;
                let bucket = buffers.entry(tenant_id).or_default();
                bucket.push(env);

                if bucket.len() >= config.batch_size {
                    let buffered = std::mem::take(bucket);
                    flush_batch(&pool, aux.as_ref(), &config, tenant_id, buffered).await;
                }

                // Drop-oldest to bound memory under sustained aux outage.
                if let Some(bucket) = buffers.get_mut(&tenant_id)
                    && bucket.len() > FACT_BUFFER_PER_SESSION_MAX
                {
                    let drop = bucket.len() - FACT_BUFFER_PER_SESSION_MAX;
                    bucket.drain(0..drop);
                    tracing::warn!(drop, "fact_extractor: per-tenant buffer full; dropped oldest");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aux_llm::AuxLlmError;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Always-erroring AuxLlm — flush path observes one call per attempted
    /// batch, letting us verify buffer-drain + cancel semantics without a DB.
    #[derive(Debug, Default)]
    struct NullAuxLlm {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl AuxLlm for NullAuxLlm {
        async fn complete_text(&self, _system: &str, _user: &str) -> Result<String, AuxLlmError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            // Return err so DB path is never exercised in unit tests.
            Err(AuxLlmError::Request("null aux — unit test".to_string()))
        }
    }

    fn sample_envelope(tenant: Uuid, idx: i32) -> TurnEnvelope {
        TurnEnvelope {
            session_id: Uuid::nil(),
            tenant_id: tenant,
            turn_index: idx,
            role: "user",
            content: json!({ "i": idx }),
            token_usage: None,
            kind: TurnEnvelope::KIND_TURN,
        }
    }

    #[test]
    fn default_config_clamps_batch_size() {
        // Remove any ambient env (single-threaded test to avoid races).
        // SAFETY: tests in this crate run with `--test-threads=1` for any env
        // mutation; default path doesn't mutate but still read current env.
        let cfg = FactExtractorConfig::default();
        assert!(
            (FACT_EXTRACTION_BATCH_MIN..=FACT_EXTRACTION_BATCH_MAX).contains(&cfg.batch_size),
            "batch_size {} must be clamped into [{},{}]",
            cfg.batch_size,
            FACT_EXTRACTION_BATCH_MIN,
            FACT_EXTRACTION_BATCH_MAX
        );
    }

    #[test]
    fn extraction_prompt_includes_all_turns() {
        let t = Uuid::nil();
        let turns = vec![sample_envelope(t, 0), sample_envelope(t, 1), sample_envelope(t, 2)];
        let prompt = extraction_prompt(&turns);
        assert!(prompt.contains("[user]"));
        assert!(prompt.contains("\"i\":0"));
        assert!(prompt.contains("\"i\":1"));
        assert!(prompt.contains("\"i\":2"));
        assert!(prompt.contains("Return ONLY"));
    }

    /// Flush-on-cancel: submit N < batch_size, then cancel; the extractor
    /// must attempt exactly one flush before exiting (even though the
    /// aux-LLM errors, we observe the call count).
    #[tokio::test]
    async fn cancel_flushes_partial_batch() {
        let (tx, rx) = tokio::sync::mpsc::channel::<TurnEnvelope>(16);
        let aux = Arc::new(NullAuxLlm::default());
        let cancel = CancellationToken::new();
        let config = FactExtractorConfig {
            batch_size: 5, // never reached
            ..Default::default()
        };

        // No pool is reachable in unit tests; the aux-LLM errors BEFORE any
        // DB call, so this test does not require a real PgPool to exercise
        // the cancel-flush code path — but `flush_batch` still takes a &PgPool
        // argument. Use a connect-lazy pool against a bogus URL: the aux
        // fails first, so the pool is never used.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://never@127.0.0.1:1/never")
            .expect("connect_lazy");

        let aux_clone: Arc<dyn AuxLlm> = aux.clone();
        let task = tokio::spawn({
            let pool = pool.clone();
            let cancel = cancel.clone();
            async move {
                run_fact_extractor_task(rx, pool, aux_clone, config, cancel).await;
            }
        });

        let t = Uuid::new_v4();
        tx.send(sample_envelope(t, 0)).await.unwrap();
        tx.send(sample_envelope(t, 1)).await.unwrap();
        // Let the receiver drain.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        cancel.cancel();
        task.await.unwrap();

        // Exactly one flush attempt on cancel (two turns in one tenant bucket).
        assert_eq!(
            aux.calls.load(Ordering::Relaxed),
            1,
            "expected one aux-llm call on cancel-flush"
        );
    }

    /// When batch_size turns arrive, a flush fires automatically; no cancel
    /// needed. This exercises the N-turn trigger path independently of
    /// session-end flush.
    #[tokio::test]
    async fn full_batch_auto_flushes() {
        let (tx, rx) = tokio::sync::mpsc::channel::<TurnEnvelope>(16);
        let aux = Arc::new(NullAuxLlm::default());
        let cancel = CancellationToken::new();
        let config = FactExtractorConfig {
            batch_size: 4,
            ..Default::default()
        };

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://never@127.0.0.1:1/never")
            .expect("connect_lazy");

        let aux_clone: Arc<dyn AuxLlm> = aux.clone();
        let cancel_clone = cancel.clone();
        let task = tokio::spawn(async move {
            run_fact_extractor_task(rx, pool, aux_clone, config, cancel_clone).await;
        });

        let t = Uuid::new_v4();
        for i in 0..4 {
            tx.send(sample_envelope(t, i)).await.unwrap();
        }
        // Give the task time to process the 4th envelope + fire flush.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        cancel.cancel();
        task.await.unwrap();

        assert_eq!(
            aux.calls.load(Ordering::Relaxed),
            1,
            "expected exactly one aux-llm call on batch-size trigger"
        );
    }
}
