use crate::error::LLMError;
use crate::plugin::{LLMProviderFactory, host::PluginRegistry};
use serde_json::{Map, Value};
use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOverrideMode {
    DefaultsOnly,
    Overwrite,
}

#[derive(Debug, Clone)]
pub struct ResolvedProviderConfig {
    pub full_config: Value,
    pub pruned_config: Value,
    pub pruned_config_str: String,
    pub pruned_keys: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderConfigBuilder {
    config: Value,
}

impl ProviderConfigBuilder {
    pub fn empty() -> Self {
        Self {
            config: Value::Object(Map::new()),
        }
    }

    pub fn from_registry_provider(
        registry: &PluginRegistry,
        provider_name: &str,
    ) -> Result<Self, LLMError> {
        Ok(Self {
            config: provider_static_config_json(registry, provider_name)?,
        })
    }

    pub fn merge_value(&mut self, value: &Value, mode: ConfigOverrideMode) {
        let Some(src) = value.as_object() else {
            return;
        };
        let dst = self.ensure_object();
        for (key, value) in src {
            match mode {
                ConfigOverrideMode::DefaultsOnly => {
                    if dst.get(key).is_none_or(Value::is_null) {
                        dst.insert(key.clone(), value.clone());
                    }
                }
                ConfigOverrideMode::Overwrite => {
                    dst.insert(key.clone(), value.clone());
                }
            }
        }
    }

    pub fn set<K: Into<String>>(&mut self, key: K, value: Value) {
        self.ensure_object().insert(key.into(), value);
    }

    pub fn set_if_missing<K: Into<String>>(&mut self, key: K, value: Value) {
        let key = key.into();
        if self.config.get(&key).is_none_or(Value::is_null) {
            self.ensure_object().insert(key, value);
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(Value::as_str)
    }

    pub fn contains_non_empty_str(&self, key: &str) -> bool {
        self.get_str(key).is_some_and(|s| !s.is_empty())
    }

    pub fn value(&self) -> &Value {
        &self.config
    }

    pub fn value_mut(&mut self) -> &mut Value {
        &mut self.config
    }

    pub fn into_value(self) -> Value {
        self.config
    }

    pub fn prune_for_factory(
        self,
        factory: &dyn LLMProviderFactory,
    ) -> Result<ResolvedProviderConfig, LLMError> {
        let schema: Value = serde_json::from_str(&factory.config_schema())?;
        let pruned_config = prune_config_by_schema(&self.config, &schema);
        let pruned_keys = pruned_top_level_keys(&self.config, &pruned_config);
        let pruned_config_str = serde_json::to_string(&pruned_config)?;
        Ok(ResolvedProviderConfig {
            full_config: self.config,
            pruned_config,
            pruned_config_str,
            pruned_keys,
        })
    }

    fn ensure_object(&mut self) -> &mut Map<String, Value> {
        if !self.config.is_object() {
            self.config = Value::Object(Map::new());
        }
        self.config.as_object_mut().expect("config must be object")
    }
}

pub fn provider_static_config_json(
    registry: &PluginRegistry,
    provider_name: &str,
) -> Result<Value, LLMError> {
    Ok(registry
        .config
        .providers
        .iter()
        .find(|p| p.name == provider_name)
        .and_then(|p| p.config.as_ref())
        .map(serde_json::to_value)
        .transpose()?
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new())))
}

pub fn resolve_registry_provider_config(
    registry: &PluginRegistry,
    provider_name: &str,
    factory: &dyn LLMProviderFactory,
) -> Result<ResolvedProviderConfig, LLMError> {
    let mut builder = ProviderConfigBuilder::from_registry_provider(registry, provider_name)?;

    if !builder.contains_non_empty_str("api_key")
        && let Some(http_factory) = factory.as_http()
        && let Some(env_var_name) = http_factory.api_key_name()
        && let Ok(api_key) = env::var(&env_var_name)
        && !api_key.trim().is_empty()
    {
        builder.set_if_missing("api_key", Value::String(api_key));
    }

    builder.prune_for_factory(factory)
}

