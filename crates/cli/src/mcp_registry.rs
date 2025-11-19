use anyhow::Result;
use querymt::mcp::{
    config::RegistryConfig,
    registry::{RegistryClient, ServerResponse},
};
use crate::mcp_cache::RegistryCache;

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
    let cache_dir = dirs::cache_dir()
        .map(|mut p| {
            p.push("querymt");
            p.push("mcp-registries");
            p
        })
        .ok_or_else(|| anyhow::anyhow!("Could not find cache directory"))?;

    std::fs::create_dir_all(&cache_dir)?;

    let cache_path = cache_dir.join("registry.db");
    let ttl = config.cache_ttl_hours.map(|hours| std::time::Duration::from_secs(hours * 3600));

    RegistryCache::new(cache_path, ttl)
}

/// Handle the list command with interactive pagination
pub async fn handle_list(
    registry_url: Option<String>,
    no_cache: bool,
    refresh: bool,
) -> Result<()> {
    let config = get_registry_config(registry_url);
    let client = RegistryClient::new(config.url.clone());

    if refresh {
        // Refresh mode: fetch everything and populate cache
        return handle_refresh_cache(&config, &client).await;
    }

    if !no_cache && config.use_cache {
        // Try to serve from cache
        let cache = create_cache(&config)?;

        match cache.get_cached_servers(&config.url) {
            Ok(cached_servers) if !cached_servers.is_empty() => {
                // Serve from cache with interactive pagination
                display_servers_paginated(&cached_servers)?;
                return Ok(());
            }
            _ => {
                // Cache miss or empty, fall through to fetch
                log::info!("Cache miss or empty, fetching from registry");
            }
        }
    }

    // Fetch with interactive pagination (no cache or cache miss)
    fetch_and_display_interactive(&client, &config, no_cache).await
}

/// Refresh the entire cache by fetching all servers
async fn handle_refresh_cache(
    config: &querymt::mcp::config::RegistryConfig,
    client: &RegistryClient,
) -> Result<()> {
    use colored::Colorize;
    use spinners::{Spinner, Spinners};

    let mut sp = Spinner::new(
        Spinners::Dots12,
        "Refreshing cache, fetching all servers...".bright_blue().to_string(),
    );

    let mut all_servers = Vec::new();
    let mut cursor = None;
    const PAGE_SIZE: u32 = 100;

    loop {
        let response = client
            .list_servers(Some(PAGE_SIZE), cursor, None, None, None)
            .await?;

        let fetched_count = response.servers.len();
        all_servers.extend(response.servers);

        // Update spinner message with progress
        sp.stop();
        sp = Spinner::new(
            Spinners::Dots12,
            format!("Fetching servers... ({})", all_servers.len()).bright_blue().to_string(),
        );

        if response.metadata.next_cursor.is_none() || fetched_count == 0 {
            break;
        }

        cursor = response.metadata.next_cursor;
    }

    sp.stop();

    // Update cache with all servers
    if config.use_cache {
        let mut cache = create_cache(config)?;
        cache.cache_servers(&config.url, &all_servers)?;
        println!("{}", format!("✓ Cache updated with {} servers", all_servers.len()).bright_green());
    }

    // Display with pagination
    display_servers_paginated(&all_servers)?;

    Ok(())
}

/// Fetch and display servers with interactive pagination
async fn fetch_and_display_interactive(
    client: &RegistryClient,
    config: &querymt::mcp::config::RegistryConfig,
    no_cache: bool,
) -> Result<()> {
    use colored::Colorize;
    use std::io::{self, Write};

    const PAGE_SIZE: u32 = 50;
    let mut all_servers = Vec::new();
    let mut cursor = None;

    loop {
        // Fetch next page
        let response = client
            .list_servers(Some(PAGE_SIZE), cursor, None, None, None)
            .await?;

        let fetched_count = response.servers.len();
        cursor = response.metadata.next_cursor.clone();

        all_servers.extend(response.servers);

        // Display current batch
        display_servers_table(&all_servers[(all_servers.len() - fetched_count)..]);

        // Check if there are more results
        if cursor.is_none() || fetched_count == 0 {
            println!("\n{}", "End of results".bright_black());
            break;
        }

        // Ask user if they want to continue (single keypress)
        print!("\n{} ", format!("Showing {} of more servers. Continue? [c/q/Esc]:", all_servers.len()).bright_yellow());
        io::stdout().flush()?;

        if !wait_for_continue()? {
            println!("\n{}", "Stopped listing".bright_black());
            break;
        }
    }

    // Cache all fetched servers if caching is enabled
    if !no_cache && config.use_cache {
        let mut cache = create_cache(config)?;
        let _ = cache.cache_servers(&config.url, &all_servers);
        println!("{}", format!("Cached {} servers", all_servers.len()).bright_black());
    }

    Ok(())
}

