use anyhow::Result;
use querymt::mcp::{
    cache::RegistryCache,
    config::RegistryConfig,
    registry::{RegistryClient, ServerResponse},
};
use std::time::Duration;

/// Load registry configuration (use provided URL or default)
fn get_registry_config(registry_url: Option<String>) -> RegistryConfig {
    match registry_url {
        Some(url) => RegistryConfig {
            url,
            use_cache: true,
            cache_ttl_hours: Some(24),
        },
        None => RegistryConfig::default(),
    }
}

/// Create a cache instance based on registry config
fn create_cache(config: &RegistryConfig) -> Result<RegistryCache> {
    match config.cache_ttl_hours {
        Some(hours) => {
            let cache_dir = dirs::cache_dir()
                .map(|mut p| {
                    p.push("querymt");
                    p.push("mcp-registries");
                    p
                })
                .ok_or_else(|| anyhow::anyhow!("Could not find cache directory"))?;

            std::fs::create_dir_all(&cache_dir)?;
            Ok(RegistryCache::new(
                cache_dir,
                Some(Duration::from_secs(hours * 3600)),
            ))
        }
        None => {
            let cache_dir = dirs::cache_dir()
                .map(|mut p| {
                    p.push("querymt");
                    p.push("mcp-registries");
                    p
                })
                .ok_or_else(|| anyhow::anyhow!("Could not find cache directory"))?;

            std::fs::create_dir_all(&cache_dir)?;
            Ok(RegistryCache::permanent_cache(cache_dir))
        }
    }
}

/// Handle the list command with automatic pagination
pub async fn handle_list(
    registry_url: Option<String>,
    limit: u32,
    no_cache: bool,
) -> Result<()> {
    let config = get_registry_config(registry_url);
    let client = RegistryClient::new(config.url.clone());

    let servers = if !no_cache && config.use_cache {
        let cache = create_cache(&config)?;

        // Try cache first
        match cache.get_cached_servers(&config.url) {
            Ok(cached_servers) => {
                // Return cached results, truncated to limit
                cached_servers.into_iter().take(limit as usize).collect()
            }
            Err(_) => {
                // Cache miss or stale, fetch from registry with pagination
                fetch_servers_with_pagination(&client, limit).await?
            }
        }
    } else {
        // No caching, fetch directly with pagination
        fetch_servers_with_pagination(&client, limit).await?
    };

    display_servers_table(&servers);

    Ok(())
}

/// Fetch servers with automatic pagination until reaching the desired limit
async fn fetch_servers_with_pagination(
    client: &RegistryClient,
    total_limit: u32,
) -> Result<Vec<ServerResponse>> {
    let mut all_servers = Vec::new();
    let mut cursor = None;
    const PAGE_SIZE: u32 = 100; // Fetch 100 at a time for efficiency

    while (all_servers.len() as u32) < total_limit {
        // Calculate how many more we need
        let remaining = total_limit - (all_servers.len() as u32);
        let fetch_limit = remaining.min(PAGE_SIZE);

        let response = client
            .list_servers(Some(fetch_limit), cursor, None, None, None)
            .await?;

        let fetched_count = response.servers.len();
        all_servers.extend(response.servers);

        // Check if we've reached the end or got enough results
        if response.metadata.next_cursor.is_none() || fetched_count == 0 {
            break;
        }

        cursor = response.metadata.next_cursor;
    }

    // Truncate to exact limit if we fetched more
    all_servers.truncate(total_limit as usize);

    Ok(all_servers)
}

/// Handle the search command
pub async fn handle_search(
    query: String,
    registry_url: Option<String>,
    no_cache: bool,
) -> Result<()> {
    let config = get_registry_config(registry_url);
    let client = RegistryClient::new(config.url.clone());

    // Use the API's search parameter for efficient server-side filtering
    let servers = if !no_cache && config.use_cache {
        let cache = create_cache(&config)?;

        match cache.get_cached_servers(&config.url) {
            Ok(servers) => {
                // Filter cached servers locally
                let query_lower = query.to_lowercase();
                servers
                    .into_iter()
                    .filter(|s| {
                        s.server.name.to_lowercase().contains(&query_lower)
                            || s.server.description.to_lowercase().contains(&query_lower)
                    })
                    .collect()
            }
            Err(_) => {
                // Cache miss - use API search
                let mut all_servers = Vec::new();
                let mut cursor = None;

                // Fetch all pages with search filter
                loop {
                    let response = client
                        .list_servers(Some(100), cursor, Some(query.clone()), None, None)
                        .await?;
                    all_servers.extend(response.servers);

                    if response.metadata.next_cursor.is_none() {
                        break;
                    }
                    cursor = response.metadata.next_cursor;
                }

                all_servers
            }
        }
    } else {
        // No caching - use API search directly
        let mut all_servers = Vec::new();
        let mut cursor = None;

        loop {
            let response = client
                .list_servers(Some(100), cursor, Some(query.clone()), None, None)
                .await?;
            all_servers.extend(response.servers);

            if response.metadata.next_cursor.is_none() {
                break;
            }
            cursor = response.metadata.next_cursor;
        }

        all_servers
    };

    if servers.is_empty() {
        println!("No servers found matching '{}'", query);
    } else {
        println!("Found {} server(s) matching '{}':\n", servers.len(), query);
        display_servers_table(&servers);
    }

    Ok(())
}

