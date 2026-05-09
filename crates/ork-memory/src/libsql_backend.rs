//! libsql-backed [`MemoryStore`] (ADR 0053 default backend).
//!
//! libsql 0.6 has a native `F32_BLOB(N)` vector type and `vector_distance_cos`
//! function, but the API surface across libsql client builds is uneven, so
//! this backend stores the raw little-endian `f32` bytes in a `BLOB` column
//! and computes cosine similarity in Rust at query time. For dev-scale
//! corpora (Mastra-equivalent), the brute-force scan stays under the noise
//! floor; production deployments use the [Postgres backend](super::postgres_backend)
//! with `pgvector`'s ivfflat index.

use std::str::FromStr;
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
use serde_json::Value;
use uuid::Uuid;

use crate::OpenMemory;
use crate::shapes;
use crate::types::MemoryOptions;

/// Connection spec used by [`super::Memory::libsql`].
pub struct LibsqlConnect {
    pub(crate) url: String,
}

#[async_trait]
impl OpenMemory for LibsqlConnect {
    async fn open(
        self,
        options: MemoryOptions,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Result<Arc<dyn MemoryStore>, OrkError> {
        let db = libsql::Builder::new_local(&self.url)
            .build()
            .await
            .map_err(|e| OrkError::Database(format!("libsql open `{}`: {e}", self.url)))?;
        let conn = db
            .connect()
            .map_err(|e| OrkError::Database(format!("libsql connect: {e}")))?;
        run_ddl(&conn).await?;
        Ok(Arc::new(LibsqlMemory {
            db: Arc::new(db),
            options,
            embedder,
        }))
    }
}

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS mem_messages (
    tenant_id   TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    thread_id   TEXT NOT NULL,
    agent_id    TEXT NOT NULL,
    message_id  TEXT NOT NULL PRIMARY KEY,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,
    parts       TEXT NOT NULL,
    tool_calls  TEXT NOT NULL,
    tool_call_id TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS mem_messages_thread
    ON mem_messages (tenant_id, resource_id, thread_id, created_at);

CREATE TABLE IF NOT EXISTS mem_working (
    tenant_id   TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    agent_id    TEXT NOT NULL,
    value       TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (tenant_id, resource_id, agent_id)
);

CREATE TABLE IF NOT EXISTS mem_embeddings (
    tenant_id   TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    thread_id   TEXT NOT NULL,
    message_id  TEXT NOT NULL PRIMARY KEY,
    embedding   BLOB NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
"#;

async fn run_ddl(conn: &libsql::Connection) -> Result<(), OrkError> {
    for stmt in DDL.split(';') {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        conn.execute(s, ())
            .await
            .map_err(|e| OrkError::Database(format!("libsql DDL: {e}")))?;
    }
    Ok(())
}

pub(crate) struct LibsqlMemory {
    db: Arc<libsql::Database>,
    options: MemoryOptions,
    embedder: Option<Arc<dyn Embedder>>,
}

impl LibsqlMemory {
    async fn conn(&self) -> Result<libsql::Connection, OrkError> {
        self.db
            .connect()
            .map_err(|e| OrkError::Database(format!("libsql connect: {e}")))
    }
}

#[async_trait]
impl MemoryStore for LibsqlMemory {
    fn name(&self) -> &str {
        "libsql"
    }

    async fn append_message(
        &self,
        ctx: &MemoryContext,
        msg: ChatMessage,
    ) -> Result<MessageId, OrkError> {
        let conn = self.conn().await?;
        let id = MessageId::new();
        let created = Utc::now().to_rfc3339();
        let parts_json = serde_json::to_string(&msg.parts).map_err(internal)?;
        let tool_calls_json = serde_json::to_string(&msg.tool_calls).map_err(internal)?;
        let role = role_str(msg.role);
        conn.execute(
            "INSERT INTO mem_messages \
             (tenant_id, resource_id, thread_id, agent_id, message_id, role, \
              content, parts, tool_calls, tool_call_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                ctx.tenant_id.0.to_string(),
                ctx.resource_id.0.to_string(),
                ctx.thread_id.0.to_string(),
                ctx.agent_id.clone(),
                id.0.to_string(),
                role.to_string(),
                msg.content.clone(),
                parts_json,
                tool_calls_json,
                msg.tool_call_id.clone(),
                created.clone(),
            ],
        )
        .await
        .map_err(|e| OrkError::Database(format!("append_message insert: {e}")))?;

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
            conn.execute(
                "INSERT OR REPLACE INTO mem_embeddings \
                 (tenant_id, resource_id, thread_id, message_id, embedding, content, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    ctx.tenant_id.0.to_string(),
                    ctx.resource_id.0.to_string(),
                    ctx.thread_id.0.to_string(),
                    id.0.to_string(),
                    f32_vec_to_bytes(&v),
                    msg.content.clone(),
                    created,
                ],
            )
            .await
            .map_err(|e| OrkError::Database(format!("append_message embedding: {e}")))?;
        }
        Ok(id)
    }

