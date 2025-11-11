use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

use super::registry::ServerResponse;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Cache entry not found")]
    NotFound,

    #[error("Cache is stale")]
    Stale,
}

/// Cache for MCP registry data with optional TTL
#[derive(Debug, Clone)]
pub struct RegistryCache {
    cache_dir: PathBuf,
    ttl: Option<Duration>,
}

/// Cached server list data
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedServers {
    servers: Vec<ServerResponse>,
    cached_at: u64,
}

/// Cached server version data
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedVersion {
    version: ServerResponse,
    cached_at: u64,
}

impl RegistryCache {
    /// Create a new cache instance
    ///
    /// # Arguments
    /// * `cache_dir` - Directory to store cache files
    /// * `ttl` - Time-to-live for cached entries (None = no expiration)
    pub fn new(cache_dir: PathBuf, ttl: Option<Duration>) -> Self {
        Self { cache_dir, ttl }
    }

    /// Create a cache with default settings (uses system cache dir + querymt/mcp-registries/, 24 hour TTL)
    pub fn default_cache() -> Result<Self> {
        let cache_dir = dirs::cache_dir()
            .map(|mut path| {
                path.push("querymt");
                path.push("mcp-registries");
                path
            })
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find cache directory",
                )
            })?;

        fs::create_dir_all(&cache_dir)?;

        Ok(Self::new(cache_dir, Some(Duration::from_secs(86_400))))
    }

    /// Create a cache with no expiration
    pub fn permanent_cache(cache_dir: PathBuf) -> Self {
        Self::new(cache_dir, None)
    }

    /// Get the cache file path for a registry URL
    fn get_registry_cache_path(&self, registry_url: &str) -> PathBuf {
        // Create a safe filename from the registry URL
        let hash = Self::url_to_filename(registry_url);
        self.cache_dir.join(format!("{}_servers.json", hash))
    }

    /// Get the cache file path for a specific server version
    fn get_version_cache_path(
        &self,
        registry_url: &str,
        server_id: &str,
        version: &str,
    ) -> PathBuf {
        let registry_hash = Self::url_to_filename(registry_url);
        let server_hash = Self::url_to_filename(server_id);
        let version_hash = Self::url_to_filename(version);

        self.cache_dir
            .join(registry_hash)
            .join(format!("{}_{}.json", server_hash, version_hash))
    }

    /// Convert a URL to a safe filename
    fn url_to_filename(url: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        let result = hasher.finalize();
        hex::encode(&result[..8]) // Use first 8 bytes for shorter names
    }

    /// Check if a cache file is stale based on TTL
    fn is_stale(&self, cached_at: u64) -> bool {
        if let Some(ttl) = self.ttl {
            if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
                let age = Duration::from_secs(now.as_secs().saturating_sub(cached_at));
                return age > ttl;
            }
        }
        false // No TTL or time error = not stale
    }

    /// Get cached server list for a registry
    pub fn get_cached_servers(&self, registry_url: &str) -> Result<Vec<ServerResponse>, CacheError> {
        let cache_path = self.get_registry_cache_path(registry_url);

        if !cache_path.exists() {
            return Err(CacheError::NotFound);
        }

        let mut file = File::open(&cache_path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;

        let cached: CachedServers = serde_json::from_str(&contents)?;

        if self.is_stale(cached.cached_at) {
            return Err(CacheError::Stale);
        }

        Ok(cached.servers)
    }

    /// Cache a server list
    pub fn cache_servers(
        &self,
        registry_url: &str,
        servers: Vec<ServerResponse>,
    ) -> Result<(), CacheError> {
        let cache_path = self.get_registry_cache_path(registry_url);

        // Ensure parent directory exists
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let cached = CachedServers {
            servers,
            cached_at: now,
        };

        let json = serde_json::to_string_pretty(&cached)?;
        let mut file = File::create(&cache_path)?;
        file.write_all(json.as_bytes())?;

        Ok(())
    }

    /// Get a cached server version
    pub fn get_cached_version(
        &self,
        registry_url: &str,
        server_id: &str,
        version: &str,
    ) -> Result<ServerResponse, CacheError> {
        let cache_path = self.get_version_cache_path(registry_url, server_id, version);

        if !cache_path.exists() {
            return Err(CacheError::NotFound);
        }

        let mut file = File::open(&cache_path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;

        let cached: CachedVersion = serde_json::from_str(&contents)?;

        if self.is_stale(cached.cached_at) {
            return Err(CacheError::Stale);
        }

        Ok(cached.version)
    }

    /// Cache a server version
    pub fn cache_version(
        &self,
        registry_url: &str,
        server_id: &str,
        version: &str,
        server_version: ServerResponse,
    ) -> Result<(), CacheError> {
        let cache_path = self.get_version_cache_path(registry_url, server_id, version);

        // Ensure parent directories exist
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let cached = CachedVersion {
            version: server_version,
            cached_at: now,
        };

        let json = serde_json::to_string_pretty(&cached)?;
        let mut file = File::create(&cache_path)?;
        file.write_all(json.as_bytes())?;

        Ok(())
    }

    /// Clear all cache for a specific registry
    pub fn clear_registry_cache(&self, registry_url: &str) -> Result<(), CacheError> {
        let servers_path = self.get_registry_cache_path(registry_url);

        // Remove servers list
        if servers_path.exists() {
            fs::remove_file(servers_path)?;
        }

        // Remove version caches
        let registry_hash = Self::url_to_filename(registry_url);
        let versions_dir = self.cache_dir.join(registry_hash);
        if versions_dir.exists() {
            fs::remove_dir_all(versions_dir)?;
        }

        Ok(())
    }

    /// Clear all cached data
    pub fn clear_all(&self) -> Result<(), CacheError> {
        if self.cache_dir.exists() {
            fs::remove_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }

    /// Get statistics about the cache
    pub fn get_stats(&self) -> Result<CacheStats, CacheError> {
        let mut stats = CacheStats::default();

        if !self.cache_dir.exists() {
            return Ok(stats);
        }

        // Count server list caches
        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() && path.extension().map_or(false, |e| e == "json") {
                stats.server_lists += 1;
            } else if path.is_dir() {
                // Count version caches in subdirectories
                for version_entry in fs::read_dir(&path)? {
                    let version_entry = version_entry?;
                    if version_entry.path().is_file() {
                        stats.cached_versions += 1;
                    }
                }
            }
        }

        Ok(stats)
    }
}

/// Statistics about cached data
#[derive(Debug, Default, Clone)]
pub struct CacheStats {
    pub server_lists: usize,
    pub cached_versions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::registry::{ServerDetail, ServerResponse, RegistryMetadata, OfficialMetadata};
    use tempfile::TempDir;

    fn create_test_server(name: &str, description: &str) -> ServerResponse {
        ServerResponse {
            server: ServerDetail {
                schema: None,
                name: name.to_string(),
                title: None,
                description: description.to_string(),
                icons: None,
                repository: None,
                version: "1.0.0".to_string(),
                packages: vec![],
            },
            meta: RegistryMetadata {
                official: Some(OfficialMetadata {
                    status: "active".to_string(),
                    published_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    is_latest: true,
                }),
            },
        }
    }

    #[test]
    fn test_cache_servers() {
        let temp_dir = TempDir::new().unwrap();
        let cache =
            RegistryCache::new(temp_dir.path().to_path_buf(), Some(Duration::from_secs(60)));

        let servers = vec![create_test_server("Test Server 1", "A test server")];

        // Cache servers
        cache
            .cache_servers("https://test.registry.com", servers.clone())
            .unwrap();

        // Retrieve from cache
        let cached = cache
            .get_cached_servers("https://test.registry.com")
            .unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].server.name, servers[0].server.name);
    }

    #[test]
    fn test_cache_expiration() {
        let temp_dir = TempDir::new().unwrap();
        let cache = RegistryCache::new(temp_dir.path().to_path_buf(), Some(Duration::from_secs(0)));

        let servers = vec![create_test_server("Test Server 1", "A test server")];

        cache
            .cache_servers("https://test.registry.com", servers)
            .unwrap();

        // Should be stale immediately due to 0 TTL
        std::thread::sleep(Duration::from_millis(10));
        let result = cache.get_cached_servers("https://test.registry.com");
        assert!(matches!(result, Err(CacheError::Stale)));
    }

    #[test]
    fn test_no_expiration() {
        let temp_dir = TempDir::new().unwrap();
        let cache = RegistryCache::permanent_cache(temp_dir.path().to_path_buf());

        let servers = vec![create_test_server("Test Server 1", "A test server")];

        cache
            .cache_servers("https://test.registry.com", servers.clone())
            .unwrap();

        // Should never be stale
        let cached = cache
            .get_cached_servers("https://test.registry.com")
            .unwrap();
        assert_eq!(cached.len(), 1);
    }
}
