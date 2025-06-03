use anyhow::Result;
use rmcp::{service::RunningService, RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path, process::Stdio};
use which::which;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub mcp: Vec<McpServerConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(flatten)]
    pub transport: McpServerTransportConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "protocol", rename_all = "lowercase")]
pub enum McpServerTransportConfig {
    Sse {
        url: String,
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
    pub async fn start(&self) -> Result<RunningService<RoleClient, ()>> {
        let client = match self {
            McpServerTransportConfig::Sse { url } => {
                let transport = rmcp::transport::sse::SseTransport::start(url).await?;
                ().serve(transport).await?
            }
            McpServerTransportConfig::Stdio { command, .. }
                if !(which(&command).is_ok() || std::path::Path::new(&command).exists()) =>
            {
                anyhow::bail!("Command not found: {}", command);
            }
            McpServerTransportConfig::Stdio {
                command,
                args,
                envs,
            } => {
                let transport = rmcp::transport::child_process::TokioChildProcess::new(
                    tokio::process::Command::new(command)
                        .args(args)
                        .envs(envs)
                        .stderr(Stdio::inherit())
                        .stdout(Stdio::inherit()),
                )?;
                ().serve(transport).await?
            }
        };
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
    ) -> Result<HashMap<String, RunningService<RoleClient, ()>>> {
        let mut clients = HashMap::new();
        for server in &self.mcp {
            let client = server.transport.start().await?;
            clients.insert(server.name.clone(), client);
        }

        Ok(clients)
    }
}
