use ork_a2a::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, MessageSendParams, SendMessageResult, TaskState,
};

const REQ: &str = include_str!("fixtures/jsonrpc_message_send_request.json");
const RESP: &str = include_str!("fixtures/jsonrpc_send_task_response.json");
const ERR: &str = include_str!("fixtures/jsonrpc_error_task_not_found.json");

#[test]
fn message_send_request_validates() {
    let req: JsonRpcRequest<MessageSendParams> = serde_json::from_str(REQ).expect("request");
    req.validate().expect("2.0");
    assert_eq!(req.method, "message/send");
    assert_eq!(req.id, Some(serde_json::json!(1)));
    assert_eq!(req.jsonrpc, "2.0");
}

#[test]
fn send_message_task_result_response() {
    let resp: JsonRpcResponse<SendMessageResult> = serde_json::from_str(RESP).expect("response");
    assert_eq!(resp.jsonrpc, "2.0");
    let r = resp.result.expect("result");
    match r {
        SendMessageResult::Task(t) => {
            assert_eq!(t.status.state, TaskState::Submitted);
        }
        SendMessageResult::Message(_) => panic!("expected Task"),
    }
}

#[test]
fn error_response_task_not_found() {
    let resp: JsonRpcResponse<SendMessageResult> = serde_json::from_str(ERR).expect("err response");
    assert!(resp.result.is_none());
    let e = resp.error.expect("error");
    assert_eq!(e.code, JsonRpcError::TASK_NOT_FOUND);
}