    async fn last_messages(
        &self,
        ctx: &MemoryContext,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, OrkError> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT role, content, parts, tool_calls, tool_call_id \
                 FROM mem_messages \
                 WHERE tenant_id = ? AND resource_id = ? AND thread_id = ? \
                 ORDER BY created_at DESC LIMIT ?",
                libsql::params![
                    ctx.tenant_id.0.to_string(),
                    ctx.resource_id.0.to_string(),
                    ctx.thread_id.0.to_string(),
                    limit as i64,
                ],
            )
            .await
            .map_err(|e| OrkError::Database(format!("last_messages: {e}")))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| OrkError::Database(format!("last_messages row: {e}")))?
        {
            out.push(row_to_chat_message(&row)?);
        }
        out.reverse();
        Ok(out)
    }

    async fn working_memory(&self, ctx: &MemoryContext) -> Result<Option<Value>, OrkError> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT value FROM mem_working \
                 WHERE tenant_id = ? AND resource_id = ? AND agent_id = ? LIMIT 1",
                libsql::params![
                    ctx.tenant_id.0.to_string(),
                    ctx.resource_id.0.to_string(),
                    ctx.agent_id.clone(),
                ],
            )
            .await
            .map_err(|e| OrkError::Database(format!("working_memory query: {e}")))?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|e| OrkError::Database(format!("working_memory row: {e}")))?
        else {
            return Ok(None);
        };
        let raw: String = row
            .get(0)
            .map_err(|e| OrkError::Database(format!("working_memory col: {e}")))?;
        let v: Value = serde_json::from_str(&raw).map_err(internal)?;
        Ok(Some(v))
    }

    async fn set_working_memory(&self, ctx: &MemoryContext, v: Value) -> Result<(), OrkError> {
        if let Some(shape) = self.options.working_memory.as_ref() {
            shapes::validate(shape, &v)?;
        }
        let conn = self.conn().await?;
        let raw = serde_json::to_string(&v).map_err(internal)?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO mem_working (tenant_id, resource_id, agent_id, value, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(tenant_id, resource_id, agent_id) DO UPDATE SET \
                value = excluded.value, updated_at = excluded.updated_at",
            libsql::params![
                ctx.tenant_id.0.to_string(),
                ctx.resource_id.0.to_string(),
                ctx.agent_id.clone(),
                raw,
                now,
            ],
        )
        .await
        .map_err(|e| OrkError::Database(format!("set_working_memory upsert: {e}")))?;
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

        let scope = self.options.semantic_recall.scope;
        let conn = self.conn().await?;
        let (sql, params): (&str, Vec<libsql::Value>) = match scope {
            Scope::Thread => (
                "SELECT message_id, thread_id, embedding, content FROM mem_embeddings \
                 WHERE tenant_id = ? AND resource_id = ? AND thread_id = ?",
                vec![
                    ctx.tenant_id.0.to_string().into(),
                    ctx.resource_id.0.to_string().into(),
                    ctx.thread_id.0.to_string().into(),
                ],
            ),
            Scope::Resource => (
                "SELECT message_id, thread_id, embedding, content FROM mem_embeddings \
                 WHERE tenant_id = ? AND resource_id = ?",
                vec![
                    ctx.tenant_id.0.to_string().into(),
                    ctx.resource_id.0.to_string().into(),
                ],
            ),
            Scope::Tenant => (
                "SELECT message_id, thread_id, embedding, content FROM mem_embeddings \
                 WHERE tenant_id = ?",
                vec![ctx.tenant_id.0.to_string().into()],
            ),
        };
        let mut rows = conn
            .query(sql, params)
            .await
            .map_err(|e| OrkError::Database(format!("semantic_recall query: {e}")))?;

        let mut hits: Vec<RecallHit> = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| OrkError::Database(format!("semantic_recall row: {e}")))?
        {
            let mid: String = row
                .get(0)
                .map_err(|e| OrkError::Database(format!("recall col 0: {e}")))?;
            let tid: String = row
                .get(1)
                .map_err(|e| OrkError::Database(format!("recall col 1: {e}")))?;
            let embedding: Vec<u8> = row
                .get(2)
                .map_err(|e| OrkError::Database(format!("recall col 2: {e}")))?;
            let content: String = row
                .get(3)
                .map_err(|e| OrkError::Database(format!("recall col 3: {e}")))?;
            let v = bytes_to_f32_vec(&embedding)?;
            let score = cosine(&q, &v);
            hits.push(RecallHit {
                message_id: MessageId(parse_uuid(&mid)?),
                thread_id: ThreadId(parse_uuid(&tid)?),
                content,
                score,
            });
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(top_k);
        Ok(hits)
    }

    async fn list_threads(
        &self,
        tenant_id: TenantId,
        resource_id: &ResourceId,
    ) -> Result<Vec<ThreadSummary>, OrkError> {
        let conn = self.conn().await?;
        let mut rows = conn
            .query(
                "SELECT thread_id, MAX(created_at) AS last_at, COUNT(*) AS msg_count \
                 FROM mem_messages \
                 WHERE tenant_id = ? AND resource_id = ? \
                 GROUP BY thread_id ORDER BY last_at DESC",
                libsql::params![tenant_id.0.to_string(), resource_id.0.to_string()],
            )
            .await
            .map_err(|e| OrkError::Database(format!("list_threads query: {e}")))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| OrkError::Database(format!("list_threads row: {e}")))?
        {
            let tid: String = row
                .get(0)
                .map_err(|e| OrkError::Database(format!("list_threads col 0: {e}")))?;
            let last_at_s: String = row
                .get(1)
                .map_err(|e| OrkError::Database(format!("list_threads col 1: {e}")))?;
            let count: i64 = row
                .get(2)
                .map_err(|e| OrkError::Database(format!("list_threads col 2: {e}")))?;
            let last_message_at = DateTime::parse_from_rfc3339(&last_at_s)
                .map_err(|e| OrkError::Database(format!("list_threads ts: {e}")))?
                .with_timezone(&Utc);
            out.push(ThreadSummary {
                thread_id: ThreadId(parse_uuid(&tid)?),
                last_message_at,
                message_count: count.max(0) as u64,
            });
        }
        Ok(out)
    }

    async fn delete_thread(&self, ctx: &MemoryContext) -> Result<(), OrkError> {
        let conn = self.conn().await?;
        conn.execute(
            "DELETE FROM mem_messages \
             WHERE tenant_id = ? AND resource_id = ? AND thread_id = ?",
            libsql::params![
                ctx.tenant_id.0.to_string(),
                ctx.resource_id.0.to_string(),
                ctx.thread_id.0.to_string(),
            ],
        )
        .await
        .map_err(|e| OrkError::Database(format!("delete_thread messages: {e}")))?;
        conn.execute(
            "DELETE FROM mem_embeddings \
             WHERE tenant_id = ? AND resource_id = ? AND thread_id = ?",
            libsql::params![
                ctx.tenant_id.0.to_string(),
                ctx.resource_id.0.to_string(),
                ctx.thread_id.0.to_string(),
            ],
        )
        .await
        .map_err(|e| OrkError::Database(format!("delete_thread embeddings: {e}")))?;
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
        other => {
            return Err(OrkError::Database(format!("unknown role `{other}`")));
        }
    })
}

