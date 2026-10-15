use super::*;

/// A single part of a system prompt, either an inline string or a file reference.
///
/// In TOML configs, the `system` field accepts a mixed array of strings and
/// `{ file = "path" }` objects, preserving order:
///
/// ```toml
/// system = [
///   "You are a helpful assistant.",
///   { file = "prompts/coder.md" },
///   "Additional instructions.",
/// ]
/// ```
///
/// For convenience, a plain string is also accepted:
///
/// ```toml
/// system = "You are a helpful assistant."
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum SystemPart {
    /// An inline system prompt string
    Inline(String),
    /// A file reference whose contents will be loaded as a system prompt part
    CachedFile {
        file: PathBuf,
        cache: querymt::chat::CacheHint,
    },
    File {
        file: PathBuf,
    },
    Text {
        text: String,
        #[serde(default)]
        cache: Option<querymt::chat::CacheHint>,
    },
}

impl SystemPart {
    pub fn resolved(&self) -> Option<querymt::params::SystemPrompt> {
        match self {
            Self::Inline(text) => Some(text.clone().into()),
            Self::Text { text, cache } => Some(querymt::params::SystemPrompt::Hinted {
                text: text.clone(),
                cache: cache.clone(),
            }),
            Self::File { .. } | Self::CachedFile { .. } => None,
        }
    }
}
impl From<querymt::params::SystemPrompt> for SystemPart {
    fn from(value: querymt::params::SystemPrompt) -> Self {
        match value {
            querymt::params::SystemPrompt::Text(text) => Self::Inline(text),
            querymt::params::SystemPrompt::Hinted { text, cache } => Self::Text { text, cache },
        }
    }
}

/// Deserializes the `system` field which can be:
/// - absent → empty vec
/// - a single string → `[Inline(s)]`
/// - an array of mixed strings and `{ file = "..." }` objects → `Vec<SystemPart>`
pub(crate) fn deserialize_system_parts<'de, D>(deserializer: D) -> Result<Vec<SystemPart>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SystemField {
        Single(String),
        Multiple(Vec<SystemPart>),
    }
    match Option::<SystemField>::deserialize(deserializer)? {
        None => Ok(Vec::new()),
        Some(SystemField::Single(s)) => Ok(vec![SystemPart::Inline(s)]),
        Some(SystemField::Multiple(v)) => Ok(v),
    }
}

/// Resolves a list of system parts into a flat list of strings by reading file contents.
pub(crate) async fn resolve_system_parts(
    parts: &[SystemPart],
    base_path: &Path,
    context: &str,
) -> Result<Vec<querymt::params::SystemPrompt>> {
    let mut resolved = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            SystemPart::Inline(s) => {
                crate::template::validate_template(s)
                    .map_err(|e| anyhow!("Failed to validate {context} prompt template: {e}"))?;
                resolved.push(s.clone().into());
            }
            SystemPart::Text { text, cache } => {
                crate::template::validate_template(text)
                    .map_err(|e| anyhow!("Failed to validate {context} prompt template: {e}"))?;
                resolved.push(querymt::params::SystemPrompt::Hinted {
                    text: text.clone(),
                    cache: cache.clone(),
                });
            }
            SystemPart::File { file } | SystemPart::CachedFile { file, .. } => {
                let path = base_path.join(file);
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("Failed to load {context} prompt from {path:?}"))?;
                let content = interpolate_env_vars(&content).with_context(|| {
                    format!("Failed to interpolate env vars in {context} prompt from {path:?}")
                })?;
                crate::template::validate_template(&content).map_err(|e| {
                    anyhow!("Failed to validate {context} prompt template from {path:?}: {e}")
                })?;
                resolved.push(match part {
                    SystemPart::CachedFile { cache, .. } => {
                        querymt::params::SystemPrompt::with_cache(content, cache.clone())
                    }
                    _ => content.into(),
                });
            }
        }
    }
    Ok(resolved)
}
