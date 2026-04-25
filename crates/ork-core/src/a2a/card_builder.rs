//! Build enriched [`AgentCard`] instances for local agents (ADR-0005 §`Card content`).
//!
//! Plain `LocalAgent::card_from_config` only knows what the agent *is* (name, skills,
//! system prompt). It can't know what the *deployment* exposes — the public Kong base URL,
//! the operator/provider organization, the DevPortal home, the Kafka namespace. Those live
//! on the boot context built once in `main.rs` and threaded through to every local agent
//! constructor.

use ork_a2a::{AgentCapabilities, AgentCard, AgentExtension, AgentProvider, AgentSkill, topics};
use url::Url;

use crate::models::agent::AgentConfig;

use super::extensions::{tenant_required_extension, transport_hint_extension};

/// Boot-time context required to build a publishable, callable [`AgentCard`].
#[derive(Clone, Debug)]
pub struct CardEnrichmentContext {
    /// Kong-fronted public base URL, e.g. `https://api.example.com/`. The per-agent URL
    /// is `{public_base_url}/a2a/agents/{id}`. `None` ⇒ the card is published without a
    /// `url` (callable only via the Kafka request topic).
    pub public_base_url: Option<Url>,
    /// Operator organization name placed in `provider.organization`. `None` ⇒ no `provider`
    /// (the spec field is optional and the partial form is invalid).
    pub provider_organization: Option<String>,
    /// DevPortal home, placed in `provider.url`.
    pub devportal_url: Option<Url>,
    /// Kafka topic namespace (matches `[kafka].namespace`); used to derive the request
    /// topic for the `transport-hint` extension.
    pub namespace: String,
    /// If `true`, every card includes the `tenant-required` extension (ADR-0020 stub).
    pub include_tenant_required_ext: bool,
    /// HTTP header carrying the tenant id (e.g. `X-Tenant-Id`). Used only when
    /// `include_tenant_required_ext` is set.
    pub tenant_header: String,
}

impl CardEnrichmentContext {
    /// Defaults suitable for unit tests and the bootless dev path: no public URL, no
    /// provider, namespace = `ork.a2a.v1`, tenant extension off.
    #[must_use]
    pub fn minimal() -> Self {
        Self {
            public_base_url: None,
            provider_organization: None,
            devportal_url: None,
            namespace: topics::DEFAULT_NAMESPACE.to_string(),
            include_tenant_required_ext: false,
            tenant_header: "X-Tenant-Id".to_string(),
        }
    }
}

impl Default for CardEnrichmentContext {
    fn default() -> Self {
        Self::minimal()
    }
}

fn per_agent_url(base: &Url, agent_id: &str) -> Url {
    // Trim a trailing slash on the base then append `/a2a/agents/{id}` so we don't
    // double-slash. `Url::join` does the right thing only if the base ends with `/`, so we
    // normalise here.
    let mut s = base.to_string();
    if s.ends_with('/') {
        s.pop();
    }
    s.push_str("/a2a/agents/");
    s.push_str(agent_id);
    Url::parse(&s).unwrap_or_else(|_| base.clone())
}

