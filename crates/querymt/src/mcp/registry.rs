use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// Custom deserializer for Repository that treats invalid/empty objects as None
fn deserialize_optional_repository<'de, D>(deserializer: D) -> Result<Option<Repository>, D::Error>
where
    D: Deserializer<'de>,
{
    // Try to deserialize as a Repository, but if it fails, return None
    #[derive(Deserialize)]
    struct Helper {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        source: Option<String>,
    }

    // Deserialize as Option to handle missing field case
    let helper = Option::<Helper>::deserialize(deserializer)?;

    match helper {
        Some(h) => {
            // Only create Repository if both required fields are present
            match (h.url, h.source) {
                (Some(url), Some(source)) => Ok(Some(Repository { url, source })),
                _ => Ok(None),
            }
        }
        None => Ok(None),
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("Invalid response format: {0}")]
    InvalidResponse(String),

    #[error("Server not found: {0}")]
    ServerNotFound(String),

    #[error("Version not found: {0}")]
    VersionNotFound(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
}

/// Client for interacting with MCP registries following the official API specification
#[derive(Debug, Clone)]
pub struct RegistryClient {
    base_url: String,
    client: reqwest::Client,
}

impl RegistryClient {
    /// Create a new registry client with a custom base URL
    ///
    /// # Arguments
    /// * `base_url` - The base URL of the registry (e.g., "https://registry.modelcontextprotocol.io")
    pub fn new(base_url: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Create a client pointing to the official MCP registry
    pub fn official() -> Self {
        Self::new("https://registry.modelcontextprotocol.io".to_string())
    }

    /// List all servers from the registry with pagination support
    ///
    /// # Arguments
    /// * `limit` - Maximum number of results to return (optional)
    /// * `cursor` - Pagination cursor from a previous response (optional)
    /// * `search` - Substring matching on server names (optional)
    /// * `updated_since` - RFC3339 format filtering (optional)
    /// * `version` - Filter by specific or latest version (optional)
    pub async fn list_servers(
        &self,
        limit: Option<u32>,
        cursor: Option<String>,
        search: Option<String>,
        updated_since: Option<String>,
        version: Option<String>,
    ) -> Result<ServerList, RegistryError> {
        let mut url = format!("{}/v0/servers", self.base_url);
        let mut params = vec![];

        if let Some(limit) = limit {
            params.push(format!("limit={}", limit));
        }
        if let Some(cursor) = cursor {
            params.push(format!("cursor={}", urlencoding::encode(&cursor)));
        }
        if let Some(search) = search {
            params.push(format!("search={}", urlencoding::encode(&search)));
        }
        if let Some(updated_since) = updated_since {
            params.push(format!("updated_since={}", urlencoding::encode(&updated_since)));
        }
        if let Some(version) = version {
            params.push(format!("version={}", urlencoding::encode(&version)));
        }

        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(RegistryError::InvalidResponse(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        let servers_response: ServerList = response.json().await?;
        Ok(servers_response)
    }

    /// Get all versions of a specific server
    ///
    /// # Arguments
    /// * `server_name` - The server identifier (e.g., "@modelcontextprotocol/server-filesystem")
    pub async fn get_server_versions(
        &self,
        server_name: &str,
    ) -> Result<ServerList, RegistryError> {
        let encoded_name = urlencoding::encode(server_name);
        let url = format!("{}/v0/servers/{}/versions", self.base_url, encoded_name);

        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(RegistryError::ServerNotFound(server_name.to_string()));
        }

        if !response.status().is_success() {
            return Err(RegistryError::InvalidResponse(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        let versions: ServerList = response.json().await?;
        Ok(versions)
    }

    /// Get a specific version of a server (or use "latest")
    ///
    /// # Arguments
    /// * `server_name` - The server identifier
    /// * `version` - The version string (e.g., "0.5.1" or "latest")
    pub async fn get_server_version(
        &self,
        server_name: &str,
        version: &str,
    ) -> Result<ServerResponse, RegistryError> {
        let encoded_name = urlencoding::encode(server_name);
        let encoded_version = urlencoding::encode(version);
        let url = format!(
            "{}/v0/servers/{}/versions/{}",
            self.base_url, encoded_name, encoded_version
        );

        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(RegistryError::VersionNotFound(format!(
                "{}@{}",
                server_name, version
            )));
        }

        if !response.status().is_success() {
            return Err(RegistryError::InvalidResponse(format!(
                "HTTP {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )));
        }

        let server_response: ServerResponse = response.json().await?;
        Ok(server_response)
    }
}

/// Response from the list servers endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerList {
    pub servers: Vec<ServerResponse>,
    pub metadata: Metadata,
}

/// Pagination metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub count: u32,
    #[serde(rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

/// Server response wrapper containing server details and registry metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerResponse {
    pub server: ServerDetail,
    #[serde(rename = "_meta")]
    pub meta: RegistryMetadata,
}

/// Server detail information (reverse-DNS format name required)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerDetail {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub name: String,
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    pub icons: Option<Vec<Icon>>,
    #[serde(default, deserialize_with = "deserialize_optional_repository", skip_serializing_if = "Option::is_none")]
    pub repository: Option<Repository>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<Package>,
}

/// Repository information with required url and source fields
#[derive(Debug, Clone, Serialize)]
pub struct Repository {
    pub url: String,
    pub source: String,
}

/// Icon metadata
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct Icon {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub sizes: Option<Vec<String>>,
    pub src: String,
    pub theme: Option<String>,
}

/// Package information with registry type, identifier, transport, and optional verification
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct Package {
    #[serde(rename = "registryType")]
    pub registry_type: PackageType,
    pub identifier: String,
    pub transport: Transport,
    #[serde(rename = "environmentVariables", skip_serializing_if = "Option::is_none")]
    pub environment_variables: Option<Vec<EnvironmentVariable>>,
    /// Optional SHA-256 hash for package verification
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Runtime hints for execution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Argument configurations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
}

/// Transport configuration
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct Transport {
    #[serde(rename = "type")]
    pub transport_type: String,
    #[serde(flatten)]
    pub config: Option<serde_json::Value>,
}

/// Input value configuration (Input schema)
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct InputValue {
    /// Description of the input
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Whether the input is required
    #[serde(rename = "isRequired", default)]
    pub is_required: bool,

    /// Input format specification
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    /// The input value
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,

    /// Whether the input is sensitive/secret
    #[serde(rename = "isSecret", default)]
    pub is_secret: bool,

    /// Default value for the input
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    /// Placeholder guidance text
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,

    /// List of possible values to select from
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
}

/// Environment variable configuration following KeyValueInput schema
/// Extends InputWithVariables which extends Input
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct EnvironmentVariable {
    /// Name of the environment variable (required)
    pub name: String,

    /// Description of the input (from Input schema)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Whether the input is required (from Input schema)
    #[serde(rename = "isRequired", default)]
    pub is_required: bool,

    /// Input format specification (from Input schema)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    /// The input value with variable substitution (from Input schema)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,

    /// Whether the input is sensitive/secret (from Input schema)
    #[serde(rename = "isSecret", default)]
    pub is_secret: bool,

    /// Default value for the input (from Input schema)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    /// Placeholder guidance text (from Input schema)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,

    /// List of possible values to select from (from Input schema)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,

    /// Variable substitution map with Input objects as values (from InputWithVariables schema)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables: Option<HashMap<String, InputValue>>,
}

/// Registry-managed metadata
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct RegistryMetadata {
    #[serde(rename = "io.modelcontextprotocol.registry/official")]
    pub official: Option<OfficialMetadata>,
}

/// Official registry metadata with timestamps and status
#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct OfficialMetadata {
    pub status: String,
    #[serde(rename = "publishedAt")]
    pub published_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "isLatest")]
    pub is_latest: bool,
}

/// Type of package distribution
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PackageType {
    Npm,
    Pypi,
    Docker,
    Binary,
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_servers_official() {
        let client = RegistryClient::official();
        let result = client.list_servers(Some(5), None, None, None, None).await;

        // This test requires network access to the official registry
        match result {
            Ok(response) => {
                assert!(response.servers.len() <= 5);
                println!("Found {} servers", response.servers.len());
                for server in response.servers.iter().take(3) {
                    println!("  - {}: {}", server.server.name, server.server.description);
                }
            }
            Err(e) => {
                println!("Warning: Could not connect to registry: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_get_server_version_latest() {
        let client = RegistryClient::official();
        let result = client
            .get_server_version("@modelcontextprotocol/server-filesystem", "latest")
            .await;

        match result {
            Ok(response) => {
                println!("Latest version: {}", response.server.version);
                println!("Description: {}", response.server.description);
                if let Some(packages) = response.server.packages.first() {
                    println!("Package type: {:?}", packages.registry_type);
                }
            }
            Err(e) => {
                println!("Warning: Could not fetch version: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_get_server_versions() {
        let client = RegistryClient::official();
        let result = client
            .get_server_versions("@modelcontextprotocol/server-filesystem")
            .await;

        match result {
            Ok(response) => {
                println!("Found {} versions", response.servers.len());
                for server in response.servers.iter().take(3) {
                    println!("  - v{}", server.server.version);
                }
            }
            Err(e) => {
                println!("Warning: Could not fetch versions: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_search_servers() {
        let client = RegistryClient::official();
        let result = client
            .list_servers(Some(10), None, Some("filesystem".to_string()), None, None)
            .await;

        match result {
            Ok(response) => {
                println!("Found {} servers matching 'filesystem'", response.servers.len());
                for server in response.servers.iter() {
                    println!("  - {}", server.server.name);
                }
            }
            Err(e) => {
                println!("Warning: Could not search servers: {}", e);
            }
        }
    }
}
