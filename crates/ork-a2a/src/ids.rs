use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for TaskId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextId(pub Uuid);

impl ContextId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ContextId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ContextId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ContextId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub Uuid);

impl MessageId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for MessageId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// ADR-0053: stable id for the human / org / device that owns a thread.
/// Working memory and semantic recall default to per-resource scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceId(pub Uuid);

impl ResourceId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Deterministic anonymous id derived from a tenant. Used when the
    /// caller has no `user_id` so anonymous traffic is isolated per tenant
    /// rather than colliding into one global resource. Real resource ids
    /// are minted via [`ResourceId::new`] (UUID v7); the anonymous form
    /// reuses the tenant's UUID directly so it sorts and hashes
    /// distinctly per tenant without pulling the `v5` uuid feature into
    /// the workspace.
    #[must_use]
    pub fn anonymous(tenant: Uuid) -> Self {
        Self(tenant)
    }
}

impl Default for ResourceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ResourceId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// ADR-0053: id of a single conversation thread. Threads are owned by a
/// `ResourceId` and isolate message history; semantic-recall scope decides
/// whether recall crosses thread boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(pub Uuid);

impl ThreadId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ThreadId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl From<ContextId> for ThreadId {
    fn from(c: ContextId) -> Self {
        Self(c.0)
    }
}

impl From<TaskId> for ThreadId {
    fn from(t: TaskId) -> Self {
        Self(t.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_is_v7() {
        let id = TaskId::new();
        assert_eq!(id.0.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn task_id_display_parse_roundtrip() {
        let id = TaskId::new();
        let s = id.to_string();
        let back: TaskId = s.parse().unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn task_id_serde_is_string_not_object() {
        let id = TaskId::new();
        let v = serde_json::to_value(id).unwrap();
        assert!(v.is_string(), "expected string, got {v}");
        let back: TaskId = serde_json::from_value(v).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn resource_id_anonymous_is_stable_per_tenant() {
        let tenant = Uuid::now_v7();
        let a = ResourceId::anonymous(tenant);
        let b = ResourceId::anonymous(tenant);
        assert_eq!(a, b, "anonymous derivation must be deterministic");

        let other = Uuid::now_v7();
        assert_ne!(
            a,
            ResourceId::anonymous(other),
            "different tenants must yield different anonymous resource ids"
        );
    }

    #[test]
    fn thread_id_from_context_id_preserves_uuid() {
        let ctx = ContextId::new();
        let thread: ThreadId = ctx.into();
        assert_eq!(thread.0, ctx.0);
    }
}
