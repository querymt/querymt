# Plugin Configuration

QueryMT uses a single configuration file to discover and manage all types of plugins, whether they are Native (shared libraries) or Extism (Wasm). This file tells QueryMT where to find the plugin artifacts and any specific settings they require.

## Configuration File Format

The configuration can be in TOML, JSON, or YAML format. By default, QueryMT looks for a file named `plugins.toml`. The host determines the plugin type automatically based on the file extension (`.wasm`, `.so`, `.dll`, `.dylib`) or from metadata within OCI images.

### TOML Example (`plugins.toml`)

This example shows the configuration for both an Extism/Wasm plugin and a Native plugin.

```toml
# Optional: Configuration for OCI image downloader
[oci]
insecure_skip_signature = false # Set to true to disable signature verification (not recommended for production)
use_sigstore_tuf_data = true    # Use official Sigstore TUF data for verification
# cert_email = "signer@example.com"
# cert_issuer = "https://github.com/login/oauth"
# rekor_pub_keys = "/path/to/rekor.pub"
# fulcio_certs = "/path/to/fulcio.crt"

# --- List of provider plugins ---

# Example 1: An Extism (Wasm) plugin loaded from a local file
[[providers]]
name = "my_openai_wasm_plugin"
path = "/path/to/openai_plugin.wasm"
# Optional plugin-specific configuration
[providers.config]
api_key_env = "MY_OPENAI_API_KEY"
model = "gpt-4"
timeout_ms = 30000

# Example 2: A Native (shared library) plugin
[[providers]]
name = "my_anthropic_native_plugin"
path = "/path/to/anthropic_plugin.so" # On Linux. Use .dll on Windows, .dylib on macOS
[providers.config]
api_key_env = "MY_ANTHROPIC_API_KEY"
model = "claude-3-opus-20240229"

# Example 3: An Extism (Wasm) plugin from an OCI registry
[[providers]]
name = "secure_oci_plugin"
path = "oci://ghcr.io/my-org/my-plugin:latest"
[providers.config]
# Config specific to this OCI plugin
```

## Configuration Fields

### Root Level

-   `oci` (Optional, Object): Configuration for downloading and verifying plugins from OCI registries. See [OCI Plugins](oci_plugins.md) for details.

### `providers` (Array of Objects)

Each object in the `providers` array defines a single plugin instance:

-   `name` (String, Required): A unique identifier for this plugin instance. This name is used to select the provider in QueryMT's `LLMBuilder`.
-   `path` (String, Required): Specifies the location of the plugin module. It can be:
    -   A local file system path to a Wasm module (e.g., `/path/to/plugin.wasm`).
    -   A local file system path to a native shared library (e.g., `/path/to/plugin.so`, `C:\plugins\plugin.dll`).
    -   An HTTP/HTTPS URL (e.g., `https://example.com/plugin.wasm`).
    -   An OCI image reference (e.g., `oci://ghcr.io/user/plugin:latest`).
-   `config` (Object, Optional): A TOML table (or JSON object/YAML map) containing plugin-specific configuration. The structure of this object must match what the plugin expects, which can be validated against the plugin's `config_schema()`. This configuration is passed to the plugin during initialization.
