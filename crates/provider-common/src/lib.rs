use hf_hub::api::sync::ApiBuilder as SyncApiBuilder;
use hf_hub::api::tokio::ApiBuilder as AsyncApiBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

#[derive(Debug, Clone)]
pub struct CachedGgufModel {
    pub repo: String,
    pub filename: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone)]
pub struct GgufMetadata {
    pub family: String,
    pub quant: String,
}

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    pub percent: Option<f32>,
    pub speed_bps: Option<u64>,
    pub eta_seconds: Option<u64>,
    pub status: DownloadStatus,
}

#[derive(Debug, Clone)]
pub enum DownloadStatus {
    Starting,
    Downloading,
    Verifying,
    Completed,
    Failed(String),
}

pub type ProgressCallback = Box<dyn Fn(DownloadProgress) + Send + Sync>;

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
        if let Some(rest) = raw.strip_prefix("hf:")
            && let Some((repo, filename)) = rest.split_once(':')
        {
            return Ok(ModelRef::Hf(HfModelRef {
                repo: repo.to_string(),
                file: filename.to_string(),
            }));
        }
        return Err(ModelRefError::Invalid(
            "hf: model refs must be formatted as hf:<repo>:<filename>".to_string(),
        ));
    }

    if raw.starts_with("file:") {
        let file = raw.trim_start_matches("file:").trim();
        if file.is_empty() {
            return Err(ModelRefError::Invalid(
                "file: model refs must include a path".to_string(),
            ));
        }
        return Ok(ModelRef::LocalPath(PathBuf::from(file)));
    }

    if is_windows_abs_path(raw) {
        return Ok(ModelRef::LocalPath(PathBuf::from(raw)));
    }

    // Parse HF refs before generic local path heuristics so `<repo>:<file.gguf>`
    // doesn't get misclassified as a local path.
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

    let path = Path::new(raw);
    let looks_like_path = raw.ends_with(".gguf")
        || raw.starts_with('.')
        || raw.starts_with('/')
        || raw.starts_with("\\\\")
        || raw.contains('\\')
        || path.exists();

    if looks_like_path {
        return Ok(ModelRef::LocalPath(PathBuf::from(raw)));
    }

    if raw.contains('/') {
        return Ok(ModelRef::HfRepo(raw.to_string()));
    }

    Err(ModelRefError::Invalid(
        "model must be a local .gguf path, <repo>:<selector>, or <owner>/<repo>".to_string(),
    ))
}

pub fn canonical_id_from_hf(repo: &str, filename: &str) -> String {
    format!("hf:{repo}:{filename}")
}

pub fn canonical_id_from_file(path: &Path) -> String {
    format!("file:{}", path.display())
}

pub fn parse_canonical_id(id: &str) -> Result<ModelRef, ModelRefError> {
    parse_model_ref(id)
}

pub fn parse_gguf_metadata(filename: &str) -> GgufMetadata {
    let stem = filename.strip_suffix(".gguf").unwrap_or(filename);
    let mut quant = "unknown".to_string();
    let mut family = stem.to_string();

    let segments: Vec<&str> = stem.split('-').collect();
    if let Some(last) = segments.last() {
        let upper = last.to_ascii_uppercase();
        if is_quant_segment(&upper) {
            quant = upper;
            family = segments[..segments.len().saturating_sub(1)].join("-");
            if family.is_empty() {
                family = stem.to_string();
            }
        }
    }

    GgufMetadata { family, quant }
}

pub fn list_cached_hf_gguf_models() -> Result<Vec<CachedGgufModel>, ModelRefError> {
    let home = dirs::home_dir()
        .ok_or_else(|| ModelRefError::Invalid("failed to resolve home directory".to_string()))?;
    let root = home.join(".cache").join("huggingface").join("hub");

    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut deduped: HashMap<(String, String), CachedGgufModel> = HashMap::new();
    let model_dirs = std::fs::read_dir(&root)
        .map_err(|e| ModelRefError::Invalid(format!("failed to read HF cache root: {e}")))?;

    for entry in model_dirs {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }

        let dirname = entry.file_name();
        let dirname = dirname.to_string_lossy();
        if !dirname.starts_with("models--") {
            continue;
        }
        let repo = dirname.trim_start_matches("models--").replace("--", "/");
        let snapshots_dir = entry.path().join("snapshots");
        if !snapshots_dir.is_dir() {
            continue;
        }

        let snapshots = match std::fs::read_dir(&snapshots_dir) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for snapshot in snapshots.flatten() {
            let snapshot_path = snapshot.path();
            if !snapshot_path.is_dir() {
                continue;
            }
            let files = match std::fs::read_dir(&snapshot_path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for file in files.flatten() {
                let path = file.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("gguf") {
                    continue;
                }
                let filename = match path.file_name().and_then(|s| s.to_str()) {
                    Some(f) => f.to_string(),
                    None => continue,
                };

                let metadata = match file.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let model = CachedGgufModel {
                    repo: repo.clone(),
                    filename: filename.clone(),
                    path,
                    size_bytes: metadata.len(),
                    modified,
                };

                let key = (repo.clone(), filename);
                match deduped.get(&key) {
                    Some(existing) if existing.modified >= model.modified => {}
                    _ => {
                        deduped.insert(key, model);
                    }
                }
            }
        }
    }

    let mut models: Vec<CachedGgufModel> = deduped.into_values().collect();
    models.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(models)
}

