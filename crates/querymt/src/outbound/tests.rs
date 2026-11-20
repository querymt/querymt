use super::{OutboundStreamResult, call_outbound_stream_raw};
use http::Request;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One-shot HTTP/1.1 responder on a random localhost port.
async fn serve_once(status_line: &str, headers: &[(&str, &str)], body: &[u8]) -> String {
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
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write hdr");
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
        OutboundStreamResult::Success { stream: _ } => {}
        OutboundStreamResult::Failure { response } => {
            panic!("expected Success, got Failure status={}", response.status())
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod bundled_roots {
    use super::super::http_client::imp::build_client_with;
    use std::cell::{Cell, RefCell};

    /// A genuine `reqwest::Error` to stand in for "no system roots".
    fn builder_error() -> reqwest::Error {
        reqwest::Proxy::all("http://[").expect_err("invalid proxy url")
    }

    #[test]
    fn fallback_client_used_when_no_system_roots() {
        let attempts = RefCell::new(Vec::new());
        let configured = Cell::new(0);
        let warnings = RefCell::new(Vec::new());

        let client = build_client_with(
            |builder| {
                configured.set(configured.get() + 1);
                builder.user_agent("test-agent")
            },
            |builder, bundled| {
                attempts.borrow_mut().push(bundled);
                if bundled {
                    builder.build()
                } else {
                    Err(builder_error())
                }
            },
            |message| warnings.borrow_mut().push(message),
        );

        assert!(client.is_ok(), "the retry result is returned");
        assert_eq!(*attempts.borrow(), vec![false, true]);
        assert_eq!(configured.get(), 2, "configuration applies to both builds");
        let warnings = warnings.borrow();
        assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
        assert!(
            warnings[0].contains("bundled root certificates"),
            "warning names the fallback: {}",
            warnings[0]
        );
    }

    #[test]
    fn no_fallback_when_the_first_build_succeeds() {
        let attempts = RefCell::new(Vec::new());
        let warnings = RefCell::new(Vec::new());

        let client = build_client_with(
            |builder| builder,
            |builder, bundled| {
                attempts.borrow_mut().push(bundled);
                builder.build()
            },
            |message| warnings.borrow_mut().push(message),
        );

        assert!(client.is_ok());
        assert_eq!(*attempts.borrow(), vec![false]);
        assert!(warnings.borrow().is_empty());
    }

    #[test]
    fn failure_of_the_retry_is_returned() {
        let result = build_client_with(|builder| builder, |_, _| Err(builder_error()), |_| {});
        assert!(result.is_err());
    }

    #[test]
    fn bundled_roots_are_all_parseable() {
        let roots = super::super::http_client::imp::bundled_roots();
        assert_eq!(roots.len(), webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
    }
}

/// Needs the rustls platform verifier to read `SSL_CERT_FILE`/`SSL_CERT_DIR`,
/// which it does on Linux. On macOS the verifier uses Security.framework and
/// ignores them, so the precondition cannot be created there.
///
/// The environment is process-global, so the test re-runs itself in a child
/// process that has the variables set from the start.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn client_builds_without_system_certificates() {
    const CHILD: &str = "QMT_TEST_NO_SYSTEM_CERTS_CHILD";

    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty_file = dir.path().join("empty.pem");
        std::fs::write(&empty_file, b"").expect("empty file");
        let empty_dir = dir.path().join("certs");
        std::fs::create_dir(&empty_dir).expect("empty dir");

        let exe = std::env::current_exe().expect("current exe");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "outbound::tests::client_builds_without_system_certificates",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("SSL_CERT_FILE", &empty_file)
            .env("SSL_CERT_DIR", &empty_dir)
            .output()
            .expect("spawn child");
        assert!(
            output.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    // Precondition: without the fallback there is no way to build a client.
    assert!(
        reqwest::Client::builder().build().is_err(),
        "plain Client::builder().build() must fail with an empty system store"
    );

    let client = super::build_client(|builder| builder).expect("build_client falls back");
    let uri = serve_once("HTTP/1.1 200 OK", &[], b"ok").await;
    let response = client.get(&uri).send().await.expect("plain http request");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.expect("body"), "ok");
}
