use hf_hub::api::sync::ApiBuilder as SyncApiBuilder;
use hf_hub::api::tokio::ApiBuilder as AsyncApiBuilder;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfModelRef {
    pub repo: String,
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRef {
    LocalPath(PathBuf),
    Hf(HfModelRef),
    HfRepo(String),
}

#[derive(Debug, Clone)]
pub enum ModelRefError {
    Invalid(String),
    Download(String),
}

impl std::fmt::Display for ModelRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "{msg}"),
            Self::Download(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ModelRefError {}

pub fn parse_model_ref(input: &str) -> Result<ModelRef, ModelRefError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(ModelRefError::Invalid("model cannot be empty".to_string()));
    }

    if raw.starts_with("hf:") {
        return Err(ModelRefError::Invalid(
            "legacy hf: prefix is no longer supported; use <repo>:<selector>".to_string(),
        ));
    }

    let path = Path::new(raw);
    let looks_like_path = raw.ends_with(".gguf")
        || raw.starts_with('.')
        || raw.starts_with('/')
        || raw.contains('\\')
        || path.exists();

    if looks_like_path {
        return Ok(ModelRef::LocalPath(PathBuf::from(raw)));
    }

    if let Some((left, right)) = raw.rsplit_once(':') {
        let repo = left.trim();
        let selector = right.trim();
        if repo.is_empty() || selector.is_empty() {
            return Err(ModelRefError::Invalid(
                "model must be formatted as <repo>:<selector>".to_string(),
            ));
        }
        if !repo.contains('/') {
            return Err(ModelRefError::Invalid(
                "Hugging Face model repo must include owner/name".to_string(),
            ));
        }
        return Ok(ModelRef::Hf(HfModelRef {
            repo: repo.to_string(),
            file: infer_gguf_filename(repo, selector),
        }));
    }

    if raw.contains('/') {
        return Ok(ModelRef::HfRepo(raw.to_string()));
    }

    Err(ModelRefError::Invalid(
        "model must be a local .gguf path, <repo>:<selector>, or <owner>/<repo>".to_string(),
    ))
}

pub fn infer_gguf_filename(repo: &str, selector: &str) -> String {
    if selector.ends_with(".gguf") {
        return selector.to_string();
    }
    let repo_name = repo.rsplit('/').next().unwrap_or(repo);
    let base = repo_name.strip_suffix("-GGUF").unwrap_or(repo_name);
    format!("{base}-{selector}.gguf")
}

pub fn resolve_hf_model_sync(model: &HfModelRef) -> Result<PathBuf, ModelRefError> {
    let api = SyncApiBuilder::new()
        .with_progress(true)
        .build()
        .map_err(|e| ModelRefError::Download(e.to_string()))?;
    api.model(model.repo.clone())
        .get(&model.file)
        .map_err(|e| ModelRefError::Download(e.to_string()))
}

pub fn resolve_hf_model_fast(model: &HfModelRef) -> Result<PathBuf, ModelRefError> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let repo = model.repo.clone();
        let file = model.file.clone();
        return tokio::task::block_in_place(|| {
            handle.block_on(async move {
                let api = AsyncApiBuilder::new()
                    .with_progress(true)
                    .high()
                    .build()
                    .map_err(|e| ModelRefError::Download(e.to_string()))?;
                api.model(repo)
                    .get(&file)
                    .await
                    .map_err(|e| ModelRefError::Download(e.to_string()))
            })
        });
    }

    resolve_hf_model_sync(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hf_with_quant() {
        let parsed = parse_model_ref("bartowski/Qwen2.5-Coder-32B-Instruct-GGUF:Q6_K").unwrap();
        assert_eq!(
            parsed,
            ModelRef::Hf(HfModelRef {
                repo: "bartowski/Qwen2.5-Coder-32B-Instruct-GGUF".to_string(),
                file: "Qwen2.5-Coder-32B-Instruct-Q6_K.gguf".to_string(),
            })
        );
    }

    #[test]
    fn parse_hf_with_filename() {
        let parsed = parse_model_ref(
            "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF:Qwen3-Coder-30B-A3B-Instruct-Q8_0.gguf",
        )
        .unwrap();
        assert_eq!(
            parsed,
            ModelRef::Hf(HfModelRef {
                repo: "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF".to_string(),
                file: "Qwen3-Coder-30B-A3B-Instruct-Q8_0.gguf".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_hf_prefix() {
        let err = parse_model_ref("hf:foo/bar:baz.gguf").unwrap_err();
        assert!(err.to_string().contains("legacy hf: prefix"));
    }
}
