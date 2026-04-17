//! Phase 18 SKILL-07 / D-15: `SkillsService` implementation.
//!
//! Backed by `roz_db::skills` (PLAN-03) and `roz_core::skills` validators
//! (PLAN-02). Auth reuses the existing `grpc_auth_middleware` + extension
//! injection pattern (see `embodiment.rs`); tenant scoping is enforced by
//! RLS — every handler opens a transaction and calls `set_tenant_context`
//! before any query.
//!
//! Task 1 ships List/Get/Delete with full safety guards (page-size cap,
//! permission gate, RLS). Import/Export are stubbed and delivered in Task 2.

#![allow(clippy::result_large_err)]

use std::io::Read as _;
use std::path::Component;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt as _, PutPayload};
use roz_core::auth::{AuthIdentity, Permissions};
use roz_db::set_tenant_context;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::grpc::auth_ext;
use crate::grpc::roz_v1::skills_service_server::{SkillsService, SkillsServiceServer};
use crate::grpc::roz_v1::{
    DeleteSkillRequest, DeleteSkillResponse, ExportChunk, ExportRequest, GetSkillRequest, ImportChunk, ImportResponse,
    ListSkillsRequest, ListSkillsResponse, SkillDetail, SkillMeta, import_chunk,
};

// ---------------------------------------------------------------------------
// Service state
// ---------------------------------------------------------------------------

/// gRPC implementation of `SkillsService`.
///
/// Holds a database pool and a pluggable object store (PLAN-01 workspace
/// dep; the default backend is `object_store::local::LocalFileSystem`).
pub struct SkillsServiceImpl {
    pool: PgPool,
    object_store: Arc<dyn ObjectStore>,
}

impl SkillsServiceImpl {
    pub const fn new(pool: PgPool, object_store: Arc<dyn ObjectStore>) -> Self {
        Self { pool, object_store }
    }

    pub fn into_server(self) -> SkillsServiceServer<Self> {
        SkillsServiceServer::new(self)
    }
}

// ---------------------------------------------------------------------------
// Export stream alias (server-streaming Export, filled in Task 2)
// ---------------------------------------------------------------------------

type ExportStream = ReceiverStream<Result<ExportChunk, Status>>;

// ---------------------------------------------------------------------------
// SkillsService trait impl
// ---------------------------------------------------------------------------

#[tonic::async_trait]
impl SkillsService for SkillsServiceImpl {
    async fn list(&self, request: Request<ListSkillsRequest>) -> Result<Response<ListSkillsResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        // RESEARCH §Security: cap page_size at 100.
        #[allow(clippy::cast_possible_wrap)]
        let requested = i64::from(request.get_ref().page_size.max(1));
        let page_size = requested.clamp(1, 100);

        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let rows = roz_db::skills::list_recent(&mut *tx, page_size)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;

