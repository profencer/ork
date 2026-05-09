//! [`Embedder`](ork_core::ports::memory_store::Embedder) implementation
//! against the OpenAI-compatible `POST /embeddings` endpoint (ADR 0053
//! §`Embedder selection`).
//!
//! v1 ships this as a constructor-pluggable component:
//!
//! ```ignore
//! use std::sync::Arc;
//! use ork_llm::embedder::OpenAiEmbedder;
//!
//! let embedder = OpenAiEmbedder::new(
//!     "https://api.openai.com/v1",
//!     std::env::var("OPENAI_API_KEY")?,
//!     "text-embedding-3-small",
//!     1536,
//! );
//! let memory = Memory::libsql("file:./ork.db").embedder(Arc::new(embedder)).open().await?;
//! ```
//!
//! Routing through [`crate::router::LlmRouter`] is deferred to a
//! follow-up ADR — the v1 `MemoryStore` accepts an `Arc<dyn Embedder>`
//! directly, so providers wire one explicitly.

use std::time::Duration;

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::memory_store::Embedder;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// OpenAI `/embeddings` client. The vector dimension is recorded at
/// construction so callers can validate it against the schema column
/// width without round-tripping the network.
#[derive(Debug, Clone)]
pub struct OpenAiEmbedder {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    dimension: usize,
}

impl OpenAiEmbedder {
    /// Construct an embedder. `base_url` should NOT include a trailing
    /// `/embeddings`; the path is appended at request time.
    /// `dimension` MUST match what the model emits (1536 for
    /// `text-embedding-3-small`, 3072 for `text-embedding-3-large`).
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimension: usize,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            dimension,
        }
    }
}

#[derive(Serialize)]
struct EmbeddingsRequest<'a> {
    input: &'a [String],
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn dimension(&self) -> usize {
        self.dimension
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, OrkError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/embeddings", self.base_url);
        let req = EmbeddingsRequest {
            input: texts,
            model: &self.model,
        };
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| OrkError::LlmProvider(format!("embed: POST {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OrkError::LlmProvider(format!(
                "embed: HTTP {status} from {url}: {body}"
            )));
        }
        let parsed: EmbeddingsResponse = resp
            .json()
            .await
            .map_err(|e| OrkError::LlmProvider(format!("embed: parse response: {e}")))?;
        if parsed.data.len() != texts.len() {
            return Err(OrkError::LlmProvider(format!(
                "embed: expected {} embeddings, got {}",
                texts.len(),
                parsed.data.len()
            )));
        }
        let mut out = Vec::with_capacity(parsed.data.len());
        for d in parsed.data {
            if d.embedding.len() != self.dimension {
                return Err(OrkError::LlmProvider(format!(
                    "embed: dimension mismatch — declared {}, got {}",
                    self.dimension,
                    d.embedding.len()
                )));
            }
            out.push(d.embedding);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_is_recorded() {
        let e = OpenAiEmbedder::new("https://example", "k", "m", 1536);
        assert_eq!(e.dimension(), 1536);
    }
}
