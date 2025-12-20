use crate::{
    chat::{BasicChatProvider, ChatMessage, ChatResponse, Tool, ToolChatProvider},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    plugin::{
        extism_impl::{ExtismChatRequest, ExtismChatResponse, ExtismEmbedRequest},
        Fut, HTTPLLMProviderFactory, LLMProviderFactory,
    },
    providers::{read_providers_from_cache, ProviderInfo},
    LLMProvider,
};

use async_trait::async_trait;
use extism::{convert::Json, Manifest, Plugin, PluginBuilder, Wasm};
use futures::FutureExt;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tracing::instrument;
use url::Url;

#[cfg(feature = "http-client")]
use extism::Function;

mod loader;
pub use loader::ExtismLoader;

mod functions;

use super::ExtismCompleteRequest;

#[derive(Clone)]
pub struct ExtismFactory {
    plugin: Arc<Mutex<Plugin>>,
    name: String,
}

fn call_plugin_str(plugin: Arc<Mutex<Plugin>>, func: &str, arg: &Value) -> anyhow::Result<String> {
    let input = serde_json::to_string(arg)?;
    let input_bytes = input.into_bytes();

    let mut plug = plugin.lock().unwrap();
    let output_bytes: Vec<u8> = plug.call(func, &input_bytes)?;
    Ok(std::str::from_utf8(&output_bytes)?.to_string())
}

fn call_plugin_json(plugin: Arc<Mutex<Plugin>>, func: &str, arg: &Value) -> anyhow::Result<Value> {
    let r = call_plugin_str(plugin, func, arg)?;
    Ok(serde_json::from_str(&r)?)
}

impl ExtismFactory {
    #[instrument(name = "extism_factory.load", skip_all)]
    pub fn load(
        wasm_content: Vec<u8>,
        config: &Option<HashMap<String, toml::Value>>,
    ) -> Result<Self, LLMError> {
        let mut env_map: HashMap<_, _> = std::env::vars().collect();

        let v = read_providers_from_cache()?;
        // NOTE: adding pricing data into current plugin as JSON string which should be deserialized
        // from another side
        env_map.insert("PROVIDERS_REGISTRY_DATA".to_string(), serde_json::to_string(&v)?);

        let initial_manifest =
            Manifest::new([Wasm::data(wasm_content.clone())]).with_config(env_map.iter());

        #[cfg(feature = "http-client")]
        let init_builder = {
            PluginBuilder::new(initial_manifest)
                .with_wasi(true)
                .with_function_in_namespace(
                    "extism:host/user",
                    "qmt_http_request",
                    [extism::PTR],
                    [extism::PTR],
                    extism::UserData::new(Vec::<String>::new()),
                    functions::reqwest_http
                )
        };
        #[cfg(not(feature = "http-client"))]
        let init_builder = PluginBuilder::new(initial_manifest).with_wasi(true);

        let init_plugin = Arc::new(Mutex::new(
            init_builder
                .build()
                .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?,
        ));

        let mut allowed_hosts: Vec<String> = Vec::new();
        if let Some(runtime_cfg) = config {
            if let Some(hosts) = runtime_cfg.get("allowed_hosts") {
                allowed_hosts.append(
                    &mut hosts
                        .clone()
                        .try_into()
                        .map_err(|e| LLMError::GenericError(format!("{:#}", e)))?,
                );
            }
        }

        let name = call_plugin_str(init_plugin.clone(), "name", &Value::Null)
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;
        if let Some(base_url) = call_plugin_str(init_plugin.clone(), "base_url", &Value::Null)
            .ok()
            .and_then(|v| Url::parse(&v).ok())
            .and_then(|url| url.host_str().map(|s| s.to_string()))
        {
            allowed_hosts.push(base_url);
        }
        drop(init_plugin);
        //        drop(init_plugin)

        let manifest = Manifest::new([Wasm::data(wasm_content.clone())])
            .with_allowed_hosts(allowed_hosts.clone().into_iter())
            .with_config(env_map.into_iter());

        #[cfg(feature = "http-client")]
        let builder = {
            // Register custom HTTP host function that uses reqwest
            PluginBuilder::new(manifest)
                .with_wasi(true)
                .with_function_in_namespace(
                    "extism:host/user",
                    "qmt_http_request",
                    [extism::PTR],
                    [extism::PTR],
                    extism::UserData::new(allowed_hosts), // Pass allowed_hosts as UserData
                    functions::reqwest_http
                )
        };
        
        #[cfg(not(feature = "http-client"))]
        let builder = PluginBuilder::new(manifest).with_wasi(true);

        let plugin = Arc::new(Mutex::new(
            builder.build()
                .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?,
        ));

        Ok(Self { plugin, name })
    }

