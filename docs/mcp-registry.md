# MCP Registry Support

QueryMT now supports the Model Context Protocol (MCP) registry, allowing you to easily discover and use MCP servers from the official registry or custom registries.

## Features

- **Registry Discovery**: Browse and search available MCP servers from any registry following the MCP API spec
- **Automatic Resolution**: Reference servers by their registry ID instead of manual configuration
- **Version Management**: Use `latest` or pin to specific versions
- **Configurable Registry**: Support for custom registries beyond the official one
- **Optional Caching**: Cache registry data with configurable TTL for offline support
- **Multiple Package Types**: Support for npm, PyPI, and binary packages

## CLI Commands

### List Servers

List all available servers from the registry:

```bash
# List from default registry
qmt mcp registry list

# List with pagination
qmt mcp registry list --limit 20

# List from custom registry
qmt mcp registry list --registry-url https://my-custom-registry.com

# List without caching
qmt mcp registry list --no-cache
```

### Search Servers

Search for servers by keyword:

```bash
# Search for filesystem-related servers
qmt mcp registry search filesystem

# Search in custom registry
qmt mcp registry search database --registry-url https://my-custom-registry.com
```

### Server Information

Get detailed information about a specific server:

```bash
# Get latest version info
qmt mcp registry info @modelcontextprotocol/server-filesystem

# Get specific version info
qmt mcp registry info @modelcontextprotocol/server-filesystem --version 0.5.1

# Get info from custom registry
qmt mcp registry info @mycompany/custom-server --registry-url https://corporate-registry.com
```

### Refresh Cache

Clear and refresh the registry cache:

```bash
# Refresh all caches
qmt mcp registry refresh

# Refresh specific registry cache
qmt mcp registry refresh --registry-url https://registry.modelcontextprotocol.io
```

## Configuration

### Using Registry Servers

You can now reference servers from the registry in your MCP configuration file:

```toml
# Global registry configuration (optional)
[registry]
url = "https://registry.modelcontextprotocol.io"  # Default
use_cache = true
cache_ttl_hours = 24

# Registry-sourced server
[[mcp]]
name = "filesystem"
source = "registry"
registry_id = "@modelcontextprotocol/server-filesystem"
version = "latest"  # or specific version like "0.5.1"

# Registry server with environment overrides
[[mcp]]
name = "github"
source = "registry"
registry_id = "@modelcontextprotocol/server-github"
version = "latest"

[mcp.env_overrides]
GITHUB_TOKEN = "your-token-here"

# Traditional direct configuration (still supported)
[[mcp]]
name = "custom-server"
source = "direct"
protocol = "stdio"
command = "/usr/local/bin/my-server"
args = ["--debug"]
```

### Custom Registry

Use a custom registry that follows the MCP API specification:

```toml
[registry]
url = "https://my-custom-registry.example.com"
use_cache = true
cache_ttl_hours = 48

[[mcp]]
name = "internal-tool"
source = "registry"
registry_id = "@mycompany/internal-server"
version = "1.2.3"
```

### Per-Server Registry Override

Override the global registry configuration for specific servers:

```toml
[[mcp]]
name = "corporate-tool"
source = "registry"
registry_id = "@corporate/special-server"
version = "latest"

# Override registry for this server only
[mcp.registry_config]
url = "https://corporate-registry.example.com"
use_cache = false  # Always fetch fresh
```

### Disable Caching

Disable caching globally or per-command:

```toml
[registry]
url = "https://registry.modelcontextprotocol.io"
use_cache = false  # Never cache
```

Or via CLI:

```bash
qmt mcp registry list --no-cache
```

## Package Type Support

The registry integration supports multiple package types:

### NPM Packages

NPM packages are automatically executed using `npx`:

```toml
[[mcp]]
name = "filesystem"
source = "registry"
registry_id = "@modelcontextprotocol/server-filesystem"
version = "latest"
# Will resolve to: npx -y @modelcontextprotocol/server-filesystem
```

### PyPI Packages

Python packages use the command specified in the registry:

```toml
[[mcp]]
name = "python-server"
source = "registry"
registry_id = "@example/python-mcp-server"
version = "0.1.0"
# Will use the command from registry metadata
```

### Binary Packages

Direct binary execution:

```toml
[[mcp]]
name = "binary-server"
source = "registry"
registry_id = "@example/binary-server"
version = "1.0.0"
# Will execute the binary specified in registry
```

