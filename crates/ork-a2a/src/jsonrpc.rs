use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ids::TaskId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest<P> {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    pub params: Option<P>,
}

impl<P> JsonRpcRequest<P> {
    #[must_use]
    pub fn new(id: Option<serde_json::Value>, method: A2aMethod, params: Option<P>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_wire_string().to_string(),
            params,
        }
    }

    /// Ensures the envelope declares JSON-RPC 2.0. Wire handlers should call this before dispatch.
    pub fn validate(&self) -> Result<(), JsonRpcError> {
        if self.jsonrpc != "2.0" {
            return Err(JsonRpcError {
                code: JsonRpcError::INVALID_REQUEST,
                message: "jsonrpc must be \"2.0\"".to_string(),
                data: None,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse<R> {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl<R> JsonRpcResponse<R> {
    /// Successful response with a `result` and no `error`.
    #[must_use]
    pub fn ok(id: Option<serde_json::Value>, result: R) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Error response with an `error` and no `result`.
    #[must_use]
    pub fn err(id: Option<serde_json::Value>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    // JSON-RPC 2.0
    pub const PARSE_ERROR: i32 = -32_700;
    pub const INVALID_REQUEST: i32 = -32_600;
    pub const METHOD_NOT_FOUND: i32 = -32_601;
    pub const INVALID_PARAMS: i32 = -32_602;
    pub const INTERNAL_ERROR: i32 = -32_603;

    // A2A application codes (aligned with SAM `common/a2a/protocol.py` conventions)
    pub const TASK_NOT_FOUND: i32 = -32_001;
    pub const TASK_NOT_CANCELABLE: i32 = -32_002;
    pub const PUSH_NOTIFICATION_NOT_SUPPORTED: i32 = -32_003;
    pub const UNSUPPORTED_OPERATION: i32 = -32_004;
    pub const CONTENT_TYPE_NOT_SUPPORTED: i32 = -32_005;
    pub const INVALID_AGENT_RESPONSE: i32 = -32_006;

    /// Invalid or missing `params` for the invoked method.
    #[must_use]
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: Self::INVALID_PARAMS,
            message: msg.into(),
            data: None,
        }
    }

    /// Unknown `method` string.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: Self::METHOD_NOT_FOUND,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }

    /// Referenced task id does not exist.
    #[must_use]
    pub fn task_not_found(id: &TaskId) -> Self {
        Self {
            code: Self::TASK_NOT_FOUND,
            message: format!("Task not found: {id}"),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A2aMethod {
    MessageSend,
    MessageStream,
    TasksGet,
    TasksCancel,
    TasksPushNotificationConfigSet,
    TasksPushNotificationConfigGet,
}

impl A2aMethod {
    #[must_use]
    pub const fn to_wire_string(self) -> &'static str {
        match self {
            Self::MessageSend => "message/send",
            Self::MessageStream => "message/stream",
            Self::TasksGet => "tasks/get",
            Self::TasksCancel => "tasks/cancel",
            Self::TasksPushNotificationConfigSet => "tasks/pushNotificationConfig/set",
            Self::TasksPushNotificationConfigGet => "tasks/pushNotificationConfig/get",
        }
    }
}

impl fmt::Display for A2aMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.to_wire_string())
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown A2A JSON-RPC method: {0}")]
pub struct UnknownA2aMethod(pub String);

impl FromStr for A2aMethod {
    type Err = UnknownA2aMethod;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "message/send" => Ok(Self::MessageSend),
            "message/stream" => Ok(Self::MessageStream),
            "tasks/get" => Ok(Self::TasksGet),
            "tasks/cancel" => Ok(Self::TasksCancel),
            "tasks/pushNotificationConfig/set" => Ok(Self::TasksPushNotificationConfigSet),
            "tasks/pushNotificationConfig/get" => Ok(Self::TasksPushNotificationConfigGet),
            other => Err(UnknownA2aMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2a_method_roundtrip_all() {
        for m in [
            A2aMethod::MessageSend,
            A2aMethod::MessageStream,
            A2aMethod::TasksGet,
            A2aMethod::TasksCancel,
            A2aMethod::TasksPushNotificationConfigSet,
            A2aMethod::TasksPushNotificationConfigGet,
        ] {
            let s = m.to_wire_string();
            let p: A2aMethod = s.parse().expect("from_str");
            assert_eq!(p, m);
        }
    }

    #[test]
    fn json_rpc_request_validate_2_0() {
        let r = JsonRpcRequest::<()> {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: "x".to_string(),
            params: None,
        };
        assert!(r.validate().is_ok());
        let r_bad = JsonRpcRequest::<()> {
            jsonrpc: "1.0".to_string(),
            id: None,
            method: "x".to_string(),
            params: None,
        };
        let e = r_bad.validate().unwrap_err();
        assert_eq!(e.code, JsonRpcError::INVALID_REQUEST);
    }

    #[test]
    fn jsonrpc_error_serde_int_code() {
        let e = JsonRpcError::task_not_found(&TaskId::new());
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v.get("code"),
            Some(&serde_json::Value::from(JsonRpcError::TASK_NOT_FOUND))
        );
    }
}
