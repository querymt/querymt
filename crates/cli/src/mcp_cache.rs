use anyhow::{Context, Result};
use querymt::mcp::registry::ServerResponse;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// SQLite-based cache for MCP registry with full-text search support
pub struct RegistryCache {
    conn: Connection,
    ttl: Option<Duration>,
}

impl RegistryCache {
    /// Create a new cache instance with SQLite backend
    ///
    /// # Arguments
    /// * `cache_path` - Path to the SQLite database file
    /// * `ttl` - Time-to-live for cached entries (None = no expiration)
    pub fn new(cache_path: PathBuf, ttl: Option<Duration>) -> Result<Self> {
        let conn = Connection::open(&cache_path)
            .context("Failed to open SQLite database")?;

        let mut cache = Self { conn, ttl };
        cache.initialize_schema()?;
        Ok(cache)
    }

    /// Create a cache with default settings (uses system cache dir, 24 hour TTL)
    pub fn default_cache() -> Result<Self> {
        let cache_dir = dirs::cache_dir()
            .context("Could not find cache directory")?
            .join("querymt")
            .join("mcp-registries");

        std::fs::create_dir_all(&cache_dir)
            .context("Failed to create cache directory")?;

        let cache_path = cache_dir.join("registry.db");
        Self::new(cache_path, Some(Duration::from_secs(86_400)))
    }

