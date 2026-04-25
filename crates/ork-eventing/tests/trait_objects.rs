//! Compile-time check that [`Producer`] and [`Consumer`] are object-safe and that
//! [`ConsumedMessage`] / [`EventingError`] can be moved across thread boundaries — the
//! whole point of these traits is that the rest of ork holds them as `Arc<dyn …>`.

use std::sync::Arc;

use ork_eventing::{ConsumedMessage, Consumer, EventingError, Producer};

fn _accepts_producer(_: Arc<dyn Producer>) {}
fn _accepts_consumer(_: Arc<dyn Consumer>) {}

#[test]
fn consumed_message_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ConsumedMessage>();
}

#[test]
fn eventing_error_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<EventingError>();
}

#[test]
fn consumed_message_round_trip_clone() {
    let m = ConsumedMessage {
        key: Some(b"k".to_vec()),
        headers: vec![("h".to_string(), b"v".to_vec())],
        payload: b"payload".to_vec(),
    };
    assert_eq!(m, m.clone());
}