/// Display servers with pagination (for already-loaded data)
fn display_servers_paginated(servers: &[ServerResponse]) -> Result<()> {
    use colored::Colorize;
    use std::io::{self, Write};

    const PAGE_SIZE: usize = 50;
    let total = servers.len();

    for (page_num, chunk) in servers.chunks(PAGE_SIZE).enumerate() {
        let start = page_num * PAGE_SIZE;
        let end = (start + chunk.len()).min(total);

        display_servers_table(chunk);

        if end < total {
            print!("\n{} ", format!("Showing {}-{} of {}. Continue? [c/q/Esc]:", start + 1, end, total).bright_yellow());
            io::stdout().flush()?;

            if !wait_for_continue()? {
                println!("\n{}", "Stopped listing".bright_black());
                break;
            }
        } else {
            println!("\n{}", format!("Showing all {} servers", total).bright_green());
        }
    }

    Ok(())
}

/// Wait for user to press a key (c to continue, q/Esc to quit)
/// Returns true to continue, false to quit
fn wait_for_continue() -> Result<bool> {
    use std::io::{self, Read};

    // Try to enable raw mode for single keypress
    // If it fails, fall back to line input
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let stdin_fd = io::stdin().as_raw_fd();

        // Try to get terminal attributes
        let original_termios = match termios::Termios::from_fd(stdin_fd) {
            Ok(t) => t,
            Err(_) => {
                // Fallback to line input
                return fallback_wait_for_continue();
            }
        };

        // Set raw mode
        let mut raw = original_termios.clone();
        raw.c_lflag &= !(termios::ICANON | termios::ECHO);
        if termios::tcsetattr(stdin_fd, termios::TCSANOW, &raw).is_err() {
            return fallback_wait_for_continue();
        }

        // Read single character
        let mut buffer = [0u8; 1];
        let result = io::stdin().read_exact(&mut buffer);

        // Restore terminal
        let _ = termios::tcsetattr(stdin_fd, termios::TCSANOW, &original_termios);

        match result {
            Ok(_) => {
                let ch = buffer[0] as char;
                // c, C, Enter, Space = continue
                // q, Q, Esc = quit
                Ok(ch != 'q' && ch != 'Q' && ch as u8 != 27)
            }
            Err(_) => Ok(false),
        }
    }

    #[cfg(not(unix))]
    {
        fallback_wait_for_continue()
    }
}

/// Fallback to line-based input if raw mode isn't available
fn fallback_wait_for_continue() -> Result<bool> {
    use std::io;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    Ok(input != "q" && input != "quit")
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

    // Use FTS5 fuzzy search on cached data, or fall back to API search
    let servers = if !no_cache && config.use_cache {
        let cache = create_cache(&config)?;

        // Try FTS5 full-text search first
        match cache.search_servers(&query, Some(&config.url), None) {
            Ok(results) if !results.is_empty() => results,
            _ => {
                // Try simple substring search as fallback
                match cache.simple_search(&query, Some(&config.url), None) {
                    Ok(results) if !results.is_empty() => results,
                    _ => {
                        // Cache miss or no results - fetch from API and update cache
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

                        // Update cache with fetched results
                        if !all_servers.is_empty() {
                            let mut cache_mut = cache;
                            let _ = cache_mut.cache_servers(&config.url, &all_servers);
                        }

                        all_servers
                    }
                }
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

        match cache.get_cached_version(&config.url, &server_id, version_str)? {
            Some(v) => v,
            None => {
                let v = client.get_server_version(&server_id, version_str).await?;
                // Update cache with the fetched version
                let mut cache_mut = cache;
                let _ = cache_mut.cache_servers(&config.url, &[v.clone()]);
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
    let mut cache = create_cache(&RegistryConfig::default())?;

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