fn row_to_chat_message(row: &libsql::Row) -> Result<ChatMessage, OrkError> {
    let role: String = row
        .get(0)
        .map_err(|e| OrkError::Database(format!("row role: {e}")))?;
    let content: String = row
        .get(1)
        .map_err(|e| OrkError::Database(format!("row content: {e}")))?;
    let parts: String = row
        .get(2)
        .map_err(|e| OrkError::Database(format!("row parts: {e}")))?;
    let tool_calls: String = row
        .get(3)
        .map_err(|e| OrkError::Database(format!("row tool_calls: {e}")))?;
    let tool_call_id: Option<String> = row
        .get(4)
        .map_err(|e| OrkError::Database(format!("row tool_call_id: {e}")))?;
    Ok(ChatMessage {
        role: parse_role(&role)?,
        content,
        tool_calls: serde_json::from_str(&tool_calls).map_err(internal)?,
        tool_call_id,
        parts: serde_json::from_str(&parts).map_err(internal)?,
    })
}

fn parse_uuid(s: &str) -> Result<Uuid, OrkError> {
    Uuid::from_str(s).map_err(|e| OrkError::Database(format!("uuid parse `{s}`: {e}")))
}

/// Encode a `Vec<f32>` as raw little-endian bytes for storage in the
/// libsql `BLOB` column. Matches libsql's `F32_BLOB(N)` byte order so a
/// future migration to the native vector type does not need a rewrite
/// of stored rows. Read by [`bytes_to_f32_vec`].
fn f32_vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn bytes_to_f32_vec(b: &[u8]) -> Result<Vec<f32>, OrkError> {
    if b.len() % 4 != 0 {
        return Err(OrkError::Database(format!(
            "embedding bytes len {} is not a multiple of 4",
            b.len()
        )));
    }
    let mut out = Vec::with_capacity(b.len() / 4);
    for chunk in b.chunks_exact(4) {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    Ok(out)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(1e-6);
    dot / denom
}

fn internal<E: std::fmt::Display>(e: E) -> OrkError {
    OrkError::Internal(format!("ork-memory: {e}"))
}
