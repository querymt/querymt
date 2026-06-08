use super::*;

/// Only interpolates strings; leaves comments untouched (they're stripped during parsing)
pub(crate) fn interpolate_toml_value(value: &mut toml::Value) -> Result<()> {
    match value {
        toml::Value::String(s) => {
            *s = interpolate_env_vars(s)?;
        }
        toml::Value::Array(arr) => {
            for item in arr {
                interpolate_toml_value(item)?;
            }
        }
        toml::Value::Table(table) => {
            for (_key, val) in table {
                interpolate_toml_value(val)?;
            }
        }
        // Other types (Integer, Float, Boolean, Datetime) don't contain env vars
        _ => {}
    }
    Ok(())
}

/// Source for loading agent configuration.
#[derive(Debug, Clone)]
pub enum ConfigSource {
    /// Load TOML from a file path.
    Path(PathBuf),
    /// Load TOML directly from a string.
    Toml(String),
}

impl<T> From<T> for ConfigSource
where
    T: AsRef<Path>,
{
    fn from(value: T) -> Self {
        Self::Path(value.as_ref().to_path_buf())
    }
}

enum PromptResolution {
    ResolveFiles { base_path: PathBuf },
    RejectFileRefs,
}

pub(crate) fn ensure_inline_system_parts(parts: &[SystemPart], context: &str) -> Result<()> {
    if let Some(SystemPart::File { file }) = parts
        .iter()
        .find(|part| matches!(part, SystemPart::File { .. }))
    {
        return Err(anyhow!(
            "{context} contains unsupported file reference '{file:?}' in inline TOML config; inline prompt text directly instead"
        ));
    }

    // Validate template syntax and variable names for all inline strings.
    for part in parts {
        if let SystemPart::Inline(s) = part {
            crate::template::validate_template(s).map_err(|e| {
                anyhow!("Failed to validate {context} inline system prompt template: {e}")
            })?;
        }
    }

    Ok(())
}

/// Build typed config from a parsed TOML value.
async fn build_config_from_toml_value(
    mut value: toml::Value,
    resolution: PromptResolution,
) -> Result<Config> {
    // Top-level [profile] is catalog-only metadata. Strip it before strict runtime
    // deserialization so it cannot affect agent behavior; all other unknown fields stay rejected.
    if let Some(table) = value.as_table_mut() {
        table.remove("profile");
    }

    let config = if value.get("agent").is_some() {
        // Single agent config
        let mut config: SingleAgentConfig = value
            .try_into()
            .with_context(|| "Failed to deserialize single agent config")?;

        // Step 4: Validate
        validate_mcp_servers(&config.mcp)?;

        // Step 5: Resolve system prompt file references
        match &resolution {
            PromptResolution::ResolveFiles { base_path } => {
                let resolved =
                    resolve_system_parts(&config.agent.system, base_path, "agent").await?;
                config.agent.system = resolved.into_iter().map(SystemPart::Inline).collect();
            }
            PromptResolution::RejectFileRefs => {
                ensure_inline_system_parts(&config.agent.system, "agent.system")?;
            }
        }

        Config::Single(Box::new(config))
    } else if value.get("quorum").is_some() || value.get("planner").is_some() {
        // Multi-agent config
        let mut config: QuorumConfig = value
            .try_into()
            .with_context(|| "Failed to deserialize quorum config")?;

        // Step 4: Validate
        validate_mcp_servers(&config.mcp)?;
        for delegate in &config.delegates {
            validate_mcp_servers(&delegate.mcp)?;
        }
        validate_peer_delegates(&config.delegates, &config.mesh)?;

        // Step 5: Resolve system prompt file references
        match &resolution {
            PromptResolution::ResolveFiles { base_path } => {
                let resolved =
                    resolve_system_parts(&config.planner.system, base_path, "planner").await?;
                config.planner.system = resolved.into_iter().map(SystemPart::Inline).collect();
                for delegate in &mut config.delegates {
                    let context = format!("delegate '{}'", delegate.id);
                    let resolved =
                        resolve_system_parts(&delegate.system, base_path, &context).await?;
                    delegate.system = resolved.into_iter().map(SystemPart::Inline).collect();
                }
            }
            PromptResolution::RejectFileRefs => {
                ensure_inline_system_parts(&config.planner.system, "planner.system")?;
                for delegate in &config.delegates {
                    let context = format!("delegate '{}'.system", delegate.id);
                    ensure_inline_system_parts(&delegate.system, &context)?;
                }
            }
        }

        Config::Multi(Box::new(config))
    } else {
        return Err(anyhow!(
            "Invalid config file: must contain [agent] for single agent or [quorum]/[planner] for multi-agent"
        ));
    };

    Ok(config)
}

