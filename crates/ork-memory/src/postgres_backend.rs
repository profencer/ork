//! Postgres-backed [`MemoryStore`] (ADR 0053 production backend).
//!
//! Expects the `mem_messages` / `mem_working` / `mem_embeddings` tables
//! created by [`migrations/013_memory_tables.sql`](../../../migrations/013_memory_tables.sql)
//! and the `pgvector` extension. Every read and write happens inside a
//! tenant-scoped transaction so existing RLS policies (ADR-0020) keep
//! per-tenant isolation enforced at the row level.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_a2a::{MessageId, ResourceId, ThreadId};
use ork_common::error::OrkError;
use ork_common::types::TenantId;
use ork_core::ports::llm::{ChatMessage, MessageRole};
use ork_core::ports::memory_store::{
    Embedder, MemoryContext, MemoryStore, RecallHit, Scope, ThreadSummary,
};
use pgvector::Vector;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::OpenMemory;
use crate::shapes;
use crate::types::MemoryOptions;

/// Connection spec used by [`super::Memory::postgres`].
pub struct PostgresConnect {
    pub(crate) pool: PgPool,
}

#[async_trait]
impl OpenMemory for PostgresConnect {
    async fn open(
        self,
        options: MemoryOptions,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Arc<dyn MemoryStore>, OrkError> {
        Ok(Arc::new(PgMemory {
            pool: self.pool,
            options,
            embedder,
        }))
    }
}

pub(crate) struct PgMemory {
    pool: PgPool,
    options: MemoryOptions,
    embedder: Option<Arc<dyn Embedder>>,
}

/// Open a tenant-scoped tx and set `app.current_tenant_id`. Mirrors the
/// canonical [`ork_persistence::postgres::tenant_scope::open_tenant_tx`]
/// helper; duplicated here because adding a dep on `ork-persistence`
/// would invert the layering (`ork-memory` is consumed by it, not the
/// other way).
async fn open_tenant_tx(
    pool: &PgPool,
    tenant: TenantId,
) -> Result<Transaction<'_, Postgres>, OrkError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| OrkError::Database(format!("memory: open tenant tx: {e}")))?;
    sqlx::query("SELECT set_config('app.current_tenant_id', $1, true)")
        .bind(tenant.0.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("memory: set tenant GUC: {e}")))?;
    Ok(tx)
}

#[async_trait]
impl MemoryStore for PgMemory {
    fn name(&self) -> &str {
        "postgres"
    }

