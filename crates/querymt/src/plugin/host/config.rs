use anyhow::{Context, Result};
use serde::Deserialize;
use std::{collections::HashMap, fs, path::Path};

use super::oci::OciDownloaderConfig;

#[derive(Debug, Deserialize)]
pub struct PluginConfig {
    pub providers: Vec<ProviderConfig>,
    pub oci: Option<OciDownloaderConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub path: String,
    pub config: Option<HashMap<String, toml::Value>>,
}

impl PluginConfig {
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p: &Path = path.as_ref();
        if !p.exists() {
            return Err(anyhow::anyhow!(
                "Config file not found at: {}. Please create a config file first.",
                p.display()
            ));
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");

        let content = fs::read_to_string(p)
            .with_context(|| format!("Failed to read config file at {}", p.display()))?;

        let config = match ext {
            "json" => serde_json::from_str(&content)?,
            "yaml" | "yml" => serde_yml::from_str(&content)?,
            "toml" => toml::from_str(&content)?,
            _ => return Err(anyhow::anyhow!("Unsupported config format: {}", ext)),
        };

        Ok(config)
    }
}
