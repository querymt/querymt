use anyhow::Result;
use rmcp::{
    RoleClient, ServiceExt,
    model::{ClientCapabilities, ClientInfo, Implementation},
    service::{DynService, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
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
    Http {
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
        client_impl: &Implementation,
    ) -> Result<RunningService<RoleClient, Box<dyn DynService<RoleClient>>>> {
        let client_info = ClientInfo::new(ClientCapabilities::default(), client_impl.clone());
        let client = match self {
            McpServerTransportConfig::Http { url, token } => {
                let config = match token {
                    Some(t) => StreamableHttpClientTransportConfig::with_uri(url.clone())
                        .auth_header(format!("Bearer {t}")),
                    None => StreamableHttpClientTransportConfig::with_uri(url.clone()),
                };
                let transport = StreamableHttpClientTransport::from_config(config);
                client_info.clone().into_dyn().serve(transport).await?
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
                    .stdout(Stdio::piped())
                    .stdin(Stdio::piped());
                let transport = rmcp::transport::child_process::TokioChildProcess::new(cmd)?;
                client_info.clone().into_dyn().serve(transport).await?
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
        client_impl: &Implementation,
    ) -> Result<HashMap<String, RunningService<RoleClient, Box<dyn DynService<RoleClient>>>>> {
        let mut clients = HashMap::new();
        for server in &self.mcp {
            let client = server.transport.start(client_impl).await?;
            clients.insert(server.name.clone(), client);
        }

        Ok(clients)
    }
}