        let skills = rows.into_iter().map(summary_to_meta).collect();
        Ok(Response::new(ListSkillsResponse {
            skills,
            next_page_token: None,
        }))
    }

    async fn get(&self, request: Request<GetSkillRequest>) -> Result<Response<SkillDetail>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let GetSkillRequest { name, version } = request.into_inner();

        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let row = match version.as_deref() {
            Some(v) => roz_db::skills::get_by_name_version(&mut *tx, &name, v).await,
            None => roz_db::skills::get_latest_by_semver(&mut *tx, &name).await,
        }
        .map_err(internal)?
        .ok_or_else(|| Status::not_found(format!("skill {name} not found")))?;

        let prefix = object_prefix(&tenant_id, &row.name, &row.version);
        let asset_paths = list_asset_paths(self.object_store.as_ref(), &prefix)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "object_store list failed");
                Status::internal("object store error")
            })?;

        tx.commit().await.map_err(internal)?;

        let frontmatter_json = serde_json::to_string(&row.frontmatter).unwrap_or_default();
        let meta = row_to_meta(&row);
        Ok(Response::new(SkillDetail {
            meta: Some(meta),
            body_md: row.body_md,
            frontmatter_json,
            asset_paths,
        }))
    }

    async fn import(
        &self,
        request: Request<tonic::Streaming<ImportChunk>>,
    ) -> Result<Response<ImportResponse>, Status> {
        // RESEARCH §Security: 10 MB cumulative byte cap (never trust client's
        // ImportHeader.total_size_bytes). File-count cap per §Anti-Patterns.
        const MAX_TAR_BYTES: usize = 10 * 1024 * 1024;
        const MAX_FILES_PER_SKILL: usize = 1000;

        let identity = auth_ext::identity_from_extensions(&request)?.clone();
        let tenant_id = identity.tenant_id().0;
        let created_by = principal_label(&identity);
        let mut stream = request.into_inner();

        // 1. Drain stream into 10MB-capped buffer; header arrives first.
        let mut header_source: Option<String> = None;
        let mut buf = BytesMut::with_capacity(64 * 1024);
        while let Some(chunk) = stream.message().await.map_err(internal)? {
            match chunk.chunk {
                Some(import_chunk::Chunk::Header(h)) => {
                    if header_source.is_some() {
                        return Err(Status::invalid_argument("multiple headers in Import stream"));
                    }
                    header_source = Some(h.source);
                }
                Some(import_chunk::Chunk::TarGzBytes(b)) => {
                    if buf.len().saturating_add(b.len()) > MAX_TAR_BYTES {
                        return Err(Status::resource_exhausted(format!(
                            "tar.gz exceeds {MAX_TAR_BYTES} byte cap"
                        )));
                    }
                    buf.extend_from_slice(&b);
                }
                None => {}
            }
        }
        let source = header_source.ok_or_else(|| Status::invalid_argument("missing ImportHeader"))?;
        let bytes = buf.freeze();

        // 2. Extract + parse + scan in spawn_blocking (sync tar/flate2 APIs).
        //    RESEARCH §Standard Stack: run sync archive handling on blocking pool.
        let extracted = tokio::task::spawn_blocking(move || extract_and_scan(&bytes, MAX_FILES_PER_SKILL))
            .await
            .map_err(internal)?
            .map_err(|e| import_error_to_status(&e))?;

        // 3. DB insert first (composite PK enforces uniqueness — D-06).
        let frontmatter_json = serde_json::to_value(&extracted.fm).map_err(internal)?;
        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let row = match roz_db::skills::insert_skill(
            &mut *tx,
            &extracted.fm.name,
            &extracted.fm.version,
            &extracted.body_md,
            &frontmatter_json,
            &source,
            &created_by,
        )
        .await
        {
            Ok(r) => r,
            Err(sqlx::Error::Database(db_err)) if db_err.constraint() == Some("roz_skills_pkey") => {
                return Err(Status::already_exists(format!(
                    "skill {} v{} already exists (D-06: versions are immutable)",
                    extracted.fm.name, extracted.fm.version
                )));
            }
            Err(e) => return Err(internal(e)),
        };
        tx.commit().await.map_err(internal)?;

        // 4. Object-store fan-out. On failure, DB row is orphaned but harmless;
        //    operator can retry by deleting the row via Delete RPC.
        let tenant_prefix = tenant_id.to_string();
        let mut files_stored: u32 = 0;
        for (rel, data) in extracted.bundled {
            let path = ObjPath::from(format!(
                "{}/{}/{}/{}",
                tenant_prefix, extracted.fm.name, extracted.fm.version, rel
            ));
            self.object_store
                .put(&path, PutPayload::from_bytes(Bytes::from(data)))
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, %path, "object_store put failed");
                    Status::internal("object store write failed")
                })?;
            files_stored = files_stored.saturating_add(1);
        }

        Ok(Response::new(ImportResponse {
            meta: Some(row_to_meta(&row)),
            files_stored,
        }))
    }

    type ExportStream = ExportStream;

    async fn export(&self, request: Request<ExportRequest>) -> Result<Response<Self::ExportStream>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let ExportRequest { name, version } = request.into_inner();

        // Fetch the row first so we can reconstruct SKILL.md.
        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let row = match version.as_deref() {
            Some(v) => roz_db::skills::get_by_name_version(&mut *tx, &name, v).await,
            None => roz_db::skills::get_latest_by_semver(&mut *tx, &name).await,
        }
        .map_err(internal)?
        .ok_or_else(|| Status::not_found(format!("skill {name} not found")))?;
        tx.commit().await.map_err(internal)?;

        // Pull every bundled asset from the object store into memory.
        let prefix = object_prefix(&tenant_id, &row.name, &row.version);
        let assets = collect_prefix_bytes(self.object_store.as_ref(), &prefix)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "object_store fetch failed");
                Status::internal("object store read failed")
            })?;

        let (tx_chan, rx_chan) = mpsc::channel::<Result<ExportChunk, Status>>(8);
        let prefix_str = prefix.to_string();
        let skill_md = rebuild_skill_md(&row.frontmatter, &row.body_md);

        // Build the tar.gz on the blocking pool, streaming 64 KB frames out.
        tokio::task::spawn_blocking(move || {
            let result = build_tarball(&skill_md, &assets, &prefix_str);
            match result {
                Ok(bytes) => {
                    const FRAME: usize = 64 * 1024;
                    for chunk in bytes.chunks(FRAME) {
                        if tx_chan
                            .blocking_send(Ok(ExportChunk {
                                tar_gz_bytes: chunk.to_vec(),
                            }))
                            .is_err()
                        {
                            return; // client disconnected
                        }
                    }
                }
                Err(e) => {
                    let _ = tx_chan.blocking_send(Err(Status::internal(e.to_string())));
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx_chan)))
    }

    async fn delete(&self, request: Request<DeleteSkillRequest>) -> Result<Response<DeleteSkillResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        // D-10 / T-18-05-05: Delete requires `can_write_skills`.
        let perms = request.extensions().get::<Permissions>().cloned().unwrap_or_default();
        if !perms.can_write_skills {
            return Err(Status::permission_denied("Delete requires can_write_skills"));
        }

        let DeleteSkillRequest { name, version } = request.into_inner();
        let version_ref = version.as_deref();

        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let n = roz_db::skills::delete_skill(&mut *tx, &name, version_ref)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;

        // Best-effort object-store cleanup. DB is source of truth; orphaned
        // blobs are harmless (tenant-scoped prefix) and can be GC'd later.
        let prefix = version_ref.map_or_else(
            || ObjPath::from(format!("{tenant_id}/{name}/")),
            |v| ObjPath::from(format!("{tenant_id}/{name}/{v}/")),
        );
        if let Err(e) = best_effort_delete_prefix(self.object_store.as_ref(), &prefix).await {
            tracing::warn!(error = %e, %prefix, "best-effort object_store cleanup failed");
        }

        #[allow(clippy::cast_possible_truncation)]
        let versions_deleted = n as u32;
        Ok(Response::new(DeleteSkillResponse { versions_deleted }))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn internal(e: impl std::fmt::Display) -> Status {
    tracing::error!(error = %e, "skills service internal error");
    Status::internal("internal error")
}

