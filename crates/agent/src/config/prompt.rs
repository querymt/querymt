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
#[serde(untagged)]
pub enum SystemPart {
    /// An inline system prompt string
    Inline(String),
    /// A file reference whose contents will be loaded as a system prompt part
    File { file: PathBuf },
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
) -> Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            SystemPart::Inline(s) => {
                crate::template::validate_template(s)
                    .map_err(|e| anyhow!("Failed to validate {context} prompt template: {e}"))?;
                resolved.push(s.clone());
            }
            SystemPart::File { file } => {
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
                resolved.push(content);
            }
        }
    }
    Ok(resolved)
}
