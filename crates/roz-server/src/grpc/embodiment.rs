//! gRPC implementation of the `EmbodimentService` trait.
//!
//! Provides `GetModel`, `GetRuntime`, `ListBindings`, `ValidateBindings`,
//! `GetRetargetingMap`, and `GetManifest` RPCs backed by JSONB columns
//! on the `roz_hosts` table.

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt as _;
use sha2::{Digest as Sha2Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::auth_ext;
use crate::grpc::embodiment_convert::domain_binding_type_to_proto;
use crate::grpc::roz_v1::embodiment_service_server::EmbodimentService;
use crate::grpc::roz_v1::{
    CalibrationDelta, CalibrationSnapshot, EmbodimentKeepalive, EmbodimentModel as ProtoModel,
    EmbodimentRuntime as ProtoRuntime, FrameTreeDelta, FrameTreeSnapshot, GetManifestRequest, GetManifestResponse,
    GetModelRequest, GetRetargetingMapRequest, GetRetargetingMapResponse, GetRuntimeRequest, ListBindingsRequest,
    ListBindingsResponse, StreamFrameTreeRequest, StreamFrameTreeResponse, ValidateBindingsRequest,
    ValidateBindingsResponse, WatchCalibrationRequest, WatchCalibrationResponse,
};
use crate::grpc::roz_v1::{
    stream_frame_tree_response::Payload as FrameTreePayload, watch_calibration_response::Payload as CalibrationPayload,
};
use roz_core::embodiment::binding::{BindingType, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest};
use roz_core::embodiment::retargeting::RetargetingMap;

// ---------------------------------------------------------------------------
// Service implementation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Stream type aliases (Plan 02 will replace these with real implementations)
// ---------------------------------------------------------------------------

type StreamFrameTreeStream = tokio_stream::wrappers::ReceiverStream<Result<StreamFrameTreeResponse, tonic::Status>>;
type WatchCalibrationStream = tokio_stream::wrappers::ReceiverStream<Result<WatchCalibrationResponse, tonic::Status>>;

// ---------------------------------------------------------------------------
// Service implementation
// ---------------------------------------------------------------------------

/// gRPC implementation of the `EmbodimentService` trait.
///
/// Holds a database pool and optional NATS client for streaming RPCs.
/// Authentication is provided structurally by the `grpc_auth_middleware`
/// layer; this service reads `AuthIdentity` from request extensions.
pub struct EmbodimentServiceImpl {
    pool: PgPool,
    nats_client: Option<async_nats::Client>,
}

impl EmbodimentServiceImpl {
    pub const fn new(pool: PgPool, nats_client: Option<async_nats::Client>) -> Self {
        Self { pool, nats_client }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `host_id` string as UUID, returning `INVALID_ARGUMENT` on failure.
#[allow(clippy::result_large_err)]
fn parse_host_id(host_id: &str) -> Result<Uuid, Status> {
    host_id
        .parse::<Uuid>()
        .map_err(|_| Status::invalid_argument("host_id is not a valid UUID"))
}

/// Fetch the embodiment row, enforcing tenant isolation.
/// Returns `NOT_FOUND` if the host doesn't exist or belongs to a different tenant.
async fn fetch_embodiment_row(
    pool: &PgPool,
    host_id: Uuid,
    tenant_id: Uuid,
) -> Result<roz_db::embodiments::EmbodimentRow, Status> {
    let row = roz_db::embodiments::get_by_host_id(pool, host_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "database error fetching embodiment");
            Status::internal("database error")
        })?
        .ok_or_else(|| Status::not_found("host not found"))?;

    // T-04-01: tenant isolation -- return NOT_FOUND (not FORBIDDEN) to avoid
    // leaking host existence across tenants.
    if row.tenant_id != tenant_id {
        return Err(Status::not_found("host not found"));
    }

    Ok(row)
}

/// Synthesize a `ControlInterfaceManifest` from channel bindings.
///
/// The manifest is not stored in the DB -- it's constructed from the model's
/// channel bindings. The `BindingType` -> `CommandInterfaceType` mapping is
/// best-effort: `Command` maps to `JointPosition` and all three IMU binding
/// types collapse to `ImuSensor`.
fn synthesize_manifest(bindings: &[roz_core::embodiment::binding::ChannelBinding]) -> ControlInterfaceManifest {
    let channels: Vec<ControlChannelDef> = bindings
        .iter()
        .map(|b| ControlChannelDef {
            name: b.physical_name.clone(),
            interface_type: binding_type_to_command_interface(&b.binding_type),
            units: b.units.clone(),
            frame_id: b.frame_id.clone(),
        })
        .collect();

    let mut manifest = ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels,
        bindings: bindings.to_vec(),
    };
    manifest.stamp_digest();
    manifest
}