/// `{tenant_id}/{name}/{version}/` prefix; tenant-prefix is the auth boundary
/// at the object layer (D-02 / T-18-05-01 defense in depth).
fn object_prefix(tenant_id: &uuid::Uuid, name: &str, version: &str) -> ObjPath {
    ObjPath::from(format!("{tenant_id}/{name}/{version}/"))
}

fn summary_to_meta(s: roz_db::skills::SkillSummary) -> SkillMeta {
    SkillMeta {
        name: s.name,
        version: s.version,
        description: s.description,
        created_at: Some(prost_types::Timestamp::from(std::time::SystemTime::from(s.created_at))),
        created_by: s.created_by,
        tags: Vec::new(),
    }
}

fn row_to_meta(r: &roz_db::skills::SkillRow) -> SkillMeta {
    let description = r
        .frontmatter
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tags = r
        .frontmatter
        .get("metadata")
        .and_then(|m| m.get("hermes"))
        .and_then(|h| h.get("tags"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    SkillMeta {
        name: r.name.clone(),
        version: r.version.clone(),
        description,
        created_at: Some(prost_types::Timestamp::from(std::time::SystemTime::from(r.created_at))),
        created_by: r.created_by.clone(),
        tags,
    }
}

async fn list_asset_paths(store: &dyn ObjectStore, prefix: &ObjPath) -> Result<Vec<String>, object_store::Error> {
    let mut stream = store.list(Some(prefix));
    let mut out = Vec::new();
    while let Some(meta) = stream.next().await {
        out.push(meta?.location.to_string());
    }
    Ok(out)
}

/// Delete every object under `prefix`. Swallows per-entry errors and returns
/// the first error, if any; missing entries are ignored.
async fn best_effort_delete_prefix(store: &dyn ObjectStore, prefix: &ObjPath) -> Result<(), object_store::Error> {
    let mut stream = store.list(Some(prefix));
    let mut first_err: Option<object_store::Error> = None;
    while let Some(meta) = stream.next().await {
        match meta {
            Ok(m) => {
                if let Err(e) = store.delete(&m.location).await {
                    tracing::warn!(error = %e, location = %m.location, "delete failed");
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "list entry error");
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    first_err.map_or(Ok(()), Err)
}

// ---------------------------------------------------------------------------
// Import helpers (scan-before-persist; zip-slip-safe tar extraction)
// ---------------------------------------------------------------------------

/// An inspected, validated, and threat-scanned skill bundle, ready to persist.
///
/// `_tmp` is held to keep the `TempDir` alive for the duration of any
/// filesystem-backed tar entries that might have been staged; in practice
/// we hold everything in-memory, but the handle is retained in case a
/// future extension uses the TempDir for large-asset staging.
struct ExtractedSkill {
    fm: roz_core::skills::frontmatter::SkillFrontmatter,
    body_md: String,
    bundled: Vec<(String, Vec<u8>)>,
    _tmp: tempfile::TempDir,
}

#[derive(Debug, thiserror::Error)]
enum ImportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("utf8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("path traversal in tar entry: {0}")]
    PathTraversal(String),
    #[error("too many files in skill bundle (max {0})")]
    TooManyFiles(usize),
    #[error("SKILL.md missing from tar")]
    MissingSkillMd,
    #[error("frontmatter: {0}")]
    Frontmatter(String),
    #[error("threat scan: {0}")]
    ThreatScan(String),
}

fn import_error_to_status(e: &ImportError) -> Status {
    match e {
        ImportError::PathTraversal(_)
        | ImportError::TooManyFiles(_)
        | ImportError::MissingSkillMd
        | ImportError::Frontmatter(_) => Status::invalid_argument(e.to_string()),
        ImportError::ThreatScan(_) => Status::failed_precondition(e.to_string()),
        ImportError::Io(_) | ImportError::Utf8(_) => Status::internal(e.to_string()),
    }
}

/// Extract a tar.gz, enforce zip-slip + file-count caps, parse+scan SKILL.md,
/// and threat-scan every UTF-8 bundled file BEFORE any persistence call site.
/// RESEARCH §Common Pitfalls 3 (path traversal) + 4 (scan-before-persist).
fn extract_and_scan(bytes: &Bytes, max_files: usize) -> Result<ExtractedSkill, ImportError> {
    let tmp = tempfile::TempDir::new()?;
    let gz = flate2::read::GzDecoder::new(bytes.as_ref());
    let mut archive = tar::Archive::new(gz);

    let mut skill_md_body: Option<String> = None;
    let mut bundled: Vec<(String, Vec<u8>)> = Vec::new();
    let mut file_count: usize = 0;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // T-18-05-01 zip-slip guard: reject any ParentDir / RootDir component.
        if path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            return Err(ImportError::PathTraversal(path.display().to_string()));
        }
        let rel = path.to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        file_count = file_count.saturating_add(1);
        if file_count > max_files {
            return Err(ImportError::TooManyFiles(max_files));
        }
        if rel == "SKILL.md" {
            skill_md_body = Some(String::from_utf8(data)?);
        } else {
            bundled.push((rel, data));
        }
    }

    let raw = skill_md_body.ok_or(ImportError::MissingSkillMd)?;

    // Parse + threat-scan BEFORE any write (Pitfall 4).
    let (fm, body_md) = roz_core::skills::parse_skill_md(&raw).map_err(|e| ImportError::Frontmatter(e.to_string()))?;
    roz_core::skills::scan_skill_content(&body_md).map_err(|k| ImportError::ThreatScan(format!("{k:?}")))?;
    for (_path, data) in &bundled {
        if let Ok(s) = std::str::from_utf8(data) {
            roz_core::skills::scan_skill_content(s).map_err(|k| ImportError::ThreatScan(format!("{k:?}")))?;
        }
    }

    Ok(ExtractedSkill {
        fm,
        body_md,
        bundled,
        _tmp: tmp,
    })
}

/// Map an `AuthIdentity` to a stable audit label for `roz_skills.created_by`.
fn principal_label(identity: &AuthIdentity) -> String {
    match identity {
        AuthIdentity::User { user_id, .. } => format!("user:{user_id}"),
        AuthIdentity::ApiKey { key_id, .. } => format!("apikey:{key_id}"),
        AuthIdentity::Worker { worker_id, .. } => format!("worker:{worker_id}"),
    }
}

// ---------------------------------------------------------------------------
// Export helpers (reconstruct SKILL.md; stream tar.gz)
// ---------------------------------------------------------------------------

/// Fetch every object under `prefix` into memory, paired with its path suffix
/// relative to the prefix. Used by Export to reassemble a skill bundle.
async fn collect_prefix_bytes(
    store: &dyn ObjectStore,
    prefix: &ObjPath,
) -> Result<Vec<(String, Vec<u8>)>, object_store::Error> {
    let mut stream = store.list(Some(prefix));
    let mut out = Vec::new();
    let prefix_str = prefix.to_string();
    while let Some(meta) = stream.next().await {
        let meta = meta?;
        let location = meta.location.clone();
        let full = location.to_string();
        let rel = full
            .strip_prefix(&prefix_str)
            .unwrap_or(&full)
            .trim_start_matches('/')
            .to_owned();
        let data = store.get(&location).await?.bytes().await?.to_vec();
        out.push((rel, data));
    }
    Ok(out)
}

/// Reconstruct a SKILL.md file from the DB frontmatter + body.
fn rebuild_skill_md(frontmatter: &serde_json::Value, body_md: &str) -> String {
    let yaml = serde_yaml::to_string(frontmatter).unwrap_or_else(|_| String::new());
    // `serde_yaml::to_string` already includes a trailing newline; trim to
    // avoid doubling when we splice between --- fences.
    let yaml = yaml.trim_end_matches('\n');
    format!("---\n{yaml}\n---\n{body_md}")
}

/// Build a gzipped tar containing SKILL.md + all bundled assets.
fn build_tarball(skill_md: &str, assets: &[(String, Vec<u8>)], prefix: &str) -> Result<Vec<u8>, std::io::Error> {
    let _ = prefix; // prefix is informational; tar entry paths are relative.
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        let mut skill_header = tar::Header::new_gnu();
        #[allow(clippy::cast_possible_truncation)]
        let skill_len = skill_md.len() as u64;
        skill_header.set_size(skill_len);
        skill_header.set_mode(0o644);
        skill_header.set_cksum();
        builder.append_data(&mut skill_header, "SKILL.md", skill_md.as_bytes())?;
        for (rel, data) in assets {
            let mut h = tar::Header::new_gnu();
            #[allow(clippy::cast_possible_truncation)]
            let len = data.len() as u64;
            h.set_size(len);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append_data(&mut h, rel, data.as_slice())?;
        }
        builder.finish()?;
    }
    encoder.finish()
}
