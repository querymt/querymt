use super::{OutboundStreamResult, call_outbound_stream_raw};
use http::Request;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One-shot HTTP/1.1 responder on a random localhost port.
async fn serve_once(
    status_line: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");
    let body = body.to_vec();
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let status_line = status_line.to_owned();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        let _ = socket.read(&mut buf).await;
        let mut response = format!("{status_line}\r\nContent-Length: {}\r\n", body.len());
        for (k, v) in &headers {
            response.push_str(&format!("{k}: {v}\r\n"));
        }
        response.push_str("Connection: close\r\n\r\n");
        socket.write_all(response.as_bytes()).await.expect("write hdr");
        socket.write_all(&body).await.expect("write body");
    });

    format!("http://{addr}/stream")
}

#[tokio::test]
async fn stream_raw_non_success_is_failure_with_status_headers_body() {
    let uri = serve_once(
        "HTTP/1.1 429 Too Many Requests",
        &[
            ("Retry-After", "11"),
            ("x-request-id", "req-stream-1"),
            ("Content-Type", "application/json"),
        ],
        br#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#,
    )
    .await;

    let req = Request::builder()
        .method("POST")
        .uri(&uri)
        .body(Vec::new())
        .expect("request");

    let result = call_outbound_stream_raw(req)
        .await
        .expect("transport should succeed");

    match result {
        OutboundStreamResult::Failure { response } => {
            assert_eq!(response.status().as_u16(), 429);
            assert_eq!(
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
                Some("11")
            );
            assert_eq!(
                response
                    .headers()
                    .get("x-request-id")
                    .and_then(|v| v.to_str().ok()),
                Some("req-stream-1")
            );
            assert!(
                std::str::from_utf8(response.body())
                    .unwrap_or_default()
                    .contains("slow down"),
                "body should be preserved: {:?}",
                String::from_utf8_lossy(response.body())
            );
        }
        OutboundStreamResult::Success { .. } => {
            panic!("non-success status must not open a success stream")
        }
    }
}

#[tokio::test]
async fn stream_raw_success_returns_stream_variant() {
    let uri = serve_once(
        "HTTP/1.1 200 OK",
        &[("Content-Type", "text/event-stream")],
        b"data: {\"ok\":true}\n\n",
    )
    .await;

    let req = Request::builder()
        .method("POST")
        .uri(&uri)
        .body(Vec::new())
        .expect("request");

    let result = call_outbound_stream_raw(req)
        .await
        .expect("transport should succeed");

    match result {
        OutboundStreamResult::Success { response, stream: _ } => {
            assert_eq!(response.status().as_u16(), 200);
        }
        OutboundStreamResult::Failure { response } => {
            panic!(
                "expected Success, got Failure status={}",
                response.status()
            )
        }
    }
}
