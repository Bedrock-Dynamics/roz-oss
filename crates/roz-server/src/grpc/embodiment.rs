//! gRPC implementation of the `EmbodimentService` trait.
//!
//! Provides `GetModel`, `GetRuntime`, `ListBindings`, `ValidateBindings`,
//! `GetRetargetingMap`, and `GetManifest` RPCs backed by JSONB columns
//! on the `roz_hosts` table.

use sqlx::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::agent::GrpcAuth;
use crate::grpc::embodiment_convert::domain_binding_type_to_proto;
use crate::grpc::roz_v1::embodiment_service_server::EmbodimentService;
use crate::grpc::roz_v1::{
    EmbodimentModel as ProtoModel, EmbodimentRuntime as ProtoRuntime, GetManifestRequest, GetManifestResponse,
    GetModelRequest, GetRetargetingMapRequest, GetRetargetingMapResponse, GetRuntimeRequest, ListBindingsRequest,
    ListBindingsResponse, StreamFrameTreeRequest, StreamFrameTreeResponse, ValidateBindingsRequest,
    ValidateBindingsResponse, WatchCalibrationRequest, WatchCalibrationResponse,
};
use roz_core::embodiment::binding::{
    BindingType, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
};
use roz_core::embodiment::retargeting::RetargetingMap;

// ---------------------------------------------------------------------------
// Service implementation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Stream type aliases (Plan 02 will replace these with real implementations)
// ---------------------------------------------------------------------------

type StreamFrameTreeStream =
    tokio_stream::wrappers::ReceiverStream<Result<StreamFrameTreeResponse, tonic::Status>>;
type WatchCalibrationStream =
    tokio_stream::wrappers::ReceiverStream<Result<WatchCalibrationResponse, tonic::Status>>;

// ---------------------------------------------------------------------------
// Service implementation
// ---------------------------------------------------------------------------

/// gRPC implementation of the `EmbodimentService` trait.
///
/// Holds a database pool, auth injector, and optional NATS client for streaming RPCs.
pub struct EmbodimentServiceImpl {
    pool: PgPool,
    auth: std::sync::Arc<dyn GrpcAuth>,
    // Used by Plan 02 streaming RPC handlers (StreamFrameTree, WatchCalibration).
    #[expect(dead_code, reason = "consumed by Plan 02 streaming RPC handlers")]
    nats_client: Option<async_nats::Client>,
}

impl EmbodimentServiceImpl {
    pub fn new(
        pool: PgPool,
        auth: std::sync::Arc<dyn GrpcAuth>,
        nats_client: Option<async_nats::Client>,
    ) -> Self {
        Self { pool, auth, nats_client }
    }

    async fn authenticated_tenant_id<T: Sync>(&self, request: &Request<T>) -> Result<Uuid, Status> {
        let auth_header = request
            .metadata()
            .get("authorization")
            .ok_or_else(|| Status::unauthenticated("missing authorization metadata"))?
            .to_str()
            .map_err(|_| Status::invalid_argument("invalid authorization metadata"))?;

        let identity = self
            .auth
            .authenticate(&self.pool, Some(auth_header))
            .await
            .map_err(Status::unauthenticated)?;
        Ok(identity.tenant_id().0)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a `host_id` string as UUID, returning `INVALID_ARGUMENT` on failure.
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

#[expect(clippy::result_large_err, reason = "tonic Status is the return error type for all gRPC RPCs")]
#[tonic::async_trait]
impl EmbodimentService for EmbodimentServiceImpl {
    async fn get_model(&self, request: Request<GetModelRequest>) -> Result<Response<ProtoModel>, Status> {
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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

        let family = domain_model
            .embodiment_family
            .ok_or_else(|| Status::failed_precondition("host has no embodiment family -- retargeting requires a family classification"))?;

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
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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

    async fn stream_frame_tree(
        &self,
        _request: Request<StreamFrameTreeRequest>,
    ) -> Result<Response<Self::StreamFrameTreeStream>, Status> {
        Err(Status::unimplemented("StreamFrameTree not yet implemented -- see Plan 02"))
    }

    type WatchCalibrationStream = WatchCalibrationStream;

    async fn watch_calibration(
        &self,
        _request: Request<WatchCalibrationRequest>,
    ) -> Result<Response<Self::WatchCalibrationStream>, Status> {
        Err(Status::unimplemented("WatchCalibration not yet implemented -- see Plan 02"))
    }
}
