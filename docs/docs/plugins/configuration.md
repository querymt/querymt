# Host Configuration

QueryMT uses a configuration file to discover and manage Extism plugins. This file tells QueryMT where to find plugin Wasm modules and any specific settings they require.

## Configuration File Format

The configuration can be in TOML, JSON, or YAML format. By default, QueryMT might look for a file like `extism_plugins.toml`.

### TOML Example (`extism_plugins.toml`)

```toml
# Optional: Configuration for OCI image downloader
[oci]
insecure_skip_signature = false # Set to true to disable signature verification (not recommended for production)
use_sigstore_tuf_data = true    # Use official Sigstore TUF data for verification
# cert_email = "signer@example.com"
# cert_issuer = "https://github.com/login/oauth"
# rekor_pub_keys = "/path/to/rekor.pub"
# fulcio_certs = "/path/to/fulcio.crt"

# List of provider plugins
[[providers]]
name = "my_openai_plugin" # A unique name for this provider instance
path = "/path/to/openai_plugin.wasm" # Path to local Wasm file
# Optional plugin-specific configuration
[providers.config]
api_key_env = "MY_OPENAI_API_KEY" # Env var name for API key
model = "gpt-4"
timeout_ms = 30000

[[providers]]
name = "another_provider_http"
path = "http://example.com/plugins/another_plugin.wasm" # URL to Wasm file
[providers.config]
custom_param = "value"

[[providers]]
name = "secure_oci_plugin"
path = "oci://ghcr.io/my-org/my-plugin:latest" # OCI image reference
[providers.config]
# Config specific to this OCI plugin
```

## Configuration Fields

### Root Level

- `oci` (Optional, Object): Configuration for the OCI image downloader. See [OCI Plugins](oci_plugins.md) for details.

### `providers` (Array of Objects)

Each object in the `providers` array defines a single plugin instance:

- `name` (String, Required): A unique identifier for this plugin instance. This name is used to select the provider in QueryMT.
- `path` (String, Required): Specifies the location of the Wasm module. It can be:
    - A local file system path (e.g., `/path/to/plugin.wasm`).
    - An HTTP/HTTPS URL (e.g., `https://example.com/plugin.wasm`).
    - An OCI image reference (e.g., `oci://docker.io/user/plugin:tag` or `oci://ghcr.io/user/plugin:latest`).
- `config` (Object, Optional): A TOML table (or JSON object/YAML map) containing plugin-specific configuration. The structure of this object must match the schema defined by the plugin's `config_schema()` export. This configuration is passed to the plugin during initialization and on relevant function calls.

### Example: Plugin-Specific Configuration

If a plugin's `config_schema()` defines that it accepts an `api_url` and `default_model`, its `config` section might look like:

```toml
# In extism_plugins.toml, for a specific provider
[providers.config]
api_url = "https://api.customllm.com/v1"
default_model = "model-x"
```

The plugin developer documentation should specify the required and optional fields for its `config` block.
