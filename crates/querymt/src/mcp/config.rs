use anyhow::Result;
use http::{header::AUTHORIZATION, HeaderValue};
use reqwest::header::HeaderMap;
use rmcp::{
    service::{DynService, RunningService},
    transport::{
        sse_client::SseClientConfig, streamable_http_client::StreamableHttpClientTransportConfig,
        SseClientTransport, StreamableHttpClientTransport,
    },
    RoleClient, ServiceExt,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};
use which::which;

use super::registry::{PackageType, RegistryClient};

/// Registry configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryConfig {
    /// Registry base URL
    #[serde(default = "default_registry_url")]
    pub url: String,

    /// Whether to use caching
    #[serde(default = "default_use_cache")]
    pub use_cache: bool,

    /// Cache TTL in hours (None = no expiration)
    pub cache_ttl_hours: Option<u64>,
}

fn default_registry_url() -> String {
    "https://registry.modelcontextprotocol.io".to_string()
}

fn default_use_cache() -> bool {
    true
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            url: default_registry_url(),
            use_cache: default_use_cache(),
            cache_ttl_hours: Some(24),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub mcp: Vec<McpServerConfig>,

    /// Global registry configuration
    #[serde(default)]
    pub registry: RegistryConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct McpServerConfig {
    pub name: String,

    #[serde(flatten)]
    pub source: McpServerSource,
}

/// Source of an MCP server configuration
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum McpServerSource {
    /// Direct transport configuration
    Direct {
        #[serde(flatten)]
        transport: McpServerTransportConfig,
    },

    /// Registry-sourced server
    Registry {
        /// Registry server ID (e.g., "@modelcontextprotocol/server-filesystem")
        registry_id: String,

        /// Version to use (e.g., "latest" or "0.5.1")
        #[serde(default = "default_version")]
        version: String,

        /// Override global registry configuration
        #[serde(skip_serializing_if = "Option::is_none")]
        registry_config: Option<RegistryConfig>,

        /// Environment variable overrides
        #[serde(skip_serializing_if = "Option::is_none")]
        env_overrides: Option<HashMap<String, String>>,
    },
}

fn default_version() -> String {
    "latest".to_string()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "protocol", rename_all = "lowercase")]
pub enum McpServerTransportConfig {
    Http {
        url: String,
        token: Option<String>,
    },
    Sse {
        url: String,
        token: Option<String>,
    },
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        envs: HashMap<String, String>,
    },
}

impl McpServerTransportConfig {
    pub async fn start(
        &self,
    ) -> Result<RunningService<RoleClient, Box<dyn DynService<RoleClient>>>> {
        let client = match self {
            McpServerTransportConfig::Sse { url, token } => {
                let transport = match token {
                    Some(t) => {
                        let mut default_headers = HeaderMap::new();
                        default_headers.insert(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {t}"))?,
                        );

                        let client = reqwest::ClientBuilder::new()
                            .default_headers(default_headers)
                            .build()?;

                        SseClientTransport::start_with_client(
                            client,
                            SseClientConfig {
                                sse_endpoint: url.clone().into(),
                                ..Default::default()
                            },
                        )
                        .await?
                    }
                    None => SseClientTransport::start(url.as_str()).await?,
                };
                ().into_dyn().serve(transport).await?
            }
            McpServerTransportConfig::Http { url, token } => {
                let transport = match token {
                    Some(t) => {
                        let mut default_headers = HeaderMap::new();
                        default_headers.insert(
                            AUTHORIZATION,
                            HeaderValue::from_str(&format!("Bearer {t}"))?,
                        );

                        let client = reqwest::ClientBuilder::new()
                            .default_headers(default_headers)
                            .build()?;

                        StreamableHttpClientTransport::with_client(
                            client,
                            StreamableHttpClientTransportConfig {
                                uri: url.clone().into(),
                                ..Default::default()
                            },
                        )
                    }
                    None => StreamableHttpClientTransport::from_uri(url.clone()),
                };
                ().into_dyn().serve(transport).await?
            }
            McpServerTransportConfig::Stdio { command, .. }
                if !(which(command).is_ok() || std::path::Path::new(&command).exists()) =>
            {
                anyhow::bail!("Command not found: {}", command);
            }
            McpServerTransportConfig::Stdio {
                command,
                args,
                envs,
            } => {
                let mut cmd = tokio::process::Command::new(command);

                cmd.args(args)
                    .envs(envs)
                    .stderr(Stdio::inherit())
                    .stdout(Stdio::inherit());
                let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd)?;
                ().into_dyn().serve(transport).await?
            }
        };
        log::trace!("Connected to server: {:#?}", client.peer_info());
        Ok(client)
    }
}

