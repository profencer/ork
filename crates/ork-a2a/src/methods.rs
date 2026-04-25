//! Per-method JSON-RPC `params` and `result` shapes for A2A 1.0 (ADR 0003).

use serde::{Deserialize, Serialize};
use url::Url;

use crate::ids::TaskId;
use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse};
use crate::types::{JsonObject, Message, Task};

// --- message/send & message/stream ---

/// Parameters for `message/send` and `message/stream` (shape is the same for both).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageSendParams {
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<MessageSendConfiguration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
}

/// Optional tuning for a send (blocking, history length, push URL, output modes).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MessageSendConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_output_modes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_notification_config: Option<PushNotificationConfig>,
}

/// `message/send` and `message/stream` result: a task in progress or a direct agent reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SendMessageResult {
    Task(Task),
    Message(Message),
}

/// Same wire shape as `MessageSendParams`; A2A uses the same `params` for `message/stream`.
pub type MessageStreamParams = MessageSendParams;

// --- tasks/* ---

/// `tasks/cancel` and any method that only needs a task id (plus optional extension metadata).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskIdParams {
    pub id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
}

/// `tasks/get` query: task id, optional history window and extension metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskQueryParams {
    pub id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
}

/// `tasks/cancel` uses the same id-only params as other task-scoped calls.
pub type TaskCancelParams = TaskIdParams;

// --- push notification config ---

/// Webhook (or other) callback configuration for a task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PushNotificationConfig {
    pub url: Url,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authentication: Option<PushNotificationAuthenticationInfo>,
}

/// Optional auth hints (schemes and opaque credentials) for the push target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PushNotificationAuthenticationInfo {
    pub schemes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

/// `tasks/pushNotificationConfig/set` params.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskPushNotificationConfigParams {
    pub task_id: TaskId,
    pub push_notification_config: PushNotificationConfig,
}

/// `tasks/pushNotificationConfig/get` params (task-scoped only).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskPushNotificationGetParams {
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
}

// --- JSON-RPC convenience type aliases ---

/// `message/send` request envelope.
pub type SendMessageRequest = JsonRpcRequest<MessageSendParams>;
/// `message/stream` request envelope.
pub type MessageStreamRequest = JsonRpcRequest<MessageStreamParams>;
/// `tasks/get` request envelope.
pub type TasksGetRequest = JsonRpcRequest<TaskQueryParams>;
/// `tasks/cancel` request envelope.
pub type TasksCancelRequest = JsonRpcRequest<TaskCancelParams>;
/// `tasks/pushNotificationConfig/set` request envelope.
pub type TaskPushNotificationConfigSetRequest = JsonRpcRequest<TaskPushNotificationConfigParams>;
/// `tasks/pushNotificationConfig/get` request envelope.
pub type TaskPushNotificationConfigGetRequest = JsonRpcRequest<TaskPushNotificationGetParams>;

// --- response aliases where `result` is typed (wire servers may return additional shapes in ADR 0008) ---

/// `message/send` and `message/stream` success envelope (`result` = task or final message).
pub type SendMessageResponse = JsonRpcResponse<SendMessageResult>;
/// `tasks/get` success envelope.
pub type TasksGetResponse = JsonRpcResponse<Task>;
/// `tasks/cancel` success envelope.
pub type TasksCancelResponse = JsonRpcResponse<Task>;