    async fn append_message(
        &self,
        ctx: &MemoryContext,
        msg: ChatMessage,
    ) -> Result<MessageId, OrkError> {
        let id = MessageId::new();
        let parts = serde_json::to_value(&msg.parts).map_err(internal)?;
        let tool_calls = serde_json::to_value(&msg.tool_calls).map_err(internal)?;
        let role = role_str(msg.role).to_string();

        let mut tx = open_tenant_tx(&self.pool, ctx.tenant_id).await?;
        sqlx::query(
            "INSERT INTO mem_messages \
             (tenant_id, resource_id, thread_id, agent_id, message_id, role, \
              content, parts, tool_calls, tool_call_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(ctx.tenant_id.0)
        .bind(ctx.resource_id.0)
        .bind(ctx.thread_id.0)
        .bind(&ctx.agent_id)
        .bind(id.0)
        .bind(&role)
        .bind(&msg.content)
        .bind(&parts)
        .bind(&tool_calls)
        .bind(msg.tool_call_id.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("append_message: {e}")))?;

        if self.options.semantic_recall.enabled
            && let Some(embedder) = self.embedder.as_ref()
            && !msg.content.trim().is_empty()
        {
            let v = embedder
                .embed(std::slice::from_ref(&msg.content))
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| OrkError::Internal("embedder returned no vector".into()))?;
            sqlx::query(
                "INSERT INTO mem_embeddings \
                 (tenant_id, resource_id, thread_id, message_id, embedding, content) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (message_id) DO UPDATE SET \
                    embedding = EXCLUDED.embedding, content = EXCLUDED.content",
            )
            .bind(ctx.tenant_id.0)
            .bind(ctx.resource_id.0)
            .bind(ctx.thread_id.0)
            .bind(id.0)
            .bind(Vector::from(v))
            .bind(&msg.content)
            .execute(&mut *tx)
            .await
            .map_err(|e| OrkError::Database(format!("append_message embedding: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| OrkError::Database(format!("append_message commit: {e}")))?;
        Ok(id)
    }

    async fn last_messages(
        &self,
        ctx: &MemoryContext,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, OrkError> {
        let mut tx = open_tenant_tx(&self.pool, ctx.tenant_id).await?;
        let rows = sqlx::query(
            "SELECT role, content, parts, tool_calls, tool_call_id \
             FROM mem_messages \
             WHERE tenant_id = $1 AND resource_id = $2 AND thread_id = $3 \
             ORDER BY created_at DESC LIMIT $4",
        )
        .bind(ctx.tenant_id.0)
        .bind(ctx.resource_id.0)
        .bind(ctx.thread_id.0)
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("last_messages: {e}")))?;
        tx.commit().await.ok();

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let role: String = row
                .try_get("role")
                .map_err(|e| OrkError::Database(format!("last_messages role: {e}")))?;
            let content: String = row
                .try_get("content")
                .map_err(|e| OrkError::Database(format!("last_messages content: {e}")))?;
            let parts: Value = row
                .try_get("parts")
                .map_err(|e| OrkError::Database(format!("last_messages parts: {e}")))?;
            let tool_calls: Value = row
                .try_get("tool_calls")
                .map_err(|e| OrkError::Database(format!("last_messages tool_calls: {e}")))?;
            let tool_call_id: Option<String> = row
                .try_get("tool_call_id")
                .map_err(|e| OrkError::Database(format!("last_messages tool_call_id: {e}")))?;
            out.push(ChatMessage {
                role: parse_role(&role)?,
                content,
                tool_calls: serde_json::from_value(tool_calls).map_err(internal)?,
                tool_call_id,
                parts: serde_json::from_value(parts).map_err(internal)?,
            });
        }
        out.reverse();
        Ok(out)
    }

    async fn working_memory(&self, ctx: &MemoryContext) -> Result<Option<Value>, OrkError> {
        let mut tx = open_tenant_tx(&self.pool, ctx.tenant_id).await?;
        let row = sqlx::query(
            "SELECT value FROM mem_working \
             WHERE tenant_id = $1 AND resource_id = $2 AND agent_id = $3 LIMIT 1",
        )
        .bind(ctx.tenant_id.0)
        .bind(ctx.resource_id.0)
        .bind(&ctx.agent_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("working_memory: {e}")))?;
        tx.commit().await.ok();
        Ok(row
            .map(|r| r.try_get::<Value, _>("value"))
            .transpose()
            .map_err(|e| OrkError::Database(format!("working_memory col: {e}")))?)
    }

