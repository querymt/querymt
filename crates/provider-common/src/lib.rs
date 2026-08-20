use hf_hub::progress::{DownloadEvent, ProgressEvent, ProgressHandler};
use hf_hub::{HFClient, HFClientSync, split_id};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use log::debug;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfModelRef {
    pub repo: String,
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfFileRef {
    pub repo: String,
    pub file: String,
    pub revision: Option<String>,
}

impl HfFileRef {
    pub fn new(repo: impl Into<String>, file: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            file: file.into(),
            revision: None,
        }
    }

    pub fn with_revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = Some(revision.into());
        self
    }
}

impl From<&HfModelRef> for HfFileRef {
    fn from(model: &HfModelRef) -> Self {
        Self::new(&model.repo, &model.file)
    }
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

pub const QMT_HF_DOWNLOAD_CONCURRENCY: &str = "QMT_HF_DOWNLOAD_CONCURRENCY";
pub const DEFAULT_HF_DOWNLOAD_CONCURRENCY: usize = 8;

const XET_INITIAL_DOWNLOAD_CONCURRENCY: &str = "HF_XET_CLIENT_AC_INITIAL_DOWNLOAD_CONCURRENCY";
const XET_MIN_DOWNLOAD_CONCURRENCY: &str = "HF_XET_CLIENT_AC_MIN_DOWNLOAD_CONCURRENCY";
const XET_MAX_DOWNLOAD_CONCURRENCY: &str = "HF_XET_CLIENT_AC_MAX_DOWNLOAD_CONCURRENCY";
const XET_DOWNLOAD_CONCURRENCY_VARS: [&str; 3] = [
    XET_INITIAL_DOWNLOAD_CONCURRENCY,
    XET_MIN_DOWNLOAD_CONCURRENCY,
    XET_MAX_DOWNLOAD_CONCURRENCY,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfDownloadConcurrencySource {
    XetEnvironment,
    QueryMtEnvironment,
    QueryMtDefault,
    PartialXetEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfDownloadConcurrencyConfig {
    pub source: HfDownloadConcurrencySource,
    pub concurrency: Option<usize>,
}

/// Configure Xet download concurrency before the first Hugging Face client is created.
///
/// Explicit low-level Xet settings take precedence. Otherwise QueryMT applies
/// `QMT_HF_DOWNLOAD_CONCURRENCY`, falling back to eight concurrent downloads.
pub fn configure_hf_download_concurrency() -> HfDownloadConcurrencyConfig {
    let xet_values = XET_DOWNLOAD_CONCURRENCY_VARS.map(std::env::var_os);
    let configured_xet_vars = xet_values.iter().filter(|value| value.is_some()).count();

    if configured_xet_vars == XET_DOWNLOAD_CONCURRENCY_VARS.len() {
        let parsed = xet_values
            .iter()
            .map(|value| {
                value
                    .as_ref()
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .collect::<Option<Vec<_>>>();
        let concurrency = parsed.and_then(|values| {
            (values.iter().all(|value| *value > 0)
                && values.windows(2).all(|pair| pair[0] == pair[1]))
            .then_some(values[0])
        });
        return HfDownloadConcurrencyConfig {
            source: HfDownloadConcurrencySource::XetEnvironment,
            concurrency,
        };
    }

    if configured_xet_vars > 0 {
        return HfDownloadConcurrencyConfig {
            source: HfDownloadConcurrencySource::PartialXetEnvironment,
            concurrency: None,
        };
    }

    let (source, concurrency) = match std::env::var(QMT_HF_DOWNLOAD_CONCURRENCY) {
        Ok(value) => match value.parse::<usize>() {
            Ok(value) if value > 0 => (HfDownloadConcurrencySource::QueryMtEnvironment, value),
            _ => {
                log::warn!(
                    "Ignoring invalid {QMT_HF_DOWNLOAD_CONCURRENCY}={value:?}; using {DEFAULT_HF_DOWNLOAD_CONCURRENCY}"
                );
                (
                    HfDownloadConcurrencySource::QueryMtDefault,
                    DEFAULT_HF_DOWNLOAD_CONCURRENCY,
                )
            }
        },
        Err(_) => (
            HfDownloadConcurrencySource::QueryMtDefault,
            DEFAULT_HF_DOWNLOAD_CONCURRENCY,
        ),
    };
    let value = concurrency.to_string();
    for variable in XET_DOWNLOAD_CONCURRENCY_VARS {
        // SAFETY: Callers must invoke this during single-threaded process startup,
        // before constructing a Hugging Face client or spawning worker threads.
        unsafe { std::env::set_var(variable, &value) };
    }

    HfDownloadConcurrencyConfig {
        source,
        concurrency: Some(concurrency),
    }
}

pub fn log_hf_download_concurrency(config: &HfDownloadConcurrencyConfig) {
    match config.source {
        HfDownloadConcurrencySource::PartialXetEnvironment => log::warn!(
            "Partial Xet download concurrency configuration detected; set all of {}, {}, and {}",
            XET_INITIAL_DOWNLOAD_CONCURRENCY,
            XET_MIN_DOWNLOAD_CONCURRENCY,
            XET_MAX_DOWNLOAD_CONCURRENCY,
        ),
        HfDownloadConcurrencySource::XetEnvironment if config.concurrency.is_none() => log::warn!(
            "Xet download concurrency variables must be positive and equal to define a fixed QueryMT profile"
        ),
        _ => log::debug!(
            "Hugging Face download concurrency: {:?}, value={:?}",
            config.source,
            config.concurrency
        ),
    }
}

type SharedProgressCallback = Arc<dyn Fn(DownloadProgress) + Send + Sync>;

struct HfDownloadProgressHandler {
    callback: SharedProgressCallback,
    files: Mutex<HashMap<String, (u64, u64)>>,
}

impl HfDownloadProgressHandler {
    fn new(callback: SharedProgressCallback) -> Self {
        Self {
            callback,
            files: Mutex::new(HashMap::new()),
        }
    }

    fn emit(&self, bytes_downloaded: u64, bytes_total: u64, speed_bps: Option<f64>) {
        let bytes_downloaded = if bytes_total > 0 {
            bytes_downloaded.min(bytes_total)
        } else {
            bytes_downloaded
        };
        let percent =
            (bytes_total > 0).then_some(bytes_downloaded as f32 * 100.0 / bytes_total as f32);
        let eta_seconds = speed_bps
            .filter(|speed| *speed > 0.0)
            .map(|speed| ((bytes_total.saturating_sub(bytes_downloaded)) as f64 / speed) as u64);
        (self.callback)(DownloadProgress {
            bytes_downloaded,
            bytes_total: (bytes_total > 0).then_some(bytes_total),
            percent,
            speed_bps: speed_bps.map(|speed| speed as u64),
            eta_seconds,
            status: DownloadStatus::Downloading,
        });
    }
}

impl ProgressHandler for HfDownloadProgressHandler {
    fn on_progress(&self, event: &ProgressEvent) {
        match event {
            ProgressEvent::Download(DownloadEvent::Start { total_bytes, .. }) => {
                if let Ok(mut files) = self.files.lock() {
                    files.clear();
                }
                self.emit(0, *total_bytes, None);
            }
            ProgressEvent::Download(DownloadEvent::Progress { files }) => {
                let Ok(mut tracked) = self.files.lock() else {
                    return;
                };
                for file in files {
                    tracked.insert(
                        file.filename.clone(),
                        (file.bytes_completed, file.total_bytes),
                    );
                }
                let (bytes_downloaded, bytes_total) = tracked.values().fold(
                    (0_u64, 0_u64),
                    |(downloaded, total), (file_downloaded, file_total)| {
                        (
                            downloaded.saturating_add(*file_downloaded),
                            total.saturating_add(*file_total),
                        )
                    },
                );
                drop(tracked);
                self.emit(bytes_downloaded, bytes_total, None);
            }
            ProgressEvent::Download(DownloadEvent::AggregateProgress {
                bytes_completed,
                total_bytes,
                bytes_per_sec,
            }) => self.emit(*bytes_completed, *total_bytes, *bytes_per_sec),
            ProgressEvent::Download(DownloadEvent::Complete) | ProgressEvent::Upload(_) => {}
        }
    }
}

pub fn no_progress() -> ProgressCallback {
    Box::new(|_| {})
}

struct TerminalProgressState {
    bar: ProgressBar,
    interactive: bool,
    determinate: bool,
    finished: bool,
    last_log: Instant,
}

pub fn terminal_progress(label: impl Into<String>) -> ProgressCallback {
    let label = label.into();
    let interactive = std::io::stderr().is_terminal();
    let bar = ProgressBar::new_spinner();
    if interactive {
        bar.set_draw_target(ProgressDrawTarget::stderr());
        bar.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .expect("valid spinner progress template"),
        );
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.set_message(format!("Downloading {label}"));
    } else {
        bar.set_draw_target(ProgressDrawTarget::hidden());
    }
    let state = Arc::new(Mutex::new(TerminalProgressState {
        bar,
        interactive,
        determinate: false,
        finished: false,
        last_log: Instant::now(),
    }));

    Box::new(move |progress| {
        let Ok(mut state) = state.lock() else {
            return;
        };
        if state.finished {
            return;
        }

        match progress.status {
            DownloadStatus::Starting => {
                if !state.interactive {
                    eprintln!("Downloading {label}...");
                }
            }
            DownloadStatus::Downloading => {
                if state.interactive {
                    if let Some(total) = progress.bytes_total.filter(|total| *total > 0) {
                        if !state.determinate {
                            state.bar.disable_steady_tick();
                            state.bar.set_style(
                                ProgressStyle::with_template(
                                    "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} \
                                     ({bytes_per_sec}, {eta})",
                                )
                                .expect("valid download progress template")
                                .progress_chars("=>-"),
                            );
                            state.determinate = true;
                        }
                        state.bar.set_length(total);
                    }
                    state.bar.set_position(progress.bytes_downloaded);
                    state.bar.set_message(format!("Downloading {label}"));
                } else if state.last_log.elapsed() >= Duration::from_secs(10) {
                    if let Some(percent) = progress.percent {
                        eprintln!("Downloading {label}: {percent:.1}%");
                    } else {
                        eprintln!("Downloading {label}: {} bytes", progress.bytes_downloaded);
                    }
                    state.last_log = Instant::now();
                }
            }
            DownloadStatus::Verifying => {
                if state.interactive {
                    state.bar.set_message(format!("Verifying {label}"));
                }
            }
            DownloadStatus::Completed => {
                if state.interactive {
                    state.bar.finish_with_message(format!("Downloaded {label}"));
                } else {
                    eprintln!("Downloaded {label}");
                }
                state.finished = true;
            }
            DownloadStatus::Failed(message) => {
                if state.interactive {
                    state
                        .bar
                        .abandon_with_message(format!("Failed to download {label}: {message}"));
                } else {
                    eprintln!("Failed to download {label}: {message}");
                }
                state.finished = true;
            }
        }
    })
}

fn model_repository(
    client: &HFClient,
    repo_id: &str,
) -> hf_hub::HFRepository<hf_hub::RepoTypeModel> {
    let (owner, name) = split_id(repo_id);
    client.model(owner, name)
}

fn model_repository_sync(
    client: &HFClientSync,
    repo_id: &str,
) -> hf_hub::HFRepositorySync<hf_hub::RepoTypeModel> {
    let (owner, name) = split_id(repo_id);
    client.model(owner, name)
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
    models.sort_by_key(|b| std::cmp::Reverse(b.modified));
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

fn starting_progress() -> DownloadProgress {
    DownloadProgress {
        bytes_downloaded: 0,
        bytes_total: None,
        percent: None,
        speed_bps: None,
        eta_seconds: None,
        status: DownloadStatus::Starting,
    }
}

fn finish_download<E: std::fmt::Display>(
    result: Result<PathBuf, E>,
    progress_cb: &SharedProgressCallback,
) -> Result<PathBuf, ModelRefError> {
    match result {
        Ok(path) => {
            let bytes_downloaded = std::fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            for status in [DownloadStatus::Verifying, DownloadStatus::Completed] {
                progress_cb(DownloadProgress {
                    bytes_downloaded,
                    bytes_total: Some(bytes_downloaded),
                    percent: Some(100.0),
                    speed_bps: None,
                    eta_seconds: Some(0),
                    status,
                });
            }
            Ok(path)
        }
        Err(error) => {
            let message = error.to_string();
            progress_cb(DownloadProgress {
                bytes_downloaded: 0,
                bytes_total: None,
                percent: None,
                speed_bps: None,
                eta_seconds: None,
                status: DownloadStatus::Failed(message.clone()),
            });
            Err(ModelRefError::Download(message))
        }
    }
}

pub async fn download_hf_file(
    file: &HfFileRef,
    progress_cb: ProgressCallback,
) -> Result<PathBuf, ModelRefError> {
    let progress_cb: SharedProgressCallback = Arc::from(progress_cb);
    debug!(
        "download_hf_file: async download for {}/{}",
        file.repo, file.file
    );
    let client = HFClient::builder()
        .build()
        .map_err(|e| ModelRefError::Download(e.to_string()))?;
    progress_cb(starting_progress());
    let repo = model_repository(&client, &file.repo);
    let result = match &file.revision {
        Some(revision) => {
            repo.download_file()
                .filename(&file.file)
                .progress(HfDownloadProgressHandler::new(Arc::clone(&progress_cb)))
                .revision(revision)
                .send()
                .await
        }
        None => {
            repo.download_file()
                .filename(&file.file)
                .progress(HfDownloadProgressHandler::new(Arc::clone(&progress_cb)))
                .send()
                .await
        }
    };

    finish_download(result, &progress_cb)
}

pub fn download_hf_file_sync(
    file: &HfFileRef,
    progress_cb: ProgressCallback,
) -> Result<PathBuf, ModelRefError> {
    let progress_cb: SharedProgressCallback = Arc::from(progress_cb);
    debug!(
        "download_hf_file_sync: blocking download for {}/{}",
        file.repo, file.file
    );
    let client = HFClient::builder()
        .build_sync()
        .map_err(|e| ModelRefError::Download(e.to_string()))?;
    progress_cb(starting_progress());
    let repo = model_repository_sync(&client, &file.repo);
    let result = match &file.revision {
        Some(revision) => repo
            .download_file()
            .filename(&file.file)
            .progress(HfDownloadProgressHandler::new(Arc::clone(&progress_cb)))
            .revision(revision)
            .send(),
        None => repo
            .download_file()
            .filename(&file.file)
            .progress(HfDownloadProgressHandler::new(Arc::clone(&progress_cb)))
            .send(),
    };

    finish_download(result, &progress_cb)
}

/// Preferred mmproj filenames in priority order (best quality/size tradeoff first).
const MMPROJ_PREFERENCES: &[&str] = &["mmproj-F16.gguf", "mmproj-BF16.gguf", "mmproj-F32.gguf"];

/// Discover mmproj GGUF files in a Hugging Face repo by querying the repo's file listing.
///
/// Queries the HF API for the repo's siblings and looks for filenames matching
/// `mmproj*.gguf` (case-insensitive). Returns the best-matched filename according
/// to [`MMPROJ_PREFERENCES`], or the first discovered file if none of the preferred
/// names match.
///
/// Returns `Ok(None)` if no mmproj files are found or the repo cannot be queried.
/// Errors are suppressed and returned as `Ok(None)` so that callers can treat
/// auto-discovery as a best-effort operation.
pub fn discover_mmproj_in_hf_repo(repo: &str) -> Result<Option<String>, ModelRefError> {
    let client = HFClient::builder()
        .build_sync()
        .map_err(|e| ModelRefError::Download(e.to_string()))?;
    let info = model_repository_sync(&client, repo)
        .info()
        .send()
        .map_err(|e| ModelRefError::Download(e.to_string()))?;

    let mmproj_files: Vec<String> = info
        .siblings
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.rfilename)
        .filter(|f| {
            // Grab just the filename portion (repos may have subdirectories)
            let name = f.rsplit('/').next().unwrap_or(f).to_lowercase();
            name.starts_with("mmproj") && name.ends_with(".gguf")
        })
        .collect();

    if mmproj_files.is_empty() {
        return Ok(None);
    }

    // Pick the best match: check preferences first, then fall back to the first found
    for pref in MMPROJ_PREFERENCES {
        if let Some(f) = mmproj_files
            .iter()
            .find(|f| f.rsplit('/').next().unwrap_or(f.as_str()) == *pref)
        {
            return Ok(Some(f.clone()));
        }
    }

    Ok(Some(mmproj_files[0].clone()))
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

    /// Requires network access. Run with:
    /// `cargo test -p querymt-provider-common -- --ignored discover_mmproj`
    #[test]
    #[ignore]
    fn discover_mmproj_qwen3vl() {
        let result = discover_mmproj_in_hf_repo("unsloth/Qwen3-VL-8B-Instruct-GGUF");
        assert!(result.is_ok(), "API call failed: {:?}", result.err());
        let file = result.unwrap();
        assert_eq!(file.as_deref(), Some("mmproj-F16.gguf"));
    }

    #[test]
    #[ignore]
    fn discover_mmproj_text_only_repo() {
        // A text-only repo should return None
        let result = discover_mmproj_in_hf_repo("bartowski/Qwen2.5-Coder-32B-Instruct-GGUF");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
