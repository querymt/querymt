use reqwest::Client;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::types::ProvidersRegistry;
use crate::error::LLMError;

const CACHE_FILE: &str = "models.dev.json";
const CACHE_DURATION: u64 = 86_400; // 24 hours in seconds
const API_URL: &str = "https://models.dev/api.json";

fn provider_cache_dir() -> Result<PathBuf, LLMError> {
    if let Ok(path) = std::env::var("QMT_PROVIDER_CACHE_DIR")
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = std::env::var("QMT_CONFIG_DIR")
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = std::env::var("QMT_HOME")
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    dirs::home_dir()
        .map(|home| home.join(".qmt"))
        .ok_or_else(|| {
            LLMError::GenericError("Could not determine QueryMT provider cache directory".into())
        })
}

fn provider_cache_path() -> Result<PathBuf, LLMError> {
    Ok(provider_cache_dir()?.join(CACHE_FILE))
}

fn is_cache_fresh(file_path: &Path) -> bool {
    if let Ok(metadata) = fs::metadata(file_path)
        && let Ok(modified) = metadata.modified()
        && let Ok(modified_time) = modified.duration_since(UNIX_EPOCH)
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        return now.as_secs() - modified_time.as_secs() < CACHE_DURATION;
    }
    false
}

async fn download_and_cache_providers(file_path: &Path) -> Result<ProvidersRegistry, LLMError> {
    let client = Client::new();
    let response = client.get(API_URL).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await?.to_vec();
        return Err(crate::error::classify_http_status(
            status.as_u16(),
            &headers,
            &body,
        ));
    }

    // API returns a top-level map of providers, convert into ProvidersRegistry
    let mut map = response
        .json::<std::collections::HashMap<String, super::types::ProviderInfo>>()
        .await?;
    for model in map.values_mut().flat_map(|p| p.models.values_mut()) {
        model.pricing.source = Some(super::types::PricingSource::Catalog);
    }

    let registry: ProvidersRegistry = map.into();

    let json = serde_json::to_string(&registry)?;
    fs::create_dir_all(file_path.parent().unwrap())?;
    let mut file = File::create(file_path)?;
    file.write_all(json.as_bytes())?;

    Ok(registry)
}

/// Identity of the on-disk registry file a parsed registry was built from.
///
/// The cache is invalidated when the path, modification time or length
/// changes, so a refreshed `models.dev.json` is picked up without a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    fn of(path: &Path) -> Result<Self, LLMError> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

struct CachedRegistry {
    stamp: FileStamp,
    registry: Arc<ProvidersRegistry>,
}

/// Process-wide parsed registry. The file is several MB; parsing it per lookup
/// costs ~100ms and used to run several times per agent step.
static REGISTRY_CACHE: RwLock<Option<CachedRegistry>> = RwLock::new(None);

fn parse_registry_file(path: &Path) -> Result<ProvidersRegistry, LLMError> {
    let mut file = File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(serde_json::from_str(&contents)?)
}

fn cached_registry(stamp: &FileStamp) -> Option<Arc<ProvidersRegistry>> {
    let guard = REGISTRY_CACHE.read().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .filter(|entry| entry.stamp == *stamp)
        .map(|entry| Arc::clone(&entry.registry))
}

/// The parsed providers registry, shared process-wide.
///
/// The hot path is one `stat` plus an `Arc` clone. The file is parsed only on
/// first use or after it changed on disk; each (re)parse is traced as
/// `providers.registry.load` so regressions in lookup cost stay visible.
pub fn providers_registry() -> Result<Arc<ProvidersRegistry>, LLMError> {
    let path = provider_cache_path()?;
    // Stat before reading: if the file changes mid-parse the stored stamp is
    // already stale and the next call reloads.
    let stamp = FileStamp::of(&path)?;
    if let Some(hit) = cached_registry(&stamp) {
        return Ok(hit);
    }

    let mut guard = REGISTRY_CACHE.write().unwrap_or_else(|e| e.into_inner());
    // Another thread may have reloaded while we waited for the write lock.
    if let Some(entry) = guard.as_ref().filter(|entry| entry.stamp == stamp) {
        return Ok(Arc::clone(&entry.registry));
    }

    let reason = if guard.is_some() { "changed" } else { "cold" };
    #[cfg(feature = "tracing")]
    let span = tracing::info_span!(
        "providers.registry.load",
        reason,
        bytes = stamp.len,
        parse_ms = tracing::field::Empty,
    );
    #[cfg(feature = "tracing")]
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let registry = Arc::new(parse_registry_file(&stamp.path)?);
    #[cfg(feature = "tracing")]
    span.record("parse_ms", started.elapsed().as_millis() as u64);
    #[cfg(not(feature = "tracing"))]
    let _ = (reason, started);

    *guard = Some(CachedRegistry {
        stamp,
        registry: Arc::clone(&registry),
    });
    Ok(registry)
}

