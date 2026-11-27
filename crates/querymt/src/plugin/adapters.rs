use super::{AccountReport, Fut, LLMProviderFactory, http::HTTPLLMProviderFactory};
use crate::{LLMProvider, adapters::LLMProviderFromHTTP, error::LLMError, outbound::call_outbound};
use futures::future::FutureExt;
use http::{Request, Response};
use std::{ops::Deref, sync::Arc};

pub struct HTTPFactoryAdapter {
    inner: Arc<dyn HTTPLLMProviderFactory>,
}

impl HTTPFactoryAdapter {
    pub fn new(inner: Arc<dyn HTTPLLMProviderFactory>) -> Self {
        Self { inner }
    }
}

impl LLMProviderFactory for HTTPFactoryAdapter {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn as_http(&self) -> Option<&dyn super::http::HTTPLLMProviderFactory> {
        Some(self.inner.deref())
    }

    fn supports_custom_models(&self) -> bool {
        self.inner.supports_custom_models()
    }

    fn config_schema(&self) -> String {
        self.inner.config_schema()
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        let sync_provider = self.inner.from_config(cfg)?;
        let adapter = LLMProviderFromHTTP::new(sync_provider).with_name(self.inner.name());
        Ok(Box::new(adapter))
    }

    fn list_models<'a>(&'a self, cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        // clone the Arc so we can move it into the async block
        let inner = Arc::clone(&self.inner);
        let cloned_cfg = cfg.to_string();