pub fn prune_config_by_schema(cfg: &Value, schema: &Value) -> Value {
    match (cfg, schema.get("properties")) {
        (Value::Object(cfg_map), Some(Value::Object(props))) => {
            let mut out = Map::with_capacity(cfg_map.len());
            for (k, v) in cfg_map {
                if let Some(prop_schema) = props.get(k) {
                    let pruned_val = if prop_schema.get("properties").is_some() {
                        prune_config_by_schema(v, prop_schema)
                    } else {
                        v.clone()
                    };
                    out.insert(k.clone(), pruned_val);
                }
            }
            Value::Object(out)
        }
        _ => cfg.clone(),
    }
}

pub fn pruned_top_level_keys(before: &Value, after: &Value) -> Vec<String> {
    let Some(before_obj) = before.as_object() else {
        return Vec::new();
    };
    let Some(after_obj) = after.as_object() else {
        return Vec::new();
    };

    let mut removed: Vec<String> = before_obj
        .keys()
        .filter(|k| !after_obj.contains_key(*k))
        .cloned()
        .collect();
    removed.sort();
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::host::PluginRegistry;
    use crate::plugin::{HTTPLLMProviderFactory, LLMProviderFactory};
    use std::sync::Arc;

    struct DummyFactory {
        schema: &'static str,
    }

    impl HTTPLLMProviderFactory for DummyFactory {
        fn name(&self) -> &str {
            "dummy"
        }

        fn config_schema(&self) -> String {
            self.schema.to_string()
        }

        fn list_models_request(&self, _cfg: &str) -> Result<http::Request<Vec<u8>>, LLMError> {
            unimplemented!()
        }

        fn parse_list_models(
            &self,
            _resp: http::Response<Vec<u8>>,
        ) -> Result<Vec<String>, LLMError> {
            unimplemented!()
        }

        fn from_config(&self, _cfg: &str) -> Result<Box<dyn crate::HTTPLLMProvider>, LLMError> {
            unimplemented!()
        }
    }

    fn adapted_factory(schema: &'static str) -> Arc<dyn LLMProviderFactory> {
        Arc::new(crate::plugin::adapters::HTTPFactoryAdapter::new(Arc::new(
            DummyFactory { schema },
        )))
    }

    fn registry_with_provider(config_body: &str) -> PluginRegistry {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("providers.toml");
        std::fs::write(&config_path, config_body).expect("write config");
        let registry = PluginRegistry::from_path(&config_path).expect("registry");
        std::mem::forget(temp);
        registry
    }

    #[test]
    fn provider_static_config_json_serializes_provider_section() {
        let registry = registry_with_provider(
            "[[providers]]\nname = \"xiaomi\"\npath = \"dummy\"\n\n[providers.config]\nbase_url = \"https://example.invalid/v1\"\napi_key = \"bogus\"\n",
        );

        let cfg = provider_static_config_json(&registry, "xiaomi").expect("config");
        assert_eq!(cfg["base_url"], "https://example.invalid/v1");
        assert_eq!(cfg["api_key"], "bogus");
    }

    #[test]
    fn merge_value_honors_defaults_only_and_overwrite_modes() {
        let mut builder = ProviderConfigBuilder::empty();
        builder.set("base_url", Value::String("https://one.invalid".into()));
        builder.merge_value(
            &serde_json::json!({"base_url": "https://two.invalid", "model": "x"}),
            ConfigOverrideMode::DefaultsOnly,
        );
        assert_eq!(builder.value()["base_url"], "https://one.invalid");
        assert_eq!(builder.value()["model"], "x");

        builder.merge_value(
            &serde_json::json!({"base_url": "https://two.invalid"}),
            ConfigOverrideMode::Overwrite,
        );
        assert_eq!(builder.value()["base_url"], "https://two.invalid");
    }

    #[test]
    fn prune_for_factory_reports_pruned_keys() {
        let mut builder = ProviderConfigBuilder::empty();
        builder.set("model", Value::String("gpt-4o-mini".into()));
        builder.set("base_url", Value::String("https://example.invalid".into()));
        builder.set("extra", serde_json::json!(1));
        let factory = adapted_factory(r#"{"properties":{"model":{},"base_url":{}}}"#);

        let resolved = builder
            .prune_for_factory(factory.as_ref())
            .expect("resolved");
        assert_eq!(
            resolved.pruned_config,
            serde_json::json!({
                "model": "gpt-4o-mini",
                "base_url": "https://example.invalid"
            })
        );
        assert_eq!(resolved.pruned_keys, vec!["extra".to_string()]);
    }
}
