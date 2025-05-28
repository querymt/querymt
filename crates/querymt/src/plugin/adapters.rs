use super::{http::HTTPLLMProviderFactory, Fut, LLMProviderFactory};
use crate::{
    adapters::LLMProviderFromHTTP, error::LLMError, outbound::call_outbound, HTTPLLMProvider,
    LLMProvider,
};
use futures::future::FutureExt;
use http::{Request, Response};
use serde_json::Value;
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

    fn config_schema(&self) -> Value {
        self.inner.config_schema()
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn LLMProvider>, LLMError> {
        let sync_provider = self
            .inner
            .from_config(cfg)
            .map_err(|e| LLMError::PluginError(e.to_string()))?;

        let arc_provider: Arc<dyn HTTPLLMProvider> = Arc::from(sync_provider);
        let adapter = LLMProviderFromHTTP::new(arc_provider);
        Ok(Box::new(adapter))
    }

    fn list_models<'a>(&'a self, cfg: &Value) -> Fut<'a, Result<Vec<String>, LLMError>> {
        // clone the Arc so we can move it into the async block
        let inner = Arc::clone(&self.inner);
        let cloned_cfg = cfg.clone();

        async move {
            let req: Request<Vec<u8>> = inner.list_models_request(&cloned_cfg)?;

            let resp: Response<Vec<u8>> = call_outbound(req)
                .await
                .map_err(|e| LLMError::HttpError(e.to_string()))?;

            inner
                .parse_list_models(resp)
                .map_err(|e| LLMError::PluginError(e.to_string()))
        }
        .boxed()
    }
}