/// Build a publishable, callable card for a local agent. The result is what gets
/// `Arc`'d into the registry and published to the discovery topic.
#[must_use]
pub fn build_local_card(config: &AgentConfig, ctx: &CardEnrichmentContext) -> AgentCard {
    let url = ctx
        .public_base_url
        .as_ref()
        .map(|b| per_agent_url(b, &config.id));

    let provider = match (
        ctx.provider_organization.as_ref(),
        ctx.devportal_url.as_ref(),
    ) {
        (Some(org), Some(home)) => Some(AgentProvider {
            organization: org.clone(),
            url: home.clone(),
        }),
        _ => None,
    };

    let mut extensions: Vec<AgentExtension> = vec![transport_hint_extension(
        topics::agent_request(&ctx.namespace, &config.id),
    )];
    if ctx.include_tenant_required_ext {
        extensions.push(tenant_required_extension(ctx.tenant_header.clone()));
    }

    AgentCard {
        name: config.name.clone(),
        description: config.description.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        url,
        provider,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        },
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills: vec![AgentSkill {
            id: format!("{}-default", config.id),
            name: config.name.clone(),
            description: config.description.clone(),
            tags: vec![],
            examples: vec![],
            input_modes: None,
            output_modes: None,
        }],
        // Kong handles auth (ADR-0021); cards stay scheme-less in Phase 1.
        security_schemes: None,
        security: None,
        extensions: Some(extensions),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ork_a2a::extensions::{
        EXT_TENANT_REQUIRED, EXT_TRANSPORT_HINT, PARAM_KAFKA_REQUEST_TOPIC, PARAM_TENANT_HEADER,
    };

    fn cfg(id: &str) -> AgentConfig {
        AgentConfig {
            id: id.into(),
            name: format!("{id} agent"),
            description: "test".into(),
            system_prompt: "sys".into(),
            tools: vec![],
            model: None,
            temperature: 0.0,
            max_tokens: 100,
            max_tool_iterations: crate::models::agent::default_max_tool_iterations(),
            max_parallel_tool_calls: crate::models::agent::default_max_parallel_tool_calls(),
            max_tool_result_bytes: crate::models::agent::default_max_tool_result_bytes(),
            expose_reasoning: false,
        }
    }

    #[test]
    fn minimal_context_omits_url_and_provider_but_keeps_transport_hint() {
        let card = build_local_card(&cfg("planner"), &CardEnrichmentContext::minimal());
        assert!(card.url.is_none());
        assert!(card.provider.is_none());
        let ext = card
            .extensions
            .as_ref()
            .and_then(|v| v.iter().find(|e| e.uri == EXT_TRANSPORT_HINT))
            .expect("transport-hint always present");
        let topic = ext
            .params
            .as_ref()
            .and_then(|p| p.get(PARAM_KAFKA_REQUEST_TOPIC))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(topic, "ork.a2a.v1.agent.request.planner");
    }

    #[test]
    fn full_context_emits_url_provider_and_optional_tenant_ext() {
        let ctx = CardEnrichmentContext {
            public_base_url: Some(Url::parse("https://api.example.com/").unwrap()),
            provider_organization: Some("Example Corp".into()),
            devportal_url: Some(Url::parse("https://devportal.example.com/").unwrap()),
            namespace: "ork.a2a.v1".into(),
            include_tenant_required_ext: true,
            tenant_header: "X-Tenant-Id".into(),
        };
        let card = build_local_card(&cfg("planner"), &ctx);

        assert_eq!(
            card.url.as_ref().map(Url::as_str),
            Some("https://api.example.com/a2a/agents/planner")
        );
        let provider = card.provider.as_ref().expect("provider populated");
        assert_eq!(provider.organization, "Example Corp");
        assert_eq!(provider.url.as_str(), "https://devportal.example.com/");

        let exts = card.extensions.as_ref().unwrap();
        assert!(exts.iter().any(|e| e.uri == EXT_TRANSPORT_HINT));
        let tenant = exts.iter().find(|e| e.uri == EXT_TENANT_REQUIRED).unwrap();
        let header = tenant
            .params
            .as_ref()
            .and_then(|p| p.get(PARAM_TENANT_HEADER))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(header, "X-Tenant-Id");
    }

    #[test]
    fn provider_omitted_when_only_one_field_set() {
        let only_org = CardEnrichmentContext {
            provider_organization: Some("Example".into()),
            ..CardEnrichmentContext::minimal()
        };
        let card = build_local_card(&cfg("planner"), &only_org);
        assert!(
            card.provider.is_none(),
            "AgentProvider requires both organization and url"
        );
    }

    #[test]
    fn per_agent_url_handles_base_with_or_without_trailing_slash() {
        let with = Url::parse("https://api.example.com/").unwrap();
        let without = Url::parse("https://api.example.com").unwrap();
        assert_eq!(
            per_agent_url(&with, "planner").as_str(),
            "https://api.example.com/a2a/agents/planner"
        );
        assert_eq!(
            per_agent_url(&without, "planner").as_str(),
            "https://api.example.com/a2a/agents/planner"
        );
    }
}