        async move {
            if let Some(result) = inner.list_models_static(&cloned_cfg) {
                return result;
            }

            let req: Request<Vec<u8>> = inner.list_models_request(&cloned_cfg)?;
            let resp: Response<Vec<u8>> = call_outbound(req).await?;

            inner.parse_list_models(resp)
        }
        .boxed()
    }

    fn account<'a>(&'a self, cfg: &str) -> Fut<'a, Result<AccountReport, LLMError>> {
        let inner = Arc::clone(&self.inner);
        let cfg = cfg.to_string();

        async move {
            let Some(req) = inner.account_request(&cfg)? else {
                return Ok(AccountReport::Unsupported);
            };
            // `call_outbound` classifies non-success statuses.
            let resp = call_outbound(req).await?;
            inner.parse_account(resp).map(AccountReport::Account)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct ErrorFactory {
        list_models_uri: Option<String>,
    }

    impl ErrorFactory {
        fn new(list_models_uri: Option<String>) -> Self {
            Self { list_models_uri }
        }
    }

    impl HTTPLLMProviderFactory for ErrorFactory {
        fn name(&self) -> &str {
            "error-factory"
        }

        fn config_schema(&self) -> String {
            "{}".into()
        }

        fn list_models_request(&self, _cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
            let uri = self
                .list_models_uri
                .as_deref()
                .ok_or_else(|| LLMError::NotImplemented("unused".into()))?;
            Request::get(uri)
                .body(Vec::new())
                .map_err(|error| LLMError::InvalidRequest(error.to_string()))
        }

        fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
            Err(LLMError::JsonError("bad models response".into()))
        }

        fn from_config(&self, _cfg: &str) -> Result<Box<dyn crate::HTTPLLMProvider>, LLMError> {
            Err(LLMError::InvalidRequest("bad provider config".into()))
        }
    }

    async fn serve_once() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .expect("write response");
        });

        format!("http://{addr}/models")
    }

    #[test]
    fn from_config_preserves_typed_error() {
        let adapter = HTTPFactoryAdapter::new(Arc::new(ErrorFactory::new(None)));
        let error = adapter.from_config("{}").err().expect("config should fail");

        assert!(!error.is_retryable());
        assert!(matches!(
            error,
            LLMError::InvalidRequest(message) if message == "bad provider config"
        ));
    }

    #[tokio::test]
    async fn parse_list_models_preserves_typed_error() {
        let adapter =
            HTTPFactoryAdapter::new(Arc::new(ErrorFactory::new(Some(serve_once().await))));
        let error = adapter
            .list_models("{}")
            .await
            .expect_err("response parsing should fail");

        assert!(!error.is_retryable());
        assert!(matches!(
            error,
            LLMError::JsonError(message) if message == "bad models response"
        ));
    }

    mod account {
        use super::*;
        use crate::plugin::{ProviderAccount, UsageLimit};
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// An HTTP factory whose account request targets `uri` and whose parser
        /// reads `{"used": <percent>}` and requires an `x-account` header.
        struct AccountFactory {
            uri: String,
        }

        impl HTTPLLMProviderFactory for AccountFactory {
            fn name(&self) -> &str {
                "account-factory"
            }

            fn config_schema(&self) -> String {
                "{}".into()
            }

            fn list_models_request(&self, _cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
                Err(LLMError::NotImplemented("unused".into()))
            }

            fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
                Err(LLMError::NotImplemented("unused".into()))
            }

            fn from_config(&self, _cfg: &str) -> Result<Box<dyn crate::HTTPLLMProvider>, LLMError> {
                Err(LLMError::NotImplemented("unused".into()))
            }

            fn account_request(&self, _cfg: &str) -> Result<Option<Request<Vec<u8>>>, LLMError> {
                Request::get(self.uri.as_str())
                    .body(Vec::new())
                    .map(Some)
                    .map_err(|error| LLMError::InvalidRequest(error.to_string()))
            }

            fn parse_account(&self, resp: Response<Vec<u8>>) -> Result<ProviderAccount, LLMError> {
                let invalid = |message: &str| LLMError::ResponseFormatError {
                    message: message.to_string(),
                    raw_response: String::from_utf8_lossy(resp.body()).into_owned(),
                };
                if resp.headers().get("x-account").is_none() {
                    return Err(invalid("missing x-account header"));
                }
                let body: serde_json::Value =
                    serde_json::from_slice(resp.body()).map_err(|_| invalid("not JSON"))?;
                let used = body
                    .get("used")
                    .and_then(serde_json::Value::as_f64)
                    .ok_or_else(|| invalid("missing used"))?;
                let mut account = ProviderAccount::new();
                account.push_limit(UsageLimit::new("weekly", used));
                Ok(account)
            }
        }

        /// Serves `count` requests with one canned response; returns the base URI
        /// and the number of requests received.
        async fn serve(
            count: usize,
            status_line: &'static str,
            headers: &'static str,
            body: &'static str,
        ) -> (String, Arc<AtomicUsize>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind test listener");
            let addr = listener.local_addr().expect("local addr");
            let hits = Arc::new(AtomicUsize::new(0));
            let served = Arc::clone(&hits);
            tokio::spawn(async move {
                for _ in 0..count {
                    let (mut socket, _) = listener.accept().await.expect("accept");
                    let mut request = [0; 1024];
                    let _ = socket.read(&mut request).await;
                    served.fetch_add(1, Ordering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("write response");
                }
            });
            (format!("http://{addr}/usage"), hits)
        }

        #[tokio::test]
        async fn default_factory_reports_unsupported() {
            let adapter = HTTPFactoryAdapter::new(Arc::new(ErrorFactory::new(None)));

            let report = adapter.account("{}").await.expect("account");

            assert_eq!(report, AccountReport::Unsupported);
        }

        #[tokio::test]
        async fn http_factory_snapshot_is_fetched_by_the_host() {
            let (uri, hits) = serve(1, "200 OK", "x-account: yes\r\n", r#"{"used":42.5}"#).await;
            let adapter = HTTPFactoryAdapter::new(Arc::new(AccountFactory { uri }));

            let report = adapter.account("{}").await.expect("account");

            let AccountReport::Account(account) = report else {
                panic!("expected a snapshot, got {report:?}");
            };
            assert_eq!(account.limits, [UsageLimit::new("weekly", 42.5)]);
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn unauthorized_account_endpoint_is_classified() {
            let (uri, _) = serve(1, "401 Unauthorized", "", r#"{"error":"expired"}"#).await;
            let adapter = HTTPFactoryAdapter::new(Arc::new(AccountFactory { uri }));

            let error = adapter
                .account("{}")
                .await
                .expect_err("401 must not become a snapshot");

            assert!(matches!(error, LLMError::AuthError(_)), "{error:?}");
        }

        #[tokio::test]
        async fn malformed_account_body_is_a_parse_error() {
            let (uri, _) = serve(1, "200 OK", "x-account: yes\r\n", "not json").await;
            let adapter = HTTPFactoryAdapter::new(Arc::new(AccountFactory { uri }));

            let error = adapter
                .account("{}")
                .await
                .expect_err("an unreadable body must not become a snapshot");

            assert!(
                matches!(error, LLMError::ResponseFormatError { .. }),
                "{error:?}"
            );
        }
    }
}
