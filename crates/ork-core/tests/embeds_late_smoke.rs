//! Late-phase stream wrapper (ADR-0015).

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use ork_a2a::{
    ContextId, Message, MessageId, Part, Role, TaskEvent as AgentEvent, TaskId, TaskState,
    TaskStatus, TaskStatusUpdateEvent,
};
use ork_common::types::TenantId;
use ork_core::embeds::{EmbedContext, EmbedLimits, EmbedRegistry};
use ork_core::ports::agent::AgentEventStream;
use ork_core::streaming::late_embed::LateEmbedResolver;
use uuid::Uuid;

fn msg(text: &str) -> AgentEvent {
    AgentEvent::Message(Message {
        role: Role::Agent,
        parts: vec![Part::text(text)],
        message_id: MessageId::new(),
        task_id: None,
        context_id: Some(ContextId::new()),
        metadata: None,
    })
}

#[tokio::test]
async fn splits_embed_across_two_message_events() {
    let registry = Arc::new(EmbedRegistry::with_builtins());
    let tid = TaskId::new();
    let embed_ctx = Arc::new(EmbedContext {
        tenant_id: TenantId(Uuid::new_v4()),
        task_id: Some(tid),
        a2a_repo: None,
        now: chrono::Utc::now(),
        variables: HashMap::new(),
        depth: 0,
    });
    let inner: AgentEventStream = Box::pin(futures::stream::iter(vec![
        Ok(msg("before «")),
        Ok(AgentEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: tid,
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
            },
            is_final: false,
        })),
        Ok(msg("uuid» after")),
    ]));
    let mut out = LateEmbedResolver::new(registry, embed_ctx, EmbedLimits::default()).wrap(inner);
    let mut texts = String::new();
    let mut saw_status = false;
    while let Some(ev) = out.next().await {
        let ev = ev.expect("ev");
        match ev {
            AgentEvent::Message(m) => {
                for p in m.parts {
                    if let Part::Text { text, .. } = p {
                        texts.push_str(&text);
                    }
                }
            }
            AgentEvent::StatusUpdate(_) => {
                saw_status = true;
            }
            AgentEvent::ArtifactUpdate(_) => {}
        }
    }
    assert!(saw_status, "status event forwarded");
    assert!(
        !texts.contains("«") && !texts.contains("»") && !texts.contains("uuid"),
        "late uuid should expand, got {texts:?}"
    );
    assert!(texts.contains("after"), "tail preserved: {texts:?}");
    assert!(texts.contains("before"), "head preserved: {texts:?}");
    assert!(texts.matches('-').count() >= 4, "uuid v4, got {texts}");
}