    /// Initialize database schema with FTS5 support
    fn initialize_schema(&mut self) -> Result<()> {
        // Main servers table
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                registry_url TEXT NOT NULL,
                server_name TEXT NOT NULL,
                version TEXT NOT NULL,
                title TEXT,
                description TEXT NOT NULL,
                repository_url TEXT,
                repository_source TEXT,
                server_data TEXT NOT NULL,
                cached_at INTEGER NOT NULL,
                UNIQUE(registry_url, server_name, version)
            )",
            [],
        )?;

        // Create FTS5 virtual table for full-text search
        // We'll index name, title, and description
        self.conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS servers_fts USING fts5(
                server_name,
                title,
                description,
                content=servers,
                content_rowid=id
            )",
            [],
        )?;

        // Triggers to keep FTS5 index in sync with main table
        self.conn.execute(
            "CREATE TRIGGER IF NOT EXISTS servers_ai AFTER INSERT ON servers BEGIN
                INSERT INTO servers_fts(rowid, server_name, title, description)
                VALUES (new.id, new.server_name, new.title, new.description);
            END",
            [],
        )?;

        self.conn.execute(
            "CREATE TRIGGER IF NOT EXISTS servers_ad AFTER DELETE ON servers BEGIN
                DELETE FROM servers_fts WHERE rowid = old.id;
            END",
            [],
        )?;

        self.conn.execute(
            "CREATE TRIGGER IF NOT EXISTS servers_au AFTER UPDATE ON servers BEGIN
                UPDATE servers_fts SET
                    server_name = new.server_name,
                    title = new.title,
                    description = new.description
                WHERE rowid = new.id;
            END",
            [],
        )?;

        // Index for faster lookups by registry URL
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_registry_url ON servers(registry_url)",
            [],
        )?;

        // Index for faster lookups by cached_at (for TTL checks)
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_cached_at ON servers(cached_at)",
            [],
        )?;

        Ok(())
    }

    /// Get current timestamp in seconds since UNIX epoch
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Check if a timestamp is stale based on TTL
    fn is_stale(&self, cached_at: u64) -> bool {
        if let Some(ttl) = self.ttl {
            let now = Self::now();
            let age = Duration::from_secs(now.saturating_sub(cached_at));
            age > ttl
        } else {
            false // No TTL = never stale
        }
    }

    /// Cache a list of servers for a registry
    pub fn cache_servers(&mut self, registry_url: &str, servers: &[ServerResponse]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let now = Self::now();

        for server in servers {
            let server_data = serde_json::to_string(server)
                .context("Failed to serialize server data")?;

            tx.execute(
                "INSERT OR REPLACE INTO servers
                (registry_url, server_name, version, title, description, repository_url, repository_source, server_data, cached_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    registry_url,
                    server.server.name,
                    server.server.version,
                    server.server.title,
                    server.server.description,
                    server.server.repository.as_ref().map(|r| &r.url),
                    server.server.repository.as_ref().map(|r| &r.source),
                    server_data,
                    now,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Get all cached servers for a registry (respecting TTL)
    pub fn get_cached_servers(&self, registry_url: &str) -> Result<Vec<ServerResponse>> {
        let mut stmt = self.conn.prepare(
            "SELECT server_data, cached_at FROM servers WHERE registry_url = ?1"
        )?;

        let servers: Result<Vec<ServerResponse>> = stmt
            .query_map([registry_url], |row| {
                let data: String = row.get(0)?;
                let cached_at: u64 = row.get(1)?;
                Ok((data, cached_at))
            })?
            .filter_map(|result| {
                match result {
                    Ok((data, cached_at)) => {
                        // Skip stale entries
                        if self.is_stale(cached_at) {
                            return None;
                        }

                        match serde_json::from_str::<ServerResponse>(&data) {
                            Ok(server) => Some(Ok(server)),
                            Err(e) => Some(Err(anyhow::anyhow!("Failed to deserialize server: {}", e))),
                        }
                    }
                    Err(e) => Some(Err(anyhow::anyhow!("Database error: {}", e))),
                }
            })
            .collect();

        servers
    }

    /// Fuzzy search servers using FTS5 full-text search
    ///
    /// This searches across server name, title, and description fields
    /// and returns results ranked by relevance.
    ///
    /// # Arguments
    /// * `query` - Search query (supports FTS5 syntax like "word1 word2", "word*", etc.)
    /// * `registry_url` - Optional registry URL to filter results
    /// * `limit` - Maximum number of results to return
    pub fn search_servers(
        &self,
        query: &str,
        registry_url: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ServerResponse>> {
        // Build the FTS5 query - use simple match for now
        // Users can use FTS5 operators like: word*, "exact phrase", word1 OR word2
        let fts_query = query;

        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(url) = registry_url {
            let sql = format!(
                "SELECT s.server_data, s.cached_at, rank
                FROM servers_fts fts
                JOIN servers s ON fts.rowid = s.id
                WHERE fts.servers_fts MATCH ?1 AND s.registry_url = ?2
                ORDER BY rank
                {}",
                if let Some(lim) = limit {
                    format!("LIMIT {}", lim)
                } else {
                    String::new()
                }
            );
            (sql, vec![Box::new(fts_query.to_string()), Box::new(url.to_string())])
        } else {
            let sql = format!(
                "SELECT s.server_data, s.cached_at, rank
                FROM servers_fts fts
                JOIN servers s ON fts.rowid = s.id
                WHERE fts.servers_fts MATCH ?1
                ORDER BY rank
                {}",
                if let Some(lim) = limit {
                    format!("LIMIT {}", lim)
                } else {
                    String::new()
                }
            );
            (sql, vec![Box::new(fts_query.to_string())])
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

        let servers: Result<Vec<ServerResponse>> = stmt
            .query_map(&params_refs[..], |row| {
                let data: String = row.get(0)?;
                let cached_at: u64 = row.get(1)?;
                Ok((data, cached_at))
            })?
            .filter_map(|result| {
                match result {
                    Ok((data, cached_at)) => {
                        // Skip stale entries
                        if self.is_stale(cached_at) {
                            return None;
                        }

                        match serde_json::from_str::<ServerResponse>(&data) {
                            Ok(server) => Some(Ok(server)),
                            Err(e) => Some(Err(anyhow::anyhow!("Failed to deserialize server: {}", e))),
                        }
                    }
                    Err(e) => Some(Err(anyhow::anyhow!("Database error: {}", e))),
                }
            })
            .collect();

        servers
    }

    /// Simple substring search (fallback for when FTS5 query fails)
    pub fn simple_search(
        &self,
        query: &str,
        registry_url: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ServerResponse>> {
        let query_lower = query.to_lowercase();
        let pattern = format!("%{}%", query_lower);

        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(url) = registry_url {
            let sql = format!(
                "SELECT server_data, cached_at FROM servers
                WHERE registry_url = ?1 AND
                (LOWER(server_name) LIKE ?2 OR LOWER(description) LIKE ?2 OR LOWER(title) LIKE ?2)
                {}",
                if let Some(lim) = limit {
                    format!("LIMIT {}", lim)
                } else {
                    String::new()
                }
            );
            (sql, vec![Box::new(url.to_string()), Box::new(pattern)])
        } else {
            let sql = format!(
                "SELECT server_data, cached_at FROM servers
                WHERE LOWER(server_name) LIKE ?1 OR LOWER(description) LIKE ?1 OR LOWER(title) LIKE ?1
                {}",
                if let Some(lim) = limit {
                    format!("LIMIT {}", lim)
                } else {
                    String::new()
                }
            );
            (sql, vec![Box::new(pattern)])
        };

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

        let servers: Result<Vec<ServerResponse>> = stmt
            .query_map(&params_refs[..], |row| {
                let data: String = row.get(0)?;
                let cached_at: u64 = row.get(1)?;
                Ok((data, cached_at))
            })?
            .filter_map(|result| {
                match result {
                    Ok((data, cached_at)) => {
                        // Skip stale entries
                        if self.is_stale(cached_at) {
                            return None;
                        }

                        match serde_json::from_str::<ServerResponse>(&data) {
                            Ok(server) => Some(Ok(server)),
                            Err(e) => Some(Err(anyhow::anyhow!("Failed to deserialize server: {}", e))),
                        }
                    }
                    Err(e) => Some(Err(anyhow::anyhow!("Database error: {}", e))),
                }
            })
            .collect();

        servers
    }

    /// Get a specific cached server version
    pub fn get_cached_version(
        &self,
        registry_url: &str,
        server_name: &str,
        version: &str,
    ) -> Result<Option<ServerResponse>> {
        let result: Option<(String, u64)> = self
            .conn
            .query_row(
                "SELECT server_data, cached_at FROM servers
                WHERE registry_url = ?1 AND server_name = ?2 AND version = ?3",
                params![registry_url, server_name, version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        match result {
            Some((data, cached_at)) => {
                if self.is_stale(cached_at) {
                    return Ok(None);
                }

                let server = serde_json::from_str::<ServerResponse>(&data)
                    .context("Failed to deserialize server")?;
                Ok(Some(server))
            }
            None => Ok(None),
        }
    }

    /// Clear all cached data for a specific registry
    pub fn clear_registry_cache(&mut self, registry_url: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM servers WHERE registry_url = ?1",
            [registry_url],
        )?;
        Ok(())
    }

    /// Clear all cached data
    pub fn clear_all(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM servers", [])?;
        Ok(())
    }

    /// Clear stale entries based on TTL
    pub fn clear_stale(&mut self) -> Result<usize> {
        if let Some(ttl) = self.ttl {
            let cutoff = Self::now().saturating_sub(ttl.as_secs());
            let count = self.conn.execute(
                "DELETE FROM servers WHERE cached_at < ?1",
                [cutoff],
            )?;
            Ok(count)
        } else {
            Ok(0)
        }
    }

    /// Get cache statistics
    pub fn get_stats(&self) -> Result<CacheStats> {
        let total_servers: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM servers",
            [],
            |row| row.get(0),
        )?;

        let unique_registries: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT registry_url) FROM servers",
            [],
            |row| row.get(0),
        )?;

        let stale_count = if self.ttl.is_some() {
            let cutoff = Self::now().saturating_sub(self.ttl.unwrap().as_secs());
            self.conn.query_row(
                "SELECT COUNT(*) FROM servers WHERE cached_at < ?1",
                [cutoff],
                |row| row.get(0),
            )?
        } else {
            0
        };

        Ok(CacheStats {
            total_servers: total_servers as usize,
            unique_registries: unique_registries as usize,
            stale_entries: stale_count as usize,
        })
    }

    /// Get all unique server names from cache for autocomplete
    pub fn get_server_names(&self, registry_url: &str) -> Result<Vec<String>> {
        let cutoff = if let Some(ttl) = self.ttl {
            Some(Self::now().saturating_sub(ttl.as_secs()))
        } else {
            None
        };

        let query = if cutoff.is_some() {
            "SELECT DISTINCT server_name FROM servers
             WHERE registry_url = ?1 AND cached_at >= ?2
             ORDER BY server_name"
        } else {
            "SELECT DISTINCT server_name FROM servers
             WHERE registry_url = ?1
             ORDER BY server_name"
        };

        let mut stmt = self.conn.prepare(query)?;

        let names: Vec<String> = if let Some(cutoff_time) = cutoff {
            stmt.query_map(params![registry_url, cutoff_time], |row| {
                row.get(0)
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![registry_url], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?
        };

        Ok(names)
    }
}

/// Statistics about cached data
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub total_servers: usize,
    pub unique_registries: usize,
    pub stale_entries: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::mcp::registry::{
        OfficialMetadata, RegistryMetadata, ServerDetail, ServerResponse,
    };
    use tempfile::NamedTempFile;

    fn create_test_server(name: &str, description: &str) -> ServerResponse {
        ServerResponse {
            server: ServerDetail {
                schema: None,
                name: name.to_string(),
                title: Some(format!("{} Title", name)),
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
    fn test_cache_and_retrieve() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut cache = RegistryCache::new(
            temp_file.path().to_path_buf(),
            Some(Duration::from_secs(60)),
        )
        .unwrap();

        let servers = vec![
            create_test_server("filesystem", "A filesystem server"),
            create_test_server("database", "A database server"),
        ];

        cache
            .cache_servers("https://test.registry.com", &servers)
            .unwrap();

        let cached = cache
            .get_cached_servers("https://test.registry.com")
            .unwrap();

        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].server.name, "filesystem");
    }

    #[test]
    fn test_fts_search() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut cache = RegistryCache::new(
            temp_file.path().to_path_buf(),
            Some(Duration::from_secs(60)),
        )
        .unwrap();

        let servers = vec![
            create_test_server("filesystem", "Access local filesystem"),
            create_test_server("postgres", "PostgreSQL database connector"),
            create_test_server("sqlite", "SQLite database support"),
        ];

        cache
            .cache_servers("https://test.registry.com", &servers)
            .unwrap();

        // Search for "database" - should match 2 servers
        let results = cache
            .search_servers("database", Some("https://test.registry.com"), None)
            .unwrap();

        assert_eq!(results.len(), 2);

        // Search for "filesystem"
        let results = cache
            .search_servers("filesystem", Some("https://test.registry.com"), None)
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].server.name, "filesystem");
    }

    #[test]
    fn test_ttl_expiration() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut cache = RegistryCache::new(
            temp_file.path().to_path_buf(),
            Some(Duration::from_secs(0)), // Immediate expiration
        )
        .unwrap();

        let servers = vec![create_test_server("test", "Test server")];

        cache
            .cache_servers("https://test.registry.com", &servers)
            .unwrap();

        // Should be empty due to immediate expiration
        std::thread::sleep(Duration::from_millis(10));
        let cached = cache
            .get_cached_servers("https://test.registry.com")
            .unwrap();

        assert_eq!(cached.len(), 0);
    }
}
