//! Deterministic test embedder.
//!
//! Real embedders live in `ork-llm` (`OpenAiEmbedder`). Tests and dev
//! environments use [`DeterministicMockEmbedder`] so semantic-recall
//! coverage runs without network calls.

use async_trait::async_trait;
use ork_common::error::OrkError;
use ork_core::ports::memory_store::Embedder;

/// Deterministic embedder: hashes each input string to a fixed-dimension
/// vector. Same input → same vector; orthogonal-ish for distinct inputs.
/// Suitable for assert-style coverage of the recall plumbing; not for
/// quality.
#[derive(Clone, Debug)]
pub struct DeterministicMockEmbedder {
    dim: usize,
}

impl DeterministicMockEmbedder {
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for DeterministicMockEmbedder {
    fn default() -> Self {
        Self::new(8)
    }
}

#[async_trait]
impl Embedder for DeterministicMockEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, OrkError> {
        Ok(texts.iter().map(|t| hash_to_vec(t, self.dim)).collect())
    }
}

fn hash_to_vec(text: &str, dim: usize) -> Vec<f32> {
    let mut out = vec![0.0_f32; dim];
    for (idx, byte) in text.bytes().enumerate() {
        let slot = idx % dim;
        // Mix the byte into the slot using a simple FNV-style step so
        // texts that differ in any byte produce distinct vectors.
        out[slot] = ((out[slot].to_bits() ^ u32::from(byte).wrapping_mul(16_777_619)) & 0x00FF_FFFF)
            as f32
            / 16_777_215.0;
    }
    let norm = (out.iter().map(|v| v * v).sum::<f32>()).sqrt().max(1e-6);
    for v in &mut out {
        *v /= norm;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deterministic_for_same_input() {
        let e = DeterministicMockEmbedder::new(16);
        let a = e.embed(&["hello".to_string()]).await.unwrap();
        let b = e.embed(&["hello".to_string()]).await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn distinct_inputs_yield_distinct_vectors() {
        let e = DeterministicMockEmbedder::new(16);
        let v = e
            .embed(&["alpha".to_string(), "omega".to_string()])
            .await
            .unwrap();
        assert_ne!(v[0], v[1]);
    }

    #[tokio::test]
    async fn vector_has_expected_dimension() {
        let e = DeterministicMockEmbedder::new(32);
        let v = e.embed(&["x".to_string()]).await.unwrap();
        assert_eq!(v[0].len(), 32);
    }
}