    fn call(&self, func: &str, arg: &Value) -> anyhow::Result<String> {
        call_plugin_str(self.plugin.clone(), func, arg)
    }

    fn call_json(&self, func: &str, arg: &Value) -> anyhow::Result<Value> {
        call_plugin_json(self.plugin.clone(), func, arg)
    }
}

impl LLMProviderFactory for ExtismFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn config_schema(&self) -> Value {
        call_plugin_json(self.plugin.clone(), "config_schema", &Value::Null)
            .expect("config_schema() must return valid JSON")
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn LLMProvider>, LLMError> {
        let _from_cfg = self
            .call("from_config", cfg)
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;

        let provider = ExtismProvider {
            plugin: self.plugin.clone(),
            config: cfg.clone(),
        };
        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, cfg: &Value) -> Fut<'a, Result<Vec<String>, LLMError>> {
        let cfg_to = cfg.clone();
        async move {
            let v = self
                .call_json("list_models", &cfg_to)
                .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;
            let arr = v.as_array().ok_or(LLMError::ProviderError(
                "Model list is not an array".to_string(),
            ))?;
            Ok(arr
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect())
        }
        .boxed()
    }

    fn as_http(&self) -> Option<&dyn crate::plugin::http::HTTPLLMProviderFactory> {
        Some(self)
    }
}

impl HTTPLLMProviderFactory for ExtismFactory {
    fn name(&self) -> &str {
        (self as &dyn LLMProviderFactory).name()
    }

    fn config_schema(&self) -> Value {
        (self as &dyn LLMProviderFactory).config_schema()
    }

    fn from_config(&self, _cfg: &Value) -> Result<Box<dyn crate::HTTPLLMProvider>, LLMError> {
        Err(LLMError::PluginError(
            "ExtismProvider should not be used as HTTPLLMProviderFactory".into(),
        ))
    }

    fn list_models_request(&self, _cfg: &Value) -> Result<http::Request<Vec<u8>>, LLMError> {
        Err(LLMError::PluginError(
            "ExtismProvider should not be used as HTTPLLMProviderFactory".into(),
        ))
    }

    fn parse_list_models(&self, _resp: http::Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        Err(LLMError::PluginError(
            "ExtismProvider should not be used as HTTPLLMProviderFactory".into(),
        ))
    }

    fn api_key_name(&self) -> Option<String> {
        self.call("api_key_name", &Value::Null) // → Result<&'static str, _>
            .ok() // → Option<&'static str>
            .filter(|s| !s.is_empty())
    }
}

/// A single LLMProvider that delegates to the plugin's LLMProvider trait functions
pub struct ExtismProvider {
    plugin: Arc<Mutex<Plugin>>,
    config: Value,
}

#[async_trait]
impl BasicChatProvider for ExtismProvider {
    #[instrument(name = "extism_provider.chat", skip_all)]
    async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        let arg = ExtismChatRequest {
            cfg: self.config.clone(),
            messages: messages.to_vec(),
            tools: None,
        };
        let mut plug = self.plugin.lock().unwrap();
        let out: Json<ExtismChatResponse> = plug
            .call("chat", Json(arg))
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;

        Ok(Box::new(out.0) as Box<dyn ChatResponse>)
    }
}

#[async_trait]
impl ToolChatProvider for ExtismProvider {
    #[instrument(name = "extism_provider.chat_with_tools", skip_all)]
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let arg = ExtismChatRequest {
            cfg: self.config.clone(),
            messages: messages.to_vec(),
            tools: tools.map(|v| v.to_vec()),
        };

        let mut plug = self.plugin.lock().unwrap();
        let out: Json<ExtismChatResponse> = plug
            .call("chat", Json(arg))
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;

        Ok(Box::new(out.0) as Box<dyn ChatResponse>)
    }
}

#[async_trait]
impl EmbeddingProvider for ExtismProvider {
    #[instrument(name = "extism_provider.embed", skip_all)]
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        let arg = ExtismEmbedRequest {
            cfg: self.config.clone(),
            inputs: input,
        };

        let mut plug = self.plugin.lock().unwrap();
        let out: Result<Json<Vec<Vec<f32>>>, LLMError> = plug
            .call("embed", Json(arg))
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)));
        out.map(|v| v.0)
    }
}

#[async_trait]
impl CompletionProvider for ExtismProvider {
    #[instrument(name = "extism_provider.complete", skip_all)]
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        let arg = ExtismCompleteRequest {
            cfg: self.config.clone(),
            req: req.clone(),
        };

        let mut plug = self.plugin.lock().unwrap();
        let out: Result<Json<CompletionResponse>, LLMError> = plug
            .call("complete", Json(arg))
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)));
        out.map(|v| v.0)
    }
}

impl LLMProvider for ExtismProvider {}
