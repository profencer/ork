//! ork-llm — wire client + router for the OpenAI-compatible LLM provider
//! catalog (ADR 0012). The single in-tree wire client lives in
//! [`openai_compatible`]; the catalog selector + per-tenant override
//! merging lives in [`router`]. Both are constructed from
//! [`ork_common::config::LlmConfig`].

pub mod embedder;
pub mod openai_compatible;
pub mod router;