/// Best-effort mapping from `BindingType` to `CommandInterfaceType`.
///
/// This is lossy: `Command` maps to `JointPosition` as fallback, and all
/// three IMU binding types (`ImuOrientation`, `ImuAngularVelocity`,
/// `ImuLinearAcceleration`) collapse to `ImuSensor`.
const fn binding_type_to_command_interface(bt: &BindingType) -> CommandInterfaceType {
    match bt {
        // Command falls back to JointPosition (lossy -- see doc comment).
        BindingType::JointPosition | BindingType::Command => CommandInterfaceType::JointPosition,
        BindingType::JointVelocity => CommandInterfaceType::JointVelocity,
        BindingType::ForceTorque => CommandInterfaceType::ForceTorqueSensor,
        BindingType::GripperPosition => CommandInterfaceType::GripperPosition,
        BindingType::GripperForce => CommandInterfaceType::GripperForce,
        BindingType::ImuOrientation | BindingType::ImuAngularVelocity | BindingType::ImuLinearAcceleration => {
            CommandInterfaceType::ImuSensor
        }
    }
}

// ---------------------------------------------------------------------------
// EmbodimentService trait impl
// ---------------------------------------------------------------------------

#[allow(clippy::result_large_err)]
#[tonic::async_trait]
impl EmbodimentService for EmbodimentServiceImpl {
    async fn get_model(&self, request: Request<GetModelRequest>) -> Result<Response<ProtoModel>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let host_id = parse_host_id(&request.get_ref().host_id)?;

        let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;

        let model_json = row
            .embodiment_model
            .ok_or_else(|| Status::failed_precondition("host has no embodiment model"))?;

        let domain_model: roz_core::embodiment::model::EmbodimentModel =
            serde_json::from_value(model_json).map_err(|e| {
                tracing::error!(error = %e, host_id = %host_id, "corrupt model data");
                Status::internal("failed to deserialize embodiment data")
            })?;

