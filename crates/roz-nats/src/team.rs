//! NATS `JetStream` helpers for multi-agent team coordination.
//!
//! Stream: `ROZ_TEAM_EVENTS`
//! Subject pattern: `roz.team.{parent_task_id}.worker.{child_task_id}`
//! KV bucket: `roz_teams` — team roster per parent task

use async_nats::jetstream::Context as JetStreamContext;
use async_nats::jetstream::kv::Config as KvConfig;
use bytes::Bytes;
use roz_core::team::{SequencedTeamEvent, TeamEvent, WorkerRecord};
use uuid::Uuid;

pub const TEAM_STREAM: &str = "ROZ_TEAM_EVENTS";
pub const TEAM_KV_BUCKET: &str = "roz_teams";
pub const TEAM_SEQUENCE_KV_BUCKET: &str = "roz_team_sequences";

/// Internal NATS request-reply subject used by `SpawnWorkerTool` to create child tasks
/// without going through the public REST API.
pub const INTERNAL_SPAWN_SUBJECT: &str = "roz.internal.tasks.spawn";

/// Subject for a single worker's progress events.
#[must_use]
pub fn worker_subject(parent_task_id: Uuid, child_task_id: Uuid) -> String {
    format!("roz.team.{parent_task_id}.worker.{child_task_id}")
}

/// Subject pattern matching all workers in a team (`JetStream` filter).
#[must_use]
pub fn team_subject_pattern(parent_task_id: Uuid) -> String {
    format!("roz.team.{parent_task_id}.worker.>")
}

#[must_use]
fn team_sequence_key(parent_task_id: Uuid, child_task_id: Uuid) -> String {
    format!("team.{parent_task_id}.worker.{child_task_id}.seq")
}

/// Publish a `TeamEvent` to the worker's `JetStream` subject.
///
/// # Errors
/// Returns an error if the NATS publish or ack fails.
pub async fn publish_team_event(
    js: &JetStreamContext,
    parent_task_id: Uuid,
    child_task_id: Uuid,
    event: &TeamEvent,
) -> Result<(), async_nats::Error> {
    let subject = worker_subject(parent_task_id, child_task_id);
    let seq = next_team_event_sequence(js, parent_task_id, child_task_id).await?;
    let payload = serde_json::to_vec(&SequencedTeamEvent {
        seq,
        timestamp_ns: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0),
        event: event.clone(),
    })
    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    js.publish(subject, payload.into()).await?.await?;
    Ok(())
}

/// Get-or-create the team roster KV bucket.
async fn get_or_create_kv(js: &JetStreamContext) -> Result<async_nats::jetstream::kv::Store, async_nats::Error> {
    match js.get_key_value(TEAM_KV_BUCKET).await {
        Ok(store) => Ok(store),
        Err(_) => js
            .create_key_value(KvConfig {
                bucket: TEAM_KV_BUCKET.into(),
                max_age: std::time::Duration::from_secs(86400),
                ..Default::default()
            })
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
    }
}

async fn get_or_create_sequence_kv(
    js: &JetStreamContext,
) -> Result<async_nats::jetstream::kv::Store, async_nats::Error> {
    match js.get_key_value(TEAM_SEQUENCE_KV_BUCKET).await {
        Ok(store) => Ok(store),
        Err(_) => js
            .create_key_value(KvConfig {
                bucket: TEAM_SEQUENCE_KV_BUCKET.into(),
                max_age: std::time::Duration::from_secs(86400),
                ..Default::default()
            })
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
    }
}