## Cache Location

Registry data is cached in the system cache directory:

- **Linux**: `~/.cache/querymt/mcp-registries/`
- **macOS**: `~/Library/Caches/querymt/mcp-registries/`
- **Windows**: `%LOCALAPPDATA%\querymt\mcp-registries\`

Cache files include:
- Server lists (per registry)
- Individual server version metadata

## Architecture

The MCP registry support is implemented in several modules:

### Core Library (`crates/querymt/src/mcp/`)

- **`registry.rs`**: HTTP client for any MCP-compatible registry
  - `RegistryClient`: REST API client
  - `ServersResponse`, `ServerVersion`: Data models
  - Support for pagination and URL encoding

- **`cache.rs`**: Optional caching system
  - `RegistryCache`: TTL-based cache manager
  - Configurable expiration
  - Per-registry and per-version caching

- **`config.rs`**: Enhanced configuration
  - `RegistryConfig`: Global registry settings
  - `McpServerSource`: Direct vs Registry sources
  - Automatic resolution in `create_mcp_clients()`

### CLI (`crates/cli/src/`)

- **`cli_args.rs`**: Command-line argument structure
  - `McpCommands` and `RegistryCommands` enums

- **`mcp_registry.rs`**: CLI command handlers
  - List, search, info, and refresh operations
  - Pretty-printed output

## Examples

### Basic Usage

```bash
# Discover available servers
qmt mcp registry list

# Find what you need
qmt mcp registry search "filesystem"

# Get details
qmt mcp registry info @modelcontextprotocol/server-filesystem

# Add to config
cat > mcp_config.toml <<EOF
[[mcp]]
name = "fs"
source = "registry"
registry_id = "@modelcontextprotocol/server-filesystem"
version = "latest"
EOF

# Use it
qmt --mcp-config mcp_config.toml "List files in my home directory"
```

### Advanced Configuration

```toml
# Multiple registries
[registry]
url = "https://registry.modelcontextprotocol.io"
use_cache = true
cache_ttl_hours = 24

[[mcp]]
name = "public-server"
source = "registry"
registry_id = "@modelcontextprotocol/server-filesystem"
version = "latest"

[[mcp]]
name = "corporate-server"
source = "registry"
registry_id = "@mycompany/internal-server"
version = "2.1.0"

[mcp.registry_config]
url = "https://corporate-registry.mycompany.com"
cache_ttl_hours = 1  # Shorter TTL for internal registry

[[mcp]]
name = "local-dev"
source = "direct"
protocol = "stdio"
command = "/Users/dev/my-mcp-server/target/debug/server"
```

## Backward Compatibility

All existing MCP configurations continue to work without modification. The registry feature is additive:

```toml
# Old format (still works)
[[mcp]]
name = "my-server"
protocol = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]

# New format (equivalent)
[[mcp]]
name = "my-server"
source = "registry"
registry_id = "@modelcontextprotocol/server-filesystem"
version = "latest"
```

To maintain compatibility, specify `source = "direct"`:

```toml
[[mcp]]
name = "my-server"
source = "direct"
protocol = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]
```

## Troubleshooting

### Registry Connection Issues

```bash
# Test registry connectivity
qmt mcp registry list --no-cache --registry-url https://registry.modelcontextprotocol.io

# Check cache status
ls -lah ~/Library/Caches/querymt/mcp-registries/  # macOS
```

### Clear Cache

```bash
# Clear all caches
qmt mcp registry refresh

# Clear specific registry
qmt mcp registry refresh --registry-url https://registry.modelcontextprotocol.io
```

### Debugging

Enable debug logging:

```bash
RUST_LOG=debug qmt mcp registry list
```

## API Reference

For implementing custom registries, see the [MCP Registry API Specification](https://github.com/modelcontextprotocol/registry/blob/main/docs/guides/consuming/use-rest-api.md).

Required endpoints:
- `GET /v0.1/servers` - List servers with pagination
- `GET /v0.1/servers/{serverName}/versions` - List versions
- `GET /v0.1/servers/{serverName}/versions/{version}` - Get specific version

## Contributing

To add support for additional package types or registry features, see:
- `crates/querymt/src/mcp/registry.rs` - Registry client
- `crates/querymt/src/mcp/config.rs` - `server_version_to_transport()` method
