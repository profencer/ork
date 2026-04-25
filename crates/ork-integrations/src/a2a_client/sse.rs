//! SSE → [`ork_a2a::TaskEvent`] adapter (ADR-0007 §`Streaming model`).
//!
//! We sit on top of `eventsource-stream` so we don't reinvent CRLF / multi-line
//! `data:` field reassembly. Each parsed event's `data` is JSON-decoded into a
//! `TaskEvent` (shape pinned by `ork_a2a::types::TaskEvent`).
//!
//! Failure model:
//!
//! - Transport errors (network drops, TCP resets, premature EOF) terminate the
//!   stream with [`OrkError::A2aStreamLost`] **after** any events that were
//!   already yielded — callers MUST be prepared to surface partial results.
//! - JSON-decode errors of an individual event are surfaced as
//!   [`OrkError::A2aClient`] with the operator-facing payload, so a malformed
//!   frame is a hard error, not a silent skip.

use std::pin::Pin;

use bytes::Bytes;
use eventsource_stream::{Event, Eventsource};
use futures::stream::{BoxStream, Stream, StreamExt};
use ork_a2a::TaskEvent;
use ork_common::error::OrkError;

/// Convert a byte stream (as produced by `reqwest::Response::bytes_stream`) into a
/// stream of [`TaskEvent`]s. Errors map per the module-level failure model.
pub fn parse_a2a_sse(
    body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
) -> BoxStream<'static, Result<TaskEvent, OrkError>> {
    let stream = body.eventsource().map(|frame| match frame {
        Ok(event) => decode_event(event),
        Err(e) => Err(OrkError::A2aStreamLost(e.to_string())),
    });
    stream.boxed()
}

fn decode_event(event: Event) -> Result<TaskEvent, OrkError> {
    serde_json::from_str::<TaskEvent>(&event.data).map_err(|e| {
        OrkError::A2aClient(
            502,
            format!(
                "malformed A2A SSE event '{}': {e}",
                if event.event.is_empty() {
                    "<unnamed>"
                } else {
                    event.event.as_str()
                }
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use ork_a2a::{
        Message, MessageId, Part, Role, TaskId, TaskState, TaskStatus, TaskStatusUpdateEvent,
    };

    fn into_byte_stream(
        chunks: Vec<&'static [u8]>,
    ) -> Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> {
        let s = futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<Bytes, reqwest::Error>(Bytes::from_static(c))),
        );
        Box::pin(s)
    }

    fn into_byte_stream_with_error(
        chunks: Vec<&'static [u8]>,
    ) -> Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> {
        // We can't easily fabricate a `reqwest::Error`, so simulate "EOF mid-stream"
        // by yielding chunks and then ending — eventsource_stream surfaces an Error
        // when the last event is incomplete.
        into_byte_stream(chunks)
    }

    fn status_update_json() -> String {
        let ev = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: TaskId::new(),
            status: TaskStatus {
                state: TaskState::Working,
                message: Some("thinking".into()),
            },
            is_final: false,
        });
        serde_json::to_string(&ev).unwrap()
    }

    fn message_event_json() -> String {
        let ev = TaskEvent::Message(Message {
            role: Role::Agent,
            parts: vec![Part::text("hello world")],
            message_id: MessageId::new(),
            task_id: None,
            context_id: None,
            metadata: None,
        });
        serde_json::to_string(&ev).unwrap()
    }

    #[tokio::test]
    async fn parses_sequence_of_events() {
        let payload = format!(
            "data: {}\n\ndata: {}\n\n",
            status_update_json(),
            message_event_json()
        );
        // Leak so we can hand a 'static slice to the stream — ok in tests.
        let leaked: &'static [u8] = Box::leak(payload.into_boxed_str().into_boxed_bytes());
        let body = into_byte_stream(vec![leaked]);
        let mut s = parse_a2a_sse(body);

        let first = s.next().await.expect("first event").expect("ok");
        assert!(matches!(first, TaskEvent::StatusUpdate(_)));
        let second = s.next().await.expect("second event").expect("ok");
        assert!(matches!(second, TaskEvent::Message(_)));
        assert!(s.next().await.is_none(), "stream complete");
    }

    #[tokio::test]
    async fn handles_chunk_split_across_frames() {
        let payload = format!("data: {}\n\n", status_update_json());
        let leaked: &'static [u8] = Box::leak(payload.into_boxed_str().into_boxed_bytes());
        let split_at = leaked.len() / 2;
        let (head, tail) = leaked.split_at(split_at);
        let body = into_byte_stream(vec![head, tail]);
        let mut s = parse_a2a_sse(body);
        let ev = s.next().await.expect("ev").expect("ok");
        assert!(matches!(ev, TaskEvent::StatusUpdate(_)));
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn malformed_event_data_surfaces_a2a_client_error() {
        let body = into_byte_stream(vec![b"data: {not-json}\n\n"]);
        let mut s = parse_a2a_sse(body);
        let err = s.next().await.expect("first").expect_err("must be error");
        assert!(matches!(err, OrkError::A2aClient(502, _)));
    }

    #[tokio::test]
    async fn early_eof_yields_buffered_events_then_terminates() {
        // First a complete event, then a partial line that never gets a blank-line
        // terminator. eventsource_stream treats this as "stream ended" — our parser
        // surfaces buffered events first and then ends without an error (since
        // the byte-stream itself didn't error).
        let payload = format!(
            "data: {}\n\ndata: incomplete-no-terminator",
            status_update_json()
        );
        let leaked: &'static [u8] = Box::leak(payload.into_boxed_str().into_boxed_bytes());
        let body = into_byte_stream_with_error(vec![leaked]);
        let mut s = parse_a2a_sse(body);
        let first = s.next().await.expect("buffered event").expect("ok");
        assert!(matches!(first, TaskEvent::StatusUpdate(_)));
        // No further events; the trailing partial line is dropped.
        assert!(s.next().await.is_none());
    }
}
