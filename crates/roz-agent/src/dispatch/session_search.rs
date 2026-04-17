//! MEM-07: `session_search` tool — Postgres FTS over this tenant's session turns.
//!
//! `Pure` category. Read-only. Tenant-scoped via RLS + explicit JOIN on
//! `roz_agent_sessions.tenant_id` (see `roz_db::session_turns::search_by_tsquery`).

use async_trait::async_trait;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::{ToolContext, TypedToolExecutor};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionSearchInput {
    /// Plain-text search expression. Escaped via `plainto_tsquery`.
    pub query: String,
    /// Max ranked turns to return. Range `[1, 50]`. Default `10`.
    #[serde(default = "default_top_k")]
    pub top_k: u32,
}

const fn default_top_k() -> u32 {
    10
}

#[derive(Debug, Serialize)]
struct SessionSearchHitOut {
    turn_id: String,
    session_id: String,
    turn_index: i32,
    role: String,
    rank: f32,
    snippet: String,
}

#[derive(Debug, Default)]
pub struct SessionSearchTool;

#[async_trait]
impl TypedToolExecutor for SessionSearchTool {
    type Input = SessionSearchInput;

    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Full-text search this tenant's session turn history via Postgres FTS. \
         Returns up to top_k ranked turns with snippet highlights. Read-only."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let Some(pool) = ctx.extensions.get::<PgPool>() else {
            return Ok(ToolResult::error(
                "session_search: PgPool extension missing from ToolContext".to_string(),
            ));
        };
        let tenant_id = match uuid::Uuid::parse_str(&ctx.tenant_id) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult::error(format!("session_search: invalid tenant_id: {e}")));
            }
        };
        let top_k = i64::from(input.top_k.clamp(1, 50));

        let mut tx = pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let hits = roz_db::session_turns::search_by_tsquery(&mut *tx, &input.query, top_k).await?;
        tx.commit().await?;

        let out: Vec<SessionSearchHitOut> = hits
            .into_iter()
            .map(|h| SessionSearchHitOut {
                turn_id: h.turn_id.to_string(),
                session_id: h.session_id.to_string(),
                turn_index: h.turn_index,
                role: h.role,
                rank: h.rank,
                snippet: h.snippet,
            })
            .collect();

        Ok(ToolResult::success(serde_json::to_value(&out)?))
    }
}