fn is_windows_abs_path(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn is_quant_segment(seg: &str) -> bool {
    seg.starts_with('Q') && seg.chars().skip(1).any(|c| c.is_ascii_digit())
}

pub fn infer_gguf_filename(repo: &str, selector: &str) -> String {
    if selector.ends_with(".gguf") {
        return selector.to_string();
    }
    let repo_name = repo.rsplit('/').next().unwrap_or(repo);
    let base = repo_name.strip_suffix("-GGUF").unwrap_or(repo_name);
    format!("{base}-{selector}.gguf")
}

pub async fn download_hf_gguf_with_progress(
    model: &HfModelRef,
    progress_cb: ProgressCallback,
) -> Result<PathBuf, ModelRefError> {
    progress_cb(DownloadProgress {
        bytes_downloaded: 0,
        bytes_total: None,
        percent: None,
        speed_bps: None,
        eta_seconds: None,
        status: DownloadStatus::Starting,
    });

    let api = AsyncApiBuilder::new()
        .with_progress(true)
        .high()
        .build()
        .map_err(|e| ModelRefError::Download(e.to_string()))?;

    progress_cb(DownloadProgress {
        bytes_downloaded: 0,
        bytes_total: None,
        percent: None,
        speed_bps: None,
        eta_seconds: None,
        status: DownloadStatus::Downloading,
    });

    let result = api.model(model.repo.clone()).get(&model.file).await;
    match result {
        Ok(path) => {
            progress_cb(DownloadProgress {
                bytes_downloaded: 0,
                bytes_total: None,
                percent: Some(100.0),
                speed_bps: None,
                eta_seconds: Some(0),
                status: DownloadStatus::Verifying,
            });
            progress_cb(DownloadProgress {
                bytes_downloaded: 0,
                bytes_total: None,
                percent: Some(100.0),
                speed_bps: None,
                eta_seconds: Some(0),
                status: DownloadStatus::Completed,
            });
            Ok(path)
        }
        Err(e) => {
            let msg = e.to_string();
            progress_cb(DownloadProgress {
                bytes_downloaded: 0,
                bytes_total: None,
                percent: None,
                speed_bps: None,
                eta_seconds: None,
                status: DownloadStatus::Failed(msg.clone()),
            });
            Err(ModelRefError::Download(msg))
        }
    }
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
        let model = model.clone();
        return tokio::task::block_in_place(|| {
            handle.block_on(async move {
                download_hf_gguf_with_progress(&model, Box::new(|_| {})).await
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
    fn parse_hf_prefix_for_canonical_id() {
        let parsed = parse_model_ref("hf:foo/bar:baz.gguf").unwrap();
        assert_eq!(
            parsed,
            ModelRef::Hf(HfModelRef {
                repo: "foo/bar".to_string(),
                file: "baz.gguf".to_string(),
            })
        );
    }

    #[test]
    fn parse_file_prefix_for_canonical_id() {
        let parsed = parse_model_ref("file:/tmp/test.gguf").unwrap();
        assert_eq!(parsed, ModelRef::LocalPath(PathBuf::from("/tmp/test.gguf")));
    }

    #[test]
    fn parse_relative_gguf_path() {
        let parsed = parse_model_ref("./models/Qwen3-Q8_0.gguf").unwrap();
        assert_eq!(
            parsed,
            ModelRef::LocalPath(PathBuf::from("./models/Qwen3-Q8_0.gguf"))
        );
    }

    #[test]
    fn parse_windows_abs_gguf_path() {
        let parsed = parse_model_ref("C:\\models\\Qwen3-Q8_0.gguf").unwrap();
        assert_eq!(
            parsed,
            ModelRef::LocalPath(PathBuf::from("C:\\models\\Qwen3-Q8_0.gguf"))
        );
    }

    #[test]
    fn canonical_id_helpers() {
        assert_eq!(
            canonical_id_from_hf("foo/bar", "model.gguf"),
            "hf:foo/bar:model.gguf"
        );
        assert_eq!(
            canonical_id_from_file(Path::new("/tmp/m.gguf")),
            "file:/tmp/m.gguf"
        );
    }

    #[test]
    fn parse_gguf_metadata_detects_quant_and_family() {
        let meta = parse_gguf_metadata("Qwen2.5-Coder-32B-Instruct-Q8_0.gguf");
        assert_eq!(meta.family, "Qwen2.5-Coder-32B-Instruct");
        assert_eq!(meta.quant, "Q8_0");

        let unknown = parse_gguf_metadata("model.gguf");
        assert_eq!(unknown.family, "model");
        assert_eq!(unknown.quant, "unknown");
    }
}