impl Config {
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = tokio::fs::read_to_string(path).await?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    pub async fn create_mcp_clients(
        &self,
    ) -> Result<HashMap<String, RunningService<RoleClient, Box<dyn DynService<RoleClient>>>>> {
        let mut clients = HashMap::new();
        for server in &self.mcp {
            // Resolve transport configuration based on source
            let transport = match &server.source {
                McpServerSource::Direct { transport } => transport.clone(),
                McpServerSource::Registry {
                    registry_id,
                    version,
                    registry_config,
                    env_overrides,
                } => {
                    // Use server-specific registry config or fall back to global
                    let reg_cfg = registry_config.as_ref().unwrap_or(&self.registry);
                    self.resolve_registry_server(
                        registry_id,
                        version,
                        reg_cfg,
                        env_overrides.as_ref(),
                    )
                    .await?
                }
            };

            let client = transport.start().await?;
            clients.insert(server.name.clone(), client);
        }

        Ok(clients)
    }

    /// Resolve a registry server reference into a transport configuration
    async fn resolve_registry_server(
        &self,
        registry_id: &str,
        version: &str,
        registry_cfg: &RegistryConfig,
        env_overrides: Option<&HashMap<String, String>>,
    ) -> Result<McpServerTransportConfig> {
        let client = RegistryClient::new(registry_cfg.url.clone());

        // Fetch server version from registry
        // Note: Caching is now handled at the CLI layer for better search capabilities
        let server_version = client.get_server_version(registry_id, version).await?;

        // Convert ServerResponse to McpServerTransportConfig
        Self::server_response_to_transport(server_version, env_overrides)
    }

    /// Convert a registry ServerResponse to a transport configuration
    fn server_response_to_transport(
        server_response: super::registry::ServerResponse,
        env_overrides: Option<&HashMap<String, String>>,
    ) -> Result<McpServerTransportConfig> {
        // Get the first package from the server (typically there's only one)
        let package = server_response
            .server
            .packages
            .first()
            .ok_or_else(|| anyhow::anyhow!("Server has no packages defined"))?;

        // Currently, we primarily support stdio-based servers from npm/pypi packages
        match package.registry_type {
            PackageType::Npm => {
                // For npm packages, we typically use npx to run them
                let command = "npx".to_string();
                let mut args = vec!["-y".to_string(), package.identifier.clone()];

                // Add any additional args from the package
                if let Some(extra_args) = &package.args {
                    args.extend(extra_args.clone());
                }

                // Merge environment variables
                let mut envs = HashMap::new();
                if let Some(env_vars) = &package.environment_variables {
                    for env_var in env_vars {
                        // Only set if not already provided in overrides
                        if let Some(overrides) = env_overrides {
                            if !overrides.contains_key(&env_var.name) {
                                // Leave unset - user must provide
                                envs.insert(env_var.name.clone(), String::new());
                            }
                        }
                    }
                }
                if let Some(overrides) = env_overrides {
                    envs.extend(overrides.clone());
                }

                Ok(McpServerTransportConfig::Stdio {
                    command,
                    args,
                    envs,
                })
            }
            PackageType::Pypi => {
                // For Python packages, use pip run or python -m
                let command = "python".to_string();
                let mut args = vec!["-m".to_string(), package.identifier.clone()];

                // Add any additional args from the package
                if let Some(extra_args) = &package.args {
                    args.extend(extra_args.clone());
                }

                // Merge environment variables
                let mut envs = HashMap::new();
                if let Some(env_vars) = &package.environment_variables {
                    for env_var in env_vars {
                        if let Some(overrides) = env_overrides {
                            if !overrides.contains_key(&env_var.name) {
                                envs.insert(env_var.name.clone(), String::new());
                            }
                        }
                    }
                }
                if let Some(overrides) = env_overrides {
                    envs.extend(overrides.clone());
                }

                Ok(McpServerTransportConfig::Stdio {
                    command,
                    args,
                    envs,
                })
            }
            PackageType::Binary => {
                // For binaries, use the identifier as the command
                let command = package.identifier.clone();
                let args = package.args.clone().unwrap_or_default();

                let mut envs = HashMap::new();
                if let Some(env_vars) = &package.environment_variables {
                    for env_var in env_vars {
                        if let Some(overrides) = env_overrides {
                            if !overrides.contains_key(&env_var.name) {
                                envs.insert(env_var.name.clone(), String::new());
                            }
                        }
                    }
                }
                if let Some(overrides) = env_overrides {
                    envs.extend(overrides.clone());
                }

                Ok(McpServerTransportConfig::Stdio {
                    command,
                    args,
                    envs,
                })
            }
            PackageType::Docker | PackageType::Other => {
                anyhow::bail!(
                    "Unsupported package type: {:?}. Only npm, pypi, and binary are supported.",
                    package.registry_type
                )
            }
        }
    }
}