async fn next_team_event_sequence(
    js: &JetStreamContext,
    parent_task_id: Uuid,
    child_task_id: Uuid,
) -> Result<u64, async_nats::Error> {
    let bucket = get_or_create_sequence_kv(js).await?;
    let key = team_sequence_key(parent_task_id, child_task_id);

    loop {
        let entry = bucket
            .entry(&key)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let revision = entry.as_ref().map_or(0, |value| value.revision);
        let current = entry
            .as_ref()
            .and_then(|value| std::str::from_utf8(&value.value).ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let next = current.saturating_add(1);

        match bucket.update(&key, Bytes::from(next.to_string()), revision).await {
            Ok(_) => return Ok(next),
            Err(error) if error.kind() == async_nats::jetstream::kv::UpdateErrorKind::WrongLastRevision => {
                tracing::debug!(%parent_task_id, %child_task_id, "team event sequence CAS conflict, retrying");
            }
            Err(error) => return Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
        }
    }
}

/// Atomically append a single `WorkerRecord` to the team roster in `JetStream` KV.
///
/// Uses a compare-and-swap retry loop (`update` with the current revision) to avoid
/// the get-then-put race where two concurrent callers overwrite each other's changes.
///
/// # Errors
/// Returns an error if the KV read or write fails for a reason other than a revision
/// conflict (which is retried internally).
pub async fn upsert_team_roster(
    js: &JetStreamContext,
    parent_task_id: Uuid,
    new_record: &WorkerRecord,
) -> Result<(), async_nats::Error> {
    let bucket = get_or_create_kv(js).await?;
    let key = parent_task_id.to_string();

    loop {
        // Read current value + revision for CAS.
        let entry = bucket
            .entry(&key)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        let revision = entry.as_ref().map_or(0, |e| e.revision);

        // Deserialize current roster (empty if the key doesn't exist yet).
        let mut roster: Vec<WorkerRecord> = match entry {
            None => vec![],
            Some(e) => {
                serde_json::from_slice(&e.value).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            }
        };

        roster.push(new_record.clone());

        let bytes: Bytes = serde_json::to_vec(&roster)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            .into();

        // Attempt atomic CAS update. revision == 0 means "create" (key must not exist).
        match bucket.update(&key, bytes, revision).await {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == async_nats::jetstream::kv::UpdateErrorKind::WrongLastRevision => {
                tracing::debug!(
                    %parent_task_id,
                    "roster CAS conflict, retrying"
                );
            }
            Err(e) => {
                return Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
            }
        }
    }
}

/// Read the worker roster for a team from `JetStream` KV. Returns empty vec if not found.
///
/// # Errors
/// Returns an error if the KV read fails.
pub async fn get_team_roster(
    js: &JetStreamContext,
    parent_task_id: Uuid,
) -> Result<Vec<WorkerRecord>, async_nats::Error> {
    let kv = js.get_key_value(TEAM_KV_BUCKET).await?;
    let key = parent_task_id.to_string();
    (kv.get(&key).await?).map_or_else(
        || Ok(vec![]),
        |entry| {
            serde_json::from_slice(&entry).map_err(|e| {
                tracing::error!(?e, %parent_task_id, "corrupt team roster in KV, returning empty");
                Box::new(e) as Box<dyn std::error::Error + Send + Sync>
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_subject_contains_both_ids() {
        let parent = Uuid::nil();
        let child = Uuid::max();
        let s = worker_subject(parent, child);
        assert!(s.starts_with("roz.team."));
        assert!(s.contains(".worker."));
        assert!(s.contains(&parent.to_string()));
        assert!(s.contains(&child.to_string()));
    }

    #[test]
    fn team_subject_pattern_ends_with_wildcard() {
        let parent = Uuid::nil();
        let p = team_subject_pattern(parent);
        assert!(p.ends_with(".worker.>"));
        assert!(p.contains(&parent.to_string()));
    }

    #[test]
    fn team_sequence_key_uses_jetstream_safe_namespace() {
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let key = team_sequence_key(parent, child);
        assert!(key.starts_with("team."));
        assert!(key.ends_with(".seq"));
        assert!(!key.contains(':'));
        assert!(!key.contains('*'));
        assert!(!key.contains('>'));
    }

    #[test]
    fn worker_subject_no_nats_special_chars_in_ids() {
        // UUIDs contain only hex digits and hyphens — safe for NATS subjects
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        let s = worker_subject(parent, child);
        // Dots are the only NATS special chars that are valid in subjects (as separators)
        // Wildcards (* and >) must not appear in the subject (only in patterns)
        assert!(!s.contains('*'));
        assert!(!s.contains('>'));
    }
}
