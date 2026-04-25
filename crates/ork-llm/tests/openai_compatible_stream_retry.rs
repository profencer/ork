//! Regression for `docs/incidents/2026-04-25-workflow-cascades-past-failed-step.md`.
//!
//! When [`OpenAiCompatibleProvider::chat_stream`] hits a transient
//! mid-stream HTTP failure (e.g. the upstream gateway closes the
//! connection before any SSE event is emitted, surfacing as
//! `error decoding response body` from `reqwest::Response::bytes_stream`),
//! the provider must retry the request once before propagating the
//! error to the caller. The retry only triggers when **no** events
//! have been yielded yet — once the consumer has seen any partial
//! content, replaying the request would corrupt the response.
//!
//! We avoid `wiremock` here because it controls the HTTP framing and
//! cannot simulate a connection-close mid-body. A hand-rolled tokio
//! TCP listener gives us byte-level control: the first connection
//! sends headers + a partial body that is shorter than the declared
//! `Content-Length`, then drops; the second connection serves a
//! complete SSE response. The provider must retry and the stream must
//! reach the `Done` event.
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt;
use ork_core::ports::llm::{ChatMessage, ChatRequest, ChatStreamEvent, LlmProvider};
use ork_llm::openai_compatible::OpenAiCompatibleProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Drains the request bytes until we see `\r\n\r\n` (end of HTTP
/// headers). For a tiny streaming-chat POST the body fits in a single
/// kernel buffer, so a few extra reads after the header terminator are
/// enough to consume it. The handler does not actually parse the body —
/// it just needs to drain enough that `write_all` doesn't race a
/// half-written request.
async fn drain_request_headers(sock: &mut tokio::net::TcpStream) {
    let mut buf = vec![0u8; 4096];
    let mut acc = Vec::new();
    while !acc.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if acc.len() > 64 * 1024 {
            break;
        }
    }
    // One more best-effort read so any small chat-completions body
    // chunk gets pulled off the socket too.
    let _ = tokio::time::timeout(Duration::from_millis(50), sock.read(&mut buf)).await;
}

const SSE_BODY_OK: &str = concat!(
    "data: {\"id\":\"chatcmpl-ok\",\"object\":\"chat.completion.chunk\",\
     \"created\":0,\"model\":\"test-model\",\
     \"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-ok\",\"object\":\"chat.completion.chunk\",\
     \"created\":0,\"model\":\"test-model\",\
     \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn chat_stream_retries_once_on_transient_mid_stream_truncation() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_server = Arc::clone(&attempts);

    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let attempts = Arc::clone(&attempts_for_server);
            tokio::spawn(async move {
                drain_request_headers(&mut sock).await;
                let attempt_no = attempts.fetch_add(1, Ordering::SeqCst);
                if attempt_no == 0 {
                    // Promise 1024 body bytes, send 0, close. reqwest's
                    // chunked/length-bound body decoder yields an error
                    // on the next `bytes_stream` poll — the same shape
                    // the live Minimax/Kong incident hit.
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: 1024\r\n\r\n";
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                } else {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\n\r\n{}",
                        SSE_BODY_OK.len(),
                        SSE_BODY_OK
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
            });
        }
    });

    let provider = OpenAiCompatibleProvider::new(
        "minimax",
        format!("http://{addr}"),
        Some("test-model".to_string()),
        HashMap::new(),
        vec![],
    );

    let req = ChatRequest {
        messages: vec![ChatMessage::user("ping")],
        temperature: Some(0.0),
        max_tokens: Some(8),
        model: None,
        provider: None,
        tools: Vec::new(),
        tool_choice: None,
    };

    let mut stream = provider
        .chat_stream(req)
        .await
        .expect("chat_stream returns Ok at the request layer (retry happens inside the stream)");

    let mut got_done = false;
    let mut errs = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ChatStreamEvent::Done { .. }) => got_done = true,
            Ok(_) => {}
            Err(e) => errs.push(e.to_string()),
        }
    }

    server.abort();

    assert!(
        errs.is_empty(),
        "expected the stream to complete cleanly after one retry, got errors: {errs:?}"
    );
    assert!(
        got_done,
        "expected a Done event after the second attempt's complete SSE response"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "exactly two HTTP attempts should reach the server: the truncated first and the successful retry"
    );
}