/// Handle the info command
pub async fn handle_info(
    server_id: String,
    version: Option<String>,
    registry_url: Option<String>,
    no_cache: bool,
) -> Result<()> {
    let config = get_registry_config(registry_url);
    let client = RegistryClient::new(config.url.clone());
    let version_str = version.as_deref().unwrap_or("latest");

    let server_version = if !no_cache && config.use_cache {
        let cache = create_cache(&config)?;

        match cache.get_cached_version(&config.url, &server_id, version_str) {
            Ok(v) => v,
            Err(_) => {
                let v = client.get_server_version(&server_id, version_str).await?;
                let _ = cache.cache_version(&config.url, &server_id, version_str, v.clone());
                v
            }
        }
    } else {
        client.get_server_version(&server_id, version_str).await?
    };

    display_server_info(&server_id, &server_version);

    Ok(())
}

/// Handle the refresh command
pub async fn handle_refresh(registry_url: Option<String>) -> Result<()> {
    let cache = create_cache(&RegistryConfig::default())?;

    if let Some(url) = registry_url {
        println!("Clearing cache for registry: {}", url);
        cache.clear_registry_cache(&url)?;
        println!("Cache cleared successfully.");
    } else {
        println!("Clearing all registry caches...");
        cache.clear_all()?;
        println!("All caches cleared successfully.");
    }

    Ok(())
}

/// Display servers in a formatted table
fn display_servers_table(servers: &[ServerResponse]) {
    if servers.is_empty() {
        println!("No servers found.");
        return;
    }

    println!("{:<50} {:<15} {:<30}", "Name", "Version", "Description");
    println!("{}", "-".repeat(95));

    for server in servers {
        println!(
            "{:<50} {:<15} {:<30}",
            truncate(&server.server.name, 50),
            server.server.version,
            truncate(&server.server.description, 30)
        );
    }

    println!("\nTotal: {} server(s)", servers.len());
}

/// Display detailed information about a specific server version
fn display_server_info(server_id: &str, response: &ServerResponse) {
    println!("Server: {}", server_id);
    println!("Name: {}", response.server.name);
    println!("Version: {}", response.server.version);
    println!("Description: {}", response.server.description);

    if let Some(title) = &response.server.title {
        println!("Title: {}", title);
    }

    if let Some(repo) = &response.server.repository {
        println!("\nRepository:");
        println!("  URL: {}", repo.url);
        println!("  Source: {}", repo.source);
    }

    if !response.server.packages.is_empty() {
        println!("\nPackages:");
        for (idx, pkg) in response.server.packages.iter().enumerate() {
            println!("  Package {}:", idx + 1);
            println!("    Type: {:?}", pkg.registry_type);
            println!("    Identifier: {}", pkg.identifier);
            println!("    Transport: {}", pkg.transport.transport_type);

            if let Some(args) = &pkg.args {
                println!("    Arguments: {}", args.join(" "));
            }

            if let Some(sha256) = &pkg.sha256 {
                println!("    SHA-256: {}", sha256);
            }

            if let Some(runtime) = &pkg.runtime {
                println!("    Runtime: {}", runtime);
            }

            if let Some(env_vars) = &pkg.environment_variables {
                if !env_vars.is_empty() {
                    println!("    Environment Variables:");
                    for env_var in env_vars {
                        print!("      {}", env_var.name);
                        if let Some(desc) = &env_var.description {
                            print!(" - {}", desc);
                        }
                        println!();
                        if env_var.is_secret {
                            println!("        (secret)");
                        }
                        if env_var.is_required {
                            println!("        (required)");
                        }
                    }
                }
            }
        }
    }

    if let Some(meta) = &response.meta.official {
        println!("\nRegistry Metadata:");
        println!("  Status: {}", meta.status);
        println!("  Published: {}", meta.published_at);
        println!("  Updated: {}", meta.updated_at);
        println!("  Is Latest: {}", meta.is_latest);
    }
}

/// Truncate a string to a maximum length with ellipsis (UTF-8 safe)
fn truncate(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}
