//! Live Kafka round-trip test for [`RsKafkaBackend`]. Runs only when `RSKAFKA_BROKERS` is
//! set in the environment so CI without a broker stays green.
//!
//! Example:
//!
//! ```bash
//! RSKAFKA_BROKERS=localhost:9092 \
//!   cargo test -p ork-eventing --test rskafka_roundtrip -- --ignored --nocapture
//! ```

use std::time::Duration;

use futures::StreamExt;
use ork_eventing::{Consumer, Producer, RsKafkaBackend};
use tokio::time::timeout;

#[tokio::test]
#[ignore = "requires a live Kafka broker; set RSKAFKA_BROKERS=host:port"]
async fn rskafka_publish_subscribe_roundtrip() {
    let brokers_env =
        std::env::var("RSKAFKA_BROKERS").expect("set RSKAFKA_BROKERS=host:port to run this test");
    let brokers: Vec<String> = brokers_env.split(',').map(str::to_owned).collect();

    let backend = RsKafkaBackend::connect(brokers)
        .await
        .expect("connect to broker");

    let topic = format!("ork.test.{}", chrono::Utc::now().format("%Y%m%d_%H%M%S_%f"));

    let mut stream = backend.subscribe(&topic).await.expect("subscribe");

    let headers = vec![("ork-a2a-version".to_string(), b"1.0".to_vec())];
    backend
        .publish(&topic, Some(b"k"), &headers, b"hello")
        .await
        .expect("publish");

    let got = timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("recv timeout")
        .expect("stream ended")
        .expect("stream error");

    assert_eq!(got.payload, b"hello");
    assert_eq!(got.key.as_deref(), Some(b"k".as_ref()));
    assert!(
        got.headers
            .iter()
            .any(|(k, v)| k == "ork-a2a-version" && v == b"1.0")
    );
}
