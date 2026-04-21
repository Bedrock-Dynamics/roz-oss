//! Phase 26 OBS-01/02/03: unified per-session MCAP observability.
//!
//! This module houses:
//!   * `mcap_archive`    — per-session `WriterActor` (Wave 3)
//!   * `channels`        — up-front 6-channel registration (Wave 3)
//!   * `projection`      — `TimestampedTransform` → Foxglove `FrameTransform`,
//!     pose → `PoseInFrame`, `SessionEvent` → `Log` summary
//!   * `schema_registry` — `FileDescriptorSet` → per-message descriptor bytes
//!   * `task_lifecycle`  — `TaskLifecycleSink` broadcast + emit helper
//!   * `recovery`        — copy-partial-to-fresh startup scan (Wave 8)
//!   * `retention`       — `ROZ_MCAP_MAX_BYTES` + TTL FIFO sweep (Wave 8)
//!   * `rollover`        — hot-swap Writer at `ROZ_MCAP_MAX_FILE_BYTES` (Wave 5)
//!   * `idle_monitor`    — finalize after `ROZ_MCAP_IDLE_TIMEOUT_SECS` (Wave 5)
//!   * `export`          — time-range seek + gRPC streaming (Wave 7)

pub mod channels;
pub mod mcap_archive;
pub mod projection;
pub mod schema_registry;
pub mod task_lifecycle;
// Later-wave modules: pub mod recovery; pub mod retention; pub mod rollover;
// pub mod idle_monitor; pub mod export;

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Channel topic constants — MCAP message topics per OBS-01.
// ---------------------------------------------------------------------------

pub const CHANNEL_TF: &str = "/tf";
pub const CHANNEL_POSE: &str = "/roz/telemetry/pose";
pub const CHANNEL_LOG: &str = "/roz/log";
pub const CHANNEL_SESSION_EVENTS: &str = "/roz/session/events";
pub const CHANNEL_TASK_LIFECYCLE: &str = "/roz/task/lifecycle";
pub const CHANNEL_TOOL_CALLS: &str = "/roz/tool/calls";

// ---------------------------------------------------------------------------
// Schema name constants — `mcap::Writer::add_schema` `name` parameter.
// Must match FQN in the vendored/roz .proto files.
// ---------------------------------------------------------------------------

pub const SCHEMA_FRAME_TRANSFORM: &str = "foxglove.FrameTransform";
pub const SCHEMA_POSE_IN_FRAME: &str = "foxglove.PoseInFrame";
pub const SCHEMA_LOG: &str = "foxglove.Log";
pub const SCHEMA_SESSION_EVENT: &str = "roz.v1.SessionEventEnvelope";
pub const SCHEMA_TASK_LIFECYCLE: &str = "roz.v1.TaskLifecycleEvent";
pub const SCHEMA_TOOL_CALL: &str = "roz.v1.ToolCallEvent";

pub const SCHEMA_ENCODING_PROTOBUF: &str = "protobuf";

// ---------------------------------------------------------------------------
// Env var names — D-01/D-02/D-03/D-05.
// ---------------------------------------------------------------------------

pub const ENV_MCAP_DIR: &str = "ROZ_MCAP_DIR";
pub const ENV_MCAP_MAX_BYTES: &str = "ROZ_MCAP_MAX_BYTES";
pub const ENV_MCAP_TTL_SECS: &str = "ROZ_MCAP_TTL_SECS";
pub const ENV_MCAP_MAX_FILE_BYTES: &str = "ROZ_MCAP_MAX_FILE_BYTES";
pub const ENV_MCAP_IDLE_TIMEOUT_SECS: &str = "ROZ_MCAP_IDLE_TIMEOUT_SECS";

// Defaults per D-01/D-02/D-03/D-05.
pub const DEFAULT_MCAP_DIR: &str = "/var/lib/roz/mcap";
pub const DEFAULT_MCAP_MAX_BYTES: u64 = 10_000_000_000; // 10 GB
pub const DEFAULT_MCAP_TTL_SECS: u64 = 604_800; // 7 days
pub const DEFAULT_MCAP_MAX_FILE_BYTES: u64 = 1_000_000_000; // 1 GB
pub const DEFAULT_MCAP_IDLE_TIMEOUT_SECS: u64 = 600; // 10 min

// ---------------------------------------------------------------------------
// Library-level error type.
// ---------------------------------------------------------------------------

/// Errors produced by the per-session MCAP observability subsystem.
///
/// Covers low-level MCAP writer faults, I/O during archive file handling,
/// prost codec failures against Foxglove/roz-v1 schemas, durable-store
/// lookups, tenant boundary violations, path-traversal guard trips, and
/// lookup-by-schema-name misses against the descriptor registry.
#[derive(Debug, thiserror::Error)]
pub enum McapArchiveError {
    #[error("mcap write failed: {0}")]
    McapWrite(#[from] mcap::McapError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("prost decode: {0}")]
    ProstDecode(#[from] prost::DecodeError),
    #[error("prost encode: {0}")]
    ProstEncode(#[from] prost::EncodeError),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("archive not found for session {0}")]
    ArchiveNotFound(Uuid),
    #[error("tenant scope violation: archive belongs to {archive_tenant}, caller is {caller}")]
    CrossTenantAccess { archive_tenant: Uuid, caller: Uuid },
    #[error("path traversal detected: {0}")]
    PathTraversal(String),
    #[error("schema {0} not found in descriptor set")]
    SchemaNotFound(String),
}