/// Load and parse config from either a file path or inline TOML content.
pub async fn load_config(source: impl Into<ConfigSource>) -> Result<Config> {
    match source.into() {
        ConfigSource::Path(path) => {
            let content = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("Failed to read config file: {:?}", path))?;

            // Step 1: Parse TOML to strip comments and get structured data
            let mut value: toml::Value = toml::from_str(&content)
                .with_context(|| format!("Failed to parse TOML config file: {:?}", path))?;

            // Step 2: Interpolate environment variables only in string values
            interpolate_toml_value(&mut value)?;

            // Step 3+: Detect config type, deserialize, validate, and resolve system prompt files
            let base_path = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            build_config_from_toml_value(value, PromptResolution::ResolveFiles { base_path })
                .await
                .with_context(|| format!("Failed to load config file: {:?}", path))
        }
        ConfigSource::Toml(content) => {
            // Step 1: Parse TOML to strip comments and get structured data
            let mut value: toml::Value =
                toml::from_str(&content).context("Failed to parse inline TOML config")?;

            // Step 2: Interpolate environment variables only in string values
            interpolate_toml_value(&mut value)?;

            // Step 3+: Detect config type, deserialize, validate; file prompt refs are rejected
            build_config_from_toml_value(value, PromptResolution::RejectFileRefs).await
        }
    }
}

/// Interpolate environment variables in config content
/// Supports ${VAR} and ${VAR:-default} syntax
pub fn interpolate_env_vars(content: &str) -> Result<String> {
    let re = Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)(?::-([^}]*))?\}")
        .context("Failed to compile env var regex")?;

    let mut errors = Vec::new();

    let result = re.replace_all(content, |caps: &Captures| {
        let var_name = &caps[1];
        let default = caps.get(2).map(|m| m.as_str());

        match (std::env::var(var_name), default) {
            (Ok(val), _) => val,
            (Err(_), Some(default)) => default.to_string(),
            (Err(_), None) => {
                errors.push(var_name.to_string());
                String::new() // Placeholder, will error below
            }
        }
    });

    if !errors.is_empty() {
        return Err(anyhow!(
            "Required environment variables not set: {}",
            errors.join(", ")
        ));
    }

    Ok(result.into_owned())
}

/// Validate that delegates with `peer` set require `[mesh] enabled = true`.
///
/// If any delegate specifies a `peer`, the mesh must be enabled — otherwise
/// the routing cannot function and the user has a misconfiguration.
pub(crate) fn validate_peer_delegates(
    delegates: &[DelegateConfig],
    mesh: &MeshTomlConfig,
) -> Result<()> {
    for delegate in delegates {
        if let Some(ref peer_name) = delegate.peer
            && !mesh.enabled
        {
            return Err(anyhow!(
                "delegate '{}' has `peer = \"{}\"` but `[mesh] enabled = false` (or mesh section absent). \
                     Set `[mesh] enabled = true` to enable mesh-routed LLM calls.",
                delegate.id,
                peer_name,
            ));
        }
    }
    Ok(())
}

/// Validate MCP servers have unique names
pub(crate) fn validate_mcp_servers(servers: &[McpServerConfig]) -> Result<()> {
    let mut seen = HashSet::new();
    for server in servers {
        let name = server.name();
        if !seen.insert(name) {
            return Err(anyhow!("Duplicate MCP server name: {}", name));
        }
    }
    Ok(())
}
