pub mod a2a_client;
pub mod agent_call;
pub mod artifact_tools;
/// ADR-0016: `Part::File` base64 → [`ArtifactStore`] + proxy URI.
pub mod artifact_wire;
pub mod code_tools;
pub mod github;
pub mod gitlab;
pub mod native_tool_defs;
pub mod tool_plane;
pub mod tools;
pub mod workspace;

pub use a2a_client::{
    A2aAuth, A2aClientConfig, A2aRemoteAgent, A2aRemoteAgentBuilder, CardFetcher, CcTokenCache,
    DEFAULT_API_KEY_HEADER, RetryPolicy, TokenProvider, parse_a2a_sse,
};
