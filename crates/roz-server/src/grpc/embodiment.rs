#![allow(clippy::result_large_err)]

//! gRPC implementation of the `EmbodimentService` trait.
//!
//! Provides `GetModel`, `GetRuntime`, `ListBindings`, and `ValidateBindings` RPCs
//! backed by JSONB columns on the `roz_hosts` table.

use sqlx::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::agent::GrpcAuth;
use crate::grpc::embodiment_convert::domain_binding_type_to_proto;
use crate::grpc::roz_v1::embodiment_service_server::EmbodimentService;
use crate::grpc::roz_v1::{
    EmbodimentModel as ProtoModel, EmbodimentRuntime as ProtoRuntime, GetModelRequest, GetRuntimeRequest,
    ListBindingsRequest, ListBindingsResponse, ValidateBindingsRequest, ValidateBindingsResponse,
};

// ---------------------------------------------------------------------------
// Service implementation
// ---------------------------------------------------------------------------

/// gRPC implementation of the `EmbodimentService` trait.
///
/// Holds only a database pool and auth injector -- no NATS, HTTP client, or
/// Restate needed since embodiment queries are purely DB-backed.
pub struct EmbodimentServiceImpl {
    pool: PgPool,
    auth: std::sync::Arc<dyn GrpcAuth>,
}

impl EmbodimentServiceImpl {
    pub const fn new(pool: PgPool, auth: std::sync::Arc<dyn GrpcAuth>) -> Self {
        Self { pool, auth }
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

// ---------------------------------------------------------------------------
// EmbodimentService trait impl
// ---------------------------------------------------------------------------

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
}