#[tokio::test]
async fn chat_stream_does_not_retry_after_emitting_events() {
    // Once the consumer has seen any chunk, retrying would corrupt the
    // response. Regression for the safety side of the retry contract.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_server = Arc::clone(&attempts);

    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let attempts = Arc::clone(&attempts_for_server);
            tokio::spawn(async move {
                drain_request_headers(&mut sock).await;
                attempts.fetch_add(1, Ordering::SeqCst);
                // Emit one valid SSE event, then close before the
                // declared Content-Length finishes. The consumer
                // already received that event, so retrying would
                // duplicate it — the stream must surface the error
                // instead.
                let body = "data: {\"id\":\"chatcmpl\",\"object\":\"chat.completion.chunk\",\
                            \"created\":0,\"model\":\"test-model\",\
                            \"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\
                            \"finish_reason\":null}]}\n\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: 4096\r\n\r\n{body}"
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    let provider = OpenAiCompatibleProvider::new(
        "minimax",
        format!("http://{addr}"),
        Some("test-model".to_string()),
        HashMap::new(),
        vec![],
    );

    let req = ChatRequest {
        messages: vec![ChatMessage::user("ping")],
        temperature: Some(0.0),
        max_tokens: Some(8),
        model: None,
        provider: None,
        tools: Vec::new(),
        tool_choice: None,
    };

    let mut stream = provider.chat_stream(req).await.expect("ok");
    let mut saw_message = false;
    let mut saw_error = false;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ChatStreamEvent::Delta(_)) => saw_message = true,
            Ok(_) => {}
            Err(_) => {
                saw_error = true;
                break;
            }
        }
    }

    server.abort();

    assert!(
        saw_message,
        "first attempt should still surface the partial content event"
    );
    assert!(
        saw_error,
        "after content was emitted, the mid-stream truncation must propagate as an Err"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "once an event has been emitted the provider must NOT retry — replay would corrupt the response"
    );
}

#[tokio::test]
async fn chat_stream_retries_once_on_transient_initial_send_error() {
    // Reproduces the live demo error
    //   `LLM provider error: request failed: error sending request for url (...)`
    // that surfaces when the upstream TCP/TLS handshake is reset before
    // any HTTP response bytes arrive. The first retry path in this
    // file covers mid-stream truncation; this one covers the symmetric
    // case where reqwest's `send().await` itself fails. The provider
    // must retry once before propagating the error.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_server = Arc::clone(&attempts);

    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let attempts = Arc::clone(&attempts_for_server);
            tokio::spawn(async move {
                let attempt_no = attempts.fetch_add(1, Ordering::SeqCst);
                if attempt_no == 0 {
                    // Drop the connection without sending any HTTP
                    // response. reqwest sees an early EOF / connection
                    // close while waiting on the response head and
                    // returns a transport error.
                    drop(sock);
                } else {
                    drain_request_headers(&mut sock).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\n\r\n{}",
                        SSE_BODY_OK.len(),
                        SSE_BODY_OK
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
            });
        }
    });

    let provider = OpenAiCompatibleProvider::new(
        "minimax",
        format!("http://{addr}"),
        Some("test-model".to_string()),
        HashMap::new(),
        vec![],
    );

    let req = ChatRequest {
        messages: vec![ChatMessage::user("ping")],
        temperature: Some(0.0),
        max_tokens: Some(8),
        model: None,
        provider: None,
        tools: Vec::new(),
        tool_choice: None,
    };

    let mut stream = provider
        .chat_stream(req)
        .await
        .expect("chat_stream returns Ok after the initial-send retry succeeds");

    let mut got_done = false;
    let mut errs = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ChatStreamEvent::Done { .. }) => got_done = true,
            Ok(_) => {}
            Err(e) => errs.push(e.to_string()),
        }
    }

    server.abort();

    assert!(
        errs.is_empty(),
        "expected the stream to complete cleanly after one initial-send retry, got errors: {errs:?}"
    );
    assert!(got_done, "expected a Done event after the retry");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "exactly two HTTP attempts should reach the listener: dropped first + successful second"
    );
}

#[tokio::test]
async fn chat_stream_does_not_retry_on_4xx_response() {
    // A 4xx is fatal (auth / validation / quota). Retrying just burns
    // budget that the next request might need. Regression for the
    // safety side of the initial-send retry contract.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid token"))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenAiCompatibleProvider::new(
        "minimax",
        server.uri(),
        Some("test-model".to_string()),
        HashMap::new(),
        vec![],
    );
    let req = ChatRequest {
        messages: vec![ChatMessage::user("ping")],
        temperature: Some(0.0),
        max_tokens: Some(8),
        model: None,
        provider: None,
        tools: Vec::new(),
        tool_choice: None,
    };

    let result = provider.chat_stream(req).await;
    let err = match result {
        Ok(_stream) => panic!("4xx must surface synchronously, not via the stream"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("401") && msg.contains("invalid token"),
        "expected the 4xx body to be preserved verbatim, got {msg:?}"
    );
    // Mock::expect(1) on drop will panic if a retry fires, providing
    // the second axis of the assertion.
}