    async fn set_working_memory(&self, ctx: &MemoryContext, v: Value) -> Result<(), OrkError> {
        if let Some(shape) = self.options.working_memory.as_ref() {
            shapes::validate(shape, &v)?;
        }
        let mut tx = open_tenant_tx(&self.pool, ctx.tenant_id).await?;
        sqlx::query(
            "INSERT INTO mem_working (tenant_id, resource_id, agent_id, value) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (tenant_id, resource_id, agent_id) \
             DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(ctx.tenant_id.0)
        .bind(ctx.resource_id.0)
        .bind(&ctx.agent_id)
        .bind(&v)
        .execute(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("set_working_memory: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| OrkError::Database(format!("set_working_memory commit: {e}")))?;
        Ok(())
    }

    async fn semantic_recall(
        &self,
        ctx: &MemoryContext,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<RecallHit>, OrkError> {
        let Some(embedder) = self.embedder.as_ref() else {
            return Ok(Vec::new());
        };
        let q = embedder
            .embed(&[query.to_string()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| OrkError::Internal("embedder returned no vector".into()))?;
        let qvec = Vector::from(q);

        let mut tx = open_tenant_tx(&self.pool, ctx.tenant_id).await?;
        let rows = match self.options.semantic_recall.scope {
            Scope::Thread => {
                sqlx::query(
                    "SELECT message_id, thread_id, content, \
                        1 - (embedding <=> $4) AS score \
                 FROM mem_embeddings \
                 WHERE tenant_id = $1 AND resource_id = $2 AND thread_id = $3 \
                 ORDER BY embedding <=> $4 LIMIT $5",
                )
                .bind(ctx.tenant_id.0)
                .bind(ctx.resource_id.0)
                .bind(ctx.thread_id.0)
                .bind(&qvec)
                .bind(top_k as i64)
                .fetch_all(&mut *tx)
                .await
            }
            Scope::Resource => {
                sqlx::query(
                    "SELECT message_id, thread_id, content, \
                        1 - (embedding <=> $3) AS score \
                 FROM mem_embeddings \
                 WHERE tenant_id = $1 AND resource_id = $2 \
                 ORDER BY embedding <=> $3 LIMIT $4",
                )
                .bind(ctx.tenant_id.0)
                .bind(ctx.resource_id.0)
                .bind(&qvec)
                .bind(top_k as i64)
                .fetch_all(&mut *tx)
                .await
            }
            Scope::Tenant => {
                sqlx::query(
                    "SELECT message_id, thread_id, content, \
                        1 - (embedding <=> $2) AS score \
                 FROM mem_embeddings \
                 WHERE tenant_id = $1 \
                 ORDER BY embedding <=> $2 LIMIT $3",
                )
                .bind(ctx.tenant_id.0)
                .bind(&qvec)
                .bind(top_k as i64)
                .fetch_all(&mut *tx)
                .await
            }
        }
        .map_err(|e| OrkError::Database(format!("semantic_recall: {e}")))?;
        tx.commit().await.ok();

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mid: Uuid = row
                .try_get("message_id")
                .map_err(|e| OrkError::Database(format!("recall message_id: {e}")))?;
            let tid: Uuid = row
                .try_get("thread_id")
                .map_err(|e| OrkError::Database(format!("recall thread_id: {e}")))?;
            let content: String = row
                .try_get("content")
                .map_err(|e| OrkError::Database(format!("recall content: {e}")))?;
            let score: f64 = row
                .try_get("score")
                .map_err(|e| OrkError::Database(format!("recall score: {e}")))?;
            out.push(RecallHit {
                message_id: MessageId(mid),
                thread_id: ThreadId(tid),
                content,
                score: score as f32,
            });
        }
        Ok(out)
    }

    async fn list_threads(
        &self,
        tenant_id: TenantId,
        resource_id: &ResourceId,
    ) -> Result<Vec<ThreadSummary>, OrkError> {
        let mut tx = open_tenant_tx(&self.pool, tenant_id).await?;
        let rows = sqlx::query(
            "SELECT thread_id, MAX(created_at) AS last_at, COUNT(*) AS n \
             FROM mem_messages \
             WHERE tenant_id = $1 AND resource_id = $2 \
             GROUP BY thread_id ORDER BY last_at DESC",
        )
        .bind(tenant_id.0)
        .bind(resource_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("list_threads: {e}")))?;
        tx.commit().await.ok();

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let tid: Uuid = row
                .try_get("thread_id")
                .map_err(|e| OrkError::Database(format!("list_threads tid: {e}")))?;
            let last_at: DateTime<Utc> = row
                .try_get("last_at")
                .map_err(|e| OrkError::Database(format!("list_threads last_at: {e}")))?;
            let n: i64 = row
                .try_get("n")
                .map_err(|e| OrkError::Database(format!("list_threads n: {e}")))?;
            out.push(ThreadSummary {
                thread_id: ThreadId(tid),
                last_message_at: last_at,
                message_count: n.max(0) as u64,
            });
        }
        Ok(out)
    }

    async fn delete_thread(&self, ctx: &MemoryContext) -> Result<(), OrkError> {
        let mut tx = open_tenant_tx(&self.pool, ctx.tenant_id).await?;
        sqlx::query(
            "DELETE FROM mem_messages \
             WHERE tenant_id = $1 AND resource_id = $2 AND thread_id = $3",
        )
        .bind(ctx.tenant_id.0)
        .bind(ctx.resource_id.0)
        .bind(ctx.thread_id.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("delete_thread messages: {e}")))?;
        sqlx::query(
            "DELETE FROM mem_embeddings \
             WHERE tenant_id = $1 AND resource_id = $2 AND thread_id = $3",
        )
        .bind(ctx.tenant_id.0)
        .bind(ctx.resource_id.0)
        .bind(ctx.thread_id.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| OrkError::Database(format!("delete_thread embeddings: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| OrkError::Database(format!("delete_thread commit: {e}")))?;
        Ok(())
    }
}

fn role_str(r: MessageRole) -> &'static str {
    match r {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn parse_role(s: &str) -> Result<MessageRole, OrkError> {
    Ok(match s {
        "system" => MessageRole::System,
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "tool" => MessageRole::Tool,
        other => return Err(OrkError::Database(format!("unknown role `{other}`"))),
    })
}

fn internal<E: std::fmt::Display>(e: E) -> OrkError {
    OrkError::Internal(format!("ork-memory(pg): {e}"))
}
