//! MEM-07: `user_model_query` Pure tool — read dialectic user-model facts.
//!
//! Ungated read within tenant RLS (D-10). Stale facts filtered by SQL.

use async_trait::async_trait;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::{ToolContext, TypedToolExecutor};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UserModelQueryInput {
    /// Opaque peer id (username, federation id, or UUID) we're asking about.
    pub observed_peer_id: String,
    /// Maximum facts to return. Range `[1, 100]`. Default `20`.
    #[serde(default = "default_max")]
    pub max_facts: u32,
}

const fn default_max() -> u32 {
    20
}

#[derive(Debug, Serialize)]
struct FactOut {
    fact_id: String,
    observer_peer_id: String,
    fact: String,
    confidence: f32,
    stale_after: Option<String>,
    created_at: String,
}

#[derive(Debug, Default)]
pub struct UserModelQueryTool;

#[async_trait]
impl TypedToolExecutor for UserModelQueryTool {
    type Input = UserModelQueryInput;

    fn name(&self) -> &str {
        "user_model_query"
    }

    fn description(&self) -> &str {
        "Return up to `max_facts` non-stale dialectic user-model facts about \
         the specified `observed_peer_id`, scoped to this tenant."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let Some(pool) = ctx.extensions.get::<PgPool>() else {
            return Ok(ToolResult::error(
                "user_model_query: PgPool extension missing".to_string(),
            ));
        };
        let tenant_id = match Uuid::parse_str(&ctx.tenant_id) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult::error(format!("user_model_query: invalid tenant_id: {e}")));
            }
        };
        let max = i64::from(input.max_facts.clamp(1, 100));

        let mut tx = pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let rows =
            roz_db::user_model_facts::list_recent_facts(&mut *tx, tenant_id, &input.observed_peer_id, max).await?;
        tx.commit().await?;

        let out: Vec<FactOut> = rows
            .into_iter()
            .map(|r| FactOut {
                fact_id: r.fact_id.to_string(),
                observer_peer_id: r.observer_peer_id,
                fact: r.fact,
                confidence: r.confidence,
                stale_after: r.stale_after.map(|t| t.to_rfc3339()),
                created_at: r.created_at.to_rfc3339(),
            })
            .collect();

        Ok(ToolResult::success(serde_json::to_value(&out)?))
    }
}
