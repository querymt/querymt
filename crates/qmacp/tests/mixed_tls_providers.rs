use std::time::Duration;

use qmacp::{client::AcpClient, policy::PermissionPolicy};
use tokio::{io::AsyncReadExt, net::TcpListener, sync::mpsc};

// A separate test binary keeps the process-default provider unset, regardless
// of any provider initialization performed by other tests.
#[tokio::test]
async fn secure_connection_works_with_both_providers_without_installing_a_global_default() {
    let _ring = rustls::crypto::ring::default_provider();
    let _aws_lc = rustls::crypto::aws_lc_rs::default_provider();
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("wss://{}/acp/ws", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut byte = [0];
        tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut byte))
            .await
            .unwrap()
            .unwrap();
        byte[0]
    });
    let (events, _receiver) = mpsc::channel(1);
    let result =
        AcpClient::connect_with_events(&url, "fixture", PermissionPolicy::Cancel, events).await;
    assert!(result.is_err(), "the fixture closes before completing TLS");
    assert_eq!(peer.await.unwrap(), 22, "expected a TLS handshake record");
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_none(),
        "the library must not take over process-wide provider selection"
    );
}
