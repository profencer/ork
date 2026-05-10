//! Boots [`OrkApp::serve`] with [`ork_server::AxumServer`](../../ork-server/).

use std::sync::Arc;
use std::time::Duration;

use ork_app::OrkApp;
use ork_app::types::ServerConfig;
use ork_server::AxumServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn healthz_then_shutdown_under_five_seconds() {
    let app = OrkApp::builder()
        .server(ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            ..ServerConfig::default()
        })
        .serve_backend(Arc::new(AxumServer))
        .build()
        .expect("build app");

    let handle = app.serve().await.expect("serve");
    let addr = handle.local_addr;

    let body = tokio::time::timeout(Duration::from_secs(5), async {
        let mut tcp = TcpStream::connect(addr).await?;
        tcp.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut buf = vec![0u8; 512];
        let n = tcp.read(&mut buf).await?;
        Result::<_, std::io::Error>::Ok(buf[..n].to_vec())
    })
    .await
    .expect("timed out waiting for GET /healthz (budget 5s)");

    let body = body.expect("http read");
    let head = String::from_utf8_lossy(&body);
    assert!(
        head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200"),
        "unexpected response: {:?}",
        head.lines().next()
    );

    handle
        .shutdown()
        .await
        .expect("graceful shutdown within 5s");

    let reconn = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await;

    match reconn {
        Err(_) => panic!("still waiting on connect after shutdown (5s timeout)"),
        Ok(Ok(_)) => panic!("unexpected successful TCP reconnect after shutdown"),
        Ok(Err(_)) => {}
    }
}
