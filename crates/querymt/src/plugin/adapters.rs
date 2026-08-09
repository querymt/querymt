use super::{Fut, LLMProviderFactory, http::HTTPLLMProviderFactory};
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
        let adapter = LLMProviderFromHTTP::new(sync_provider);
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
}