/// Parse the registry from disk on every call.
///
/// Prefer [`providers_registry`], which parses once and shares the result.
pub fn read_providers_from_cache() -> Result<ProvidersRegistry, LLMError> {
    parse_registry_file(&provider_cache_path()?)
}

pub async fn update_providers_if_stale() -> Result<bool, LLMError> {
    let file_path = provider_cache_path()?;

    if is_cache_fresh(&file_path) {
        return Ok(false);
    }

    download_and_cache_providers(&file_path).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The cache dir comes from a process-wide env var and the cache is a
    /// process-wide static, so these tests must not interleave.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct CacheDir {
        dir: PathBuf,
        previous: Option<std::ffi::OsString>,
    }

    impl CacheDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "qmt-registry-test-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            let previous = std::env::var_os("QMT_PROVIDER_CACHE_DIR");
            // SAFETY: serialized by ENV_LOCK; no other thread reads this var concurrently.
            unsafe { std::env::set_var("QMT_PROVIDER_CACHE_DIR", &dir) };
            Self { dir, previous }
        }

        fn write(&self, json: &str) {
            fs::write(self.dir.join(CACHE_FILE), json).unwrap();
        }
    }

    impl Drop for CacheDir {
        fn drop(&mut self) {
            // SAFETY: see `new`.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("QMT_PROVIDER_CACHE_DIR", value),
                    None => std::env::remove_var("QMT_PROVIDER_CACHE_DIR"),
                }
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn registry_json(provider_id: &str) -> String {
        format!(
            r#"{{"providers":{{"{provider_id}":{{"id":"{provider_id}","name":"{provider_id}"}}}}}}"#
        )
    }

    #[test]
    fn repeated_lookups_share_one_parsed_registry() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cache = CacheDir::new("share");
        cache.write(&registry_json("alpha"));

        let first = providers_registry().unwrap();
        let second = providers_registry().unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "second call must reuse the parse"
        );
        assert!(first.providers.contains_key("alpha"));
    }

    #[test]
    fn changed_file_is_reloaded() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cache = CacheDir::new("reload");
        cache.write(&registry_json("alpha"));
        let before = providers_registry().unwrap();

        // A different length invalidates the stamp even when mtime granularity
        // is coarser than the time between the two writes.
        cache.write(&registry_json("beta-longer-id"));
        let after = providers_registry().unwrap();

        assert!(!Arc::ptr_eq(&before, &after));
        assert!(after.providers.contains_key("beta-longer-id"));
        assert!(!after.providers.contains_key("alpha"));
    }

    #[test]
    fn different_cache_dirs_do_not_share_an_entry() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let first_dir = CacheDir::new("dir-a");
        first_dir.write(&registry_json("alpha"));
        let first = providers_registry().unwrap();
        drop(first_dir);

        let second_dir = CacheDir::new("dir-b");
        second_dir.write(&registry_json("alpha"));
        let second = providers_registry().unwrap();

        assert!(
            !Arc::ptr_eq(&first, &second),
            "the path is part of the stamp, so a new directory must reparse"
        );
    }

    #[test]
    fn missing_file_is_an_error_not_a_stale_hit() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cache = CacheDir::new("missing");
        assert!(providers_registry().is_err());
        cache.write(&registry_json("alpha"));
        assert!(providers_registry().is_ok());
    }

    #[test]
    fn uncached_reader_still_parses_every_call() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cache = CacheDir::new("uncached");
        cache.write(&registry_json("alpha"));
        let registry = read_providers_from_cache().unwrap();
        assert!(registry.providers.contains_key("alpha"));
    }
}