        Ok(Response::new(ProtoModel::from(&domain_model)))
    }

    async fn get_runtime(&self, request: Request<GetRuntimeRequest>) -> Result<Response<ProtoRuntime>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let host_id = parse_host_id(&request.get_ref().host_id)?;

        let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;

        let runtime_json = row
            .embodiment_runtime
            .ok_or_else(|| Status::failed_precondition("host has no embodiment runtime"))?;

        let domain_runtime: roz_core::embodiment::embodiment_runtime::EmbodimentRuntime =
            serde_json::from_value(runtime_json).map_err(|e| {
                tracing::error!(error = %e, host_id = %host_id, "corrupt runtime data");
                Status::internal("failed to deserialize embodiment data")
            })?;

        Ok(Response::new(ProtoRuntime::from(&domain_runtime)))
    }

    async fn list_bindings(
        &self,
        request: Request<ListBindingsRequest>,
    ) -> Result<Response<ListBindingsResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let host_id = parse_host_id(&request.get_ref().host_id)?;

        let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;

        let model_json = row
            .embodiment_model
            .ok_or_else(|| Status::failed_precondition("host has no embodiment model"))?;

        let domain_model: roz_core::embodiment::model::EmbodimentModel =
            serde_json::from_value(model_json).map_err(|e| {
                tracing::error!(error = %e, host_id = %host_id, "corrupt model data");
                Status::internal("failed to deserialize embodiment data")
            })?;

        let bindings = domain_model
            .channel_bindings
            .iter()
            .map(crate::grpc::roz_v1::ChannelBinding::from)
            .collect();

        Ok(Response::new(ListBindingsResponse { bindings }))
    }

    async fn validate_bindings(
        &self,
        request: Request<ValidateBindingsRequest>,
    ) -> Result<Response<ValidateBindingsResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let host_id = parse_host_id(&request.get_ref().host_id)?;

        let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;

        let runtime_json = row
            .embodiment_runtime
            .ok_or_else(|| Status::failed_precondition("host has no embodiment runtime"))?;

        let domain_runtime: roz_core::embodiment::embodiment_runtime::EmbodimentRuntime =
            serde_json::from_value(runtime_json).map_err(|e| {
                tracing::error!(error = %e, host_id = %host_id, "corrupt runtime data");
                Status::internal("failed to deserialize embodiment data")
            })?;

        let joint_names: Vec<&str> = domain_runtime.model.joints.iter().map(|j| j.name.as_str()).collect();
        let sensor_ids: Vec<&str> = domain_runtime
            .model
            .sensor_mounts
            .iter()
            .map(|s| s.sensor_id.as_str())
            .collect();
        let frame_ids = domain_runtime.model.frame_tree.all_frame_ids();
        let channel_count = u32::try_from(domain_runtime.model.channel_bindings.len()).unwrap_or(u32::MAX);

        let unbound = roz_core::embodiment::binding::validate_bindings(
            &domain_runtime.model.channel_bindings,
            &joint_names,
            &sensor_ids,
            &frame_ids,
            channel_count,
        );

        let unbound_channels = unbound
            .into_iter()
            .map(|uc| crate::grpc::roz_v1::UnboundChannel {
                physical_name: uc.physical_name,
                binding_type: domain_binding_type_to_proto(&uc.binding_type),
                reason: uc.reason,
            })
            .collect::<Vec<_>>();

        Ok(Response::new(ValidateBindingsResponse {
            valid: unbound_channels.is_empty(),
            unbound_channels,
        }))
    }

    async fn get_retargeting_map(
        &self,
        request: Request<GetRetargetingMapRequest>,
    ) -> Result<Response<GetRetargetingMapResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let host_id = parse_host_id(&request.get_ref().host_id)?;

        let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;

        let model_json = row
            .embodiment_model
            .ok_or_else(|| Status::failed_precondition("host has no embodiment model"))?;

        let domain_model: roz_core::embodiment::model::EmbodimentModel =
            serde_json::from_value(model_json).map_err(|e| {
                tracing::error!(error = %e, host_id = %host_id, "corrupt model data");
                Status::internal("failed to deserialize embodiment data")
            })?;

        let family = domain_model.embodiment_family.ok_or_else(|| {
            Status::failed_precondition("host has no embodiment family -- retargeting requires a family classification")
        })?;

        let retargeting_map = RetargetingMap::from_bindings(family, &domain_model.channel_bindings);
        let mapped_count = u32::try_from(retargeting_map.canonical_to_local.len()).unwrap_or(u32::MAX);
        let total_binding_count = u32::try_from(domain_model.channel_bindings.len()).unwrap_or(u32::MAX);

        Ok(Response::new(GetRetargetingMapResponse {
            retargeting_map: Some(crate::grpc::roz_v1::RetargetingMap::from(&retargeting_map)),
            mapped_count,
            total_binding_count,
        }))
    }

    async fn get_manifest(
        &self,
        request: Request<GetManifestRequest>,
    ) -> Result<Response<GetManifestResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let host_id = parse_host_id(&request.get_ref().host_id)?;

        let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;

        let model_json = row
            .embodiment_model
            .ok_or_else(|| Status::failed_precondition("host has no embodiment model"))?;

        let domain_model: roz_core::embodiment::model::EmbodimentModel =
            serde_json::from_value(model_json).map_err(|e| {
                tracing::error!(error = %e, host_id = %host_id, "corrupt model data");
                Status::internal("failed to deserialize embodiment data")
            })?;

        let manifest = synthesize_manifest(&domain_model.channel_bindings);

        Ok(Response::new(GetManifestResponse {
            manifest: Some(crate::grpc::roz_v1::ControlInterfaceManifest::from(&manifest)),
        }))
    }

    type StreamFrameTreeStream = StreamFrameTreeStream;

    #[allow(clippy::too_many_lines)]
    async fn stream_frame_tree(
        &self,
        request: Request<StreamFrameTreeRequest>,
    ) -> Result<Response<Self::StreamFrameTreeStream>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let host_id = parse_host_id(&request.get_ref().host_id)?;

        // D-02: NATS required for streaming RPCs.
        let nats = self
            .nats_client
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("frame tree streaming requires NATS"))?
            .clone();

        // Subscribe BEFORE reading initial state to avoid losing events that arrive
        // between the DB read and subscribe call (subscribe-before-read race avoidance).
        let subject = roz_nats::dispatch::embodiment_changed_subject(host_id);
        let mut sub = nats
            .subscribe(subject)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "NATS subscribe failed");
                Status::internal("NATS subscribe failed")
            })?;

        // D-04: Send initial full snapshot.
        let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;
        let model: roz_core::embodiment::model::EmbodimentModel = serde_json::from_value(
            row.embodiment_model
                .ok_or_else(|| Status::not_found("host has no embodiment model"))?,
        )
        .map_err(|e| {
            tracing::error!(error = %e, "failed to deserialize embodiment model");
            Status::internal("failed to deserialize embodiment data")
        })?;

        let initial_digest = compute_frame_tree_digest(&model.frame_tree);

        let (tx, rx) = mpsc::channel::<Result<StreamFrameTreeResponse, Status>>(32);

        let initial = StreamFrameTreeResponse {
            host_id: host_id.to_string(),
            digest: initial_digest.clone(),
            payload: Some(FrameTreePayload::Snapshot(FrameTreeSnapshot {
                frame_tree: Some(crate::grpc::roz_v1::FrameTree::from(&model.frame_tree)),
            })),
        };
        let _ = tx.send(Ok(initial)).await;

        let pool = self.pool.clone();
        tokio::spawn(async move {
            // WR-04: bail out after this many consecutive deserialize failures.
            // A poisoned/legacy payload on the subject would otherwise keep
            // tripping the same bug forever.
            const MAX_CONSECUTIVE_DESERIALIZE_FAILURES: u32 = 10;
            let mut consecutive_deserialize_failures: u32 = 0;
            let mut last_digest = initial_digest;
            let mut last_tree = model.frame_tree;
            let mut keepalive_interval = tokio::time::interval(Duration::from_secs(15));
            keepalive_interval.tick().await; // consume the immediate first tick

            loop {
                tokio::select! {
                    msg = sub.next() => {
                        if let Some(nats_msg) = msg {
                            let event: roz_nats::dispatch::EmbodimentChangedEvent =
                                match serde_json::from_slice(&nats_msg.payload) {
                                    Ok(e) => {
                                        consecutive_deserialize_failures = 0;
                                        e
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "failed to deserialize EmbodimentChangedEvent");
                                        consecutive_deserialize_failures += 1;
                                        if consecutive_deserialize_failures >= MAX_CONSECUTIVE_DESERIALIZE_FAILURES {
                                            tracing::error!(
                                                failures = consecutive_deserialize_failures,
                                                "too many consecutive deserialize failures; closing frame tree stream"
                                            );
                                            let _ = tx
                                                .send(Err(Status::internal("event stream corrupted")))
                                                .await;
                                            break;
                                        }
                                        continue;
                                    }
                                };
                            // Multi-tenant safety: only process events for this tenant's host.
                            if event.tenant_id != tenant_id {
                                continue;
                            }

                            // Re-read from DB. DB is the source of truth.
                            let row = match roz_db::embodiments::get_by_host_id(&pool, host_id).await {
                                Ok(Some(r)) => r,
                                Ok(None) => { tracing::warn!(%host_id, "host disappeared"); continue; }
                                Err(e) => { tracing::warn!(error = %e, "DB read error"); continue; }
                            };
                            let new_model: roz_core::embodiment::model::EmbodimentModel =
                                match serde_json::from_value(match row.embodiment_model {
                                    Some(v) => v,
                                    None => { continue; }
                                }) {
                                    Ok(m) => m,
                                    Err(e) => { tracing::warn!(error = %e, "deserialize error"); continue; }
                                };

                            let new_digest = compute_frame_tree_digest(&new_model.frame_tree);
                            if new_digest == last_digest {
                                // No frame-tree change (e.g., only channel_bindings changed). Skip.
                                continue;
                            }

                            let delta = compute_frame_tree_delta(&last_tree, &new_model.frame_tree);
                            let response = StreamFrameTreeResponse {
                                host_id: host_id.to_string(),
                                digest: new_digest.clone(),
                                payload: Some(FrameTreePayload::Delta(delta)),
                            };
                            if tx.send(Ok(response)).await.is_err() {
                                break; // client disconnected
                            }
                            last_digest = new_digest;
                            last_tree = new_model.frame_tree;
                        } else {
                            // D-10: NATS subscription closed unexpectedly.
                            // Send explicit error item so client receives Status::internal.
                            let _ = tx.send(Err(Status::internal("NATS subscription closed"))).await;
                            break;
                        }
                    }
                    _ = keepalive_interval.tick() => {
                        // D-06: periodic keepalive with current digest.
                        let keepalive = StreamFrameTreeResponse {
                            host_id: host_id.to_string(),
                            digest: last_digest.clone(),
                            payload: Some(FrameTreePayload::Keepalive(EmbodimentKeepalive {
                                server_time: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
                                digest: last_digest.clone(),
                            })),
                        };
                        if tx.send(Ok(keepalive)).await.is_err() {
                            break; // client disconnected
                        }
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type WatchCalibrationStream = WatchCalibrationStream;

    #[allow(clippy::too_many_lines)]
    async fn watch_calibration(
        &self,
        request: Request<WatchCalibrationRequest>,
    ) -> Result<Response<Self::WatchCalibrationStream>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let host_id = parse_host_id(&request.get_ref().host_id)?;

        // D-02: NATS required.
        let nats = self
            .nats_client
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("calibration streaming requires NATS"))?
            .clone();

        // Subscribe before DB read (subscribe-before-read race avoidance).
        let subject = roz_nats::dispatch::embodiment_changed_subject(host_id);
        let mut sub = nats
            .subscribe(subject)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "NATS subscribe failed");
                Status::internal("NATS subscribe failed")
            })?;

        // D-04: Send initial full calibration snapshot.
        let row = fetch_embodiment_row(&self.pool, host_id, tenant_id).await?;
        let runtime: roz_core::embodiment::embodiment_runtime::EmbodimentRuntime = serde_json::from_value(
            row.embodiment_runtime
                .ok_or_else(|| Status::not_found("host has no embodiment runtime"))?,
        )
        .map_err(|e| {
            tracing::error!(error = %e, "failed to deserialize embodiment runtime");
            Status::internal("failed to deserialize embodiment data")
        })?;

        let calibration = runtime
            .calibration
            .ok_or_else(|| Status::not_found("host has no calibration data"))?;

        // Use canonical CalibrationOverlay::compute_digest() -- do NOT reinvent SHA-256.
        let initial_digest = calibration.compute_digest();

        let (tx, rx) = mpsc::channel::<Result<WatchCalibrationResponse, Status>>(32);

        let initial = WatchCalibrationResponse {
            host_id: host_id.to_string(),
            digest: initial_digest.clone(),
            payload: Some(CalibrationPayload::Snapshot(CalibrationSnapshot {
                calibration: Some(crate::grpc::roz_v1::CalibrationOverlay::from(&calibration)),
            })),
        };
        let _ = tx.send(Ok(initial)).await;

        let pool = self.pool.clone();
        tokio::spawn(async move {
            // WR-04: bail out after this many consecutive deserialize failures.
            // A poisoned/legacy payload on the subject would otherwise keep
            // tripping the same bug forever.
            const MAX_CONSECUTIVE_DESERIALIZE_FAILURES: u32 = 10;
            let mut consecutive_deserialize_failures: u32 = 0;
            let mut last_digest = initial_digest;
            let mut keepalive_interval = tokio::time::interval(Duration::from_secs(15));
            keepalive_interval.tick().await; // consume immediate first tick

            loop {
                tokio::select! {
                    msg = sub.next() => {
                        if let Some(nats_msg) = msg {
                            let event: roz_nats::dispatch::EmbodimentChangedEvent =
                                match serde_json::from_slice(&nats_msg.payload) {
                                    Ok(e) => {
                                        consecutive_deserialize_failures = 0;
                                        e
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "failed to deserialize EmbodimentChangedEvent");
                                        consecutive_deserialize_failures += 1;
                                        if consecutive_deserialize_failures >= MAX_CONSECUTIVE_DESERIALIZE_FAILURES {
                                            tracing::error!(
                                                failures = consecutive_deserialize_failures,
                                                "too many consecutive deserialize failures; closing calibration stream"
                                            );
                                            let _ = tx
                                                .send(Err(Status::internal("event stream corrupted")))
                                                .await;
                                            break;
                                        }
                                        continue;
                                    }
                                };
                            if event.tenant_id != tenant_id {
                                continue;
                            }

                            let row = match roz_db::embodiments::get_by_host_id(&pool, host_id).await {
                                Ok(Some(r)) => r,
                                Ok(None) => { tracing::warn!(%host_id, "host disappeared"); continue; }
                                Err(e) => { tracing::warn!(error = %e, "DB read error"); continue; }
                            };

                            let runtime: roz_core::embodiment::embodiment_runtime::EmbodimentRuntime =
                                match serde_json::from_value(match row.embodiment_runtime {
                                    Some(v) => v,
                                    None => { continue; } // runtime removed; no delta to send
                                }) {
                                    Ok(r) => r,
                                    Err(e) => { tracing::warn!(error = %e, "deserialize runtime"); continue; }
                                };

                            let Some(new_calibration) = runtime.calibration else { continue; }; // calibration removed; no delta

                            // Use canonical compute_digest() -- no custom hash.
                            let new_digest = new_calibration.compute_digest();
                            if new_digest == last_digest {
                                continue; // no change
                            }

                            // Whole-overlay replacement delta (D-05, STRM-04).
                            let response = WatchCalibrationResponse {
                                host_id: host_id.to_string(),
                                digest: new_digest.clone(),
                                payload: Some(CalibrationPayload::Delta(CalibrationDelta {
                                    calibration: Some(crate::grpc::roz_v1::CalibrationOverlay::from(&new_calibration)),
                                })),
                            };
                            if tx.send(Ok(response)).await.is_err() {
                                break; // client disconnected
                            }
                            last_digest = new_digest;
                        } else {
                            // D-10: NATS subscription closed unexpectedly.
                            let _ = tx.send(Err(Status::internal("NATS subscription closed"))).await;
                            break;
                        }
                    }
                    _ = keepalive_interval.tick() => {
                        let keepalive = WatchCalibrationResponse {
                            host_id: host_id.to_string(),
                            digest: last_digest.clone(),
                            payload: Some(CalibrationPayload::Keepalive(EmbodimentKeepalive {
                                server_time: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
                                digest: last_digest.clone(),
                            })),
                        };
                        if tx.send(Ok(keepalive)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Compute a SHA-256 digest of the `FrameTree` payload alone.
///
/// MUST NOT delegate to `EmbodimentModel::compute_digest()`: that function hashes
/// the full model, so a `channel_bindings` change would flip the digest while the
/// frame tree payload is unchanged — clients would see digest mismatches with no
/// corresponding delta.
fn compute_frame_tree_digest(tree: &roz_core::embodiment::frame_tree::FrameTree) -> String {
    let json = serde_json::to_vec(tree).expect("FrameTree serialization cannot fail");
    let mut hasher = Sha256::new();
    hasher.update(&json);
    format!("{:x}", hasher.finalize())
}

/// Compute the `FrameTreeDelta` between two `FrameTree`s.
fn compute_frame_tree_delta(
    old: &roz_core::embodiment::frame_tree::FrameTree,
    new: &roz_core::embodiment::frame_tree::FrameTree,
) -> FrameTreeDelta {
    use std::collections::BTreeMap;

    let old_ids: HashSet<&str> = old.all_frame_ids().into_iter().collect();
    let new_ids: HashSet<&str> = new.all_frame_ids().into_iter().collect();

    let mut changed_frames = BTreeMap::new();
    for id in &new_ids {
        let new_node = new.get_frame(id).expect("frame exists in new tree");
        match old.get_frame(id) {
            Some(old_node) if old_node == new_node => {}
            _ => {
                changed_frames.insert((*id).to_string(), crate::grpc::roz_v1::FrameNode::from(new_node));
            }
        }
    }

    let removed_frame_ids: Vec<String> = old_ids.difference(&new_ids).map(|id| (*id).to_string()).collect();

    let new_root = if old.root() == new.root() {
        None
    } else {
        new.root().map(String::from)
    };

    FrameTreeDelta {
        changed_frames,
        removed_frame_ids,
        new_root,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::embodiment::frame_tree::{FrameSource, FrameTree, Transform3D};

    #[test]
    fn delta_no_changes() {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        let delta = compute_frame_tree_delta(&tree, &tree);
        assert!(delta.changed_frames.is_empty());
        assert!(delta.removed_frame_ids.is_empty());
        assert!(delta.new_root.is_none());
    }

    #[test]
    fn delta_added_frame() {
        let mut old = FrameTree::new();
        old.set_root("world", FrameSource::Static);
        let mut new = old.clone();
        new.add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();
        let delta = compute_frame_tree_delta(&old, &new);
        assert!(delta.changed_frames.contains_key("base"));
        assert!(delta.removed_frame_ids.is_empty());
    }

    #[test]
    fn delta_removed_frame() {
        let mut old = FrameTree::new();
        old.set_root("world", FrameSource::Static);
        old.add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();
        let mut new = FrameTree::new();
        new.set_root("world", FrameSource::Static);
        let delta = compute_frame_tree_delta(&old, &new);
        assert!(delta.changed_frames.is_empty());
        assert!(delta.removed_frame_ids.contains(&"base".to_string()));
    }

    #[test]
    fn delta_changed_root() {
        let mut old = FrameTree::new();
        old.set_root("world", FrameSource::Static);
        let mut new = FrameTree::new();
        new.set_root("base_link", FrameSource::Static);
        let delta = compute_frame_tree_delta(&old, &new);
        assert_eq!(delta.new_root.as_deref(), Some("base_link"));
    }

    #[test]
    fn frame_tree_digest_stable_for_identical_trees() {
        let mut a = FrameTree::new();
        a.set_root("world", FrameSource::Static);
        a.add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();
        let b = a.clone();
        assert_eq!(compute_frame_tree_digest(&a), compute_frame_tree_digest(&b));
    }

    #[test]
    fn frame_tree_digest_changes_when_tree_changes() {
        let mut old = FrameTree::new();
        old.set_root("world", FrameSource::Static);
        let mut new = old.clone();
        new.add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();
        assert_ne!(compute_frame_tree_digest(&old), compute_frame_tree_digest(&new));
    }
}
