# OCI Plugins

QueryMT supports loading plugins distributed as OCI (Open Container Initiative) images from container registries like Docker Hub, GitHub Container Registry (GHCR), etc. This provides a standardized way to package and distribute plugins.

## Distributing Native and Wasm Plugins

OCI registries can be used to distribute both Native and Wasm plugins. The QueryMT host determines which type of plugin an image contains using the following logic:

1.  **Platform Matching**: It first checks the image index for a manifest matching the host's operating system and architecture (e.g., `linux/amd64`). If a match is found, it's treated as a **Native Plugin**.
2.  **Wasm Fallback**: If no native platform matches, it looks for a `wasi/wasm` platform. If found, it's treated as an **Extism (Wasm) Plugin**.
3.  **Layer Type/Annotations**: As a fallback, it can inspect layer media types (`application/vnd.oci.image.layer.v1.tar+gzip` for native, `application/vnd.wasm.v1.layer+wasm` for Wasm) or an annotation (`mt.query.plugin.type`) to determine the type.

When publishing, ensure your image is built for the correct target platform(s).

## Configuration

To use an OCI plugin, specify its path in the `extism_plugins.toml` (or equivalent JSON/YAML) configuration file using the `oci://` prefix:

```toml
[[providers]]
name = "my_oci_plugin"
path = "oci://ghcr.io/my-org/my-plugin:latest" # OCI image reference
[providers.config]
# Plugin-specific configuration
# ...
```

## Plugin Caching

When QueryMT encounters an `oci://` path, it will:
1.  Parse the image reference (e.g., `ghcr.io/my-org/my-plugin:latest`).
2.  Generate a cache key based on the image reference and plugin name.
3.  Check if the plugin Wasm file already exists in the local cache directory (typically `~/.cache/querymt/` or platform equivalent).
4.  If not cached:
    a.  Pull the OCI image layers.
    b.  Attempt to extract a file named `plugin.wasm` (or a configured target file path, though `plugin.wasm` is the default assumption for generic OCI plugins) from the image layers.
    c.  Store the extracted Wasm file in the cache directory.
5.  Load the Wasm plugin from the cached file.

This means subsequent loads of the same plugin version will be much faster as they use the local cache.

## Signature Verification (Sigstore Cosign)

For enhanced security, QueryMT can verify OCI image signatures using [Sigstore Cosign](https://www.sigstore.dev/) before pulling and extracting the plugin. This helps ensure the plugin's authenticity and integrity.

Signature verification is configured in the optional `[oci]` section of the `extism_plugins.toml` file:

```toml
[oci]
# If 'true', skips signature verification. Defaults to 'false'.
# Not recommended for production environments.
insecure_skip_signature = false

# If 'true' (default), uses the official Sigstore TUF root to fetch trusted
# Fulcio root certificates and Rekor transparency log public keys.
use_sigstore_tuf_data = true

# --- Manual Trust Configuration (if 'use_sigstore_tuf_data' is false or for specific overrides) ---

# Path to a file containing Rekor public keys (PEM encoded).
# rekor_pub_keys = "/etc/querymt/security/rekor.pub"

# Path to a file containing Fulcio root certificates (PEM encoded).
# fulcio_certs = "/etc/querymt/security/fulcio.crt"

# --- Verification Constraints ---
# These act as policies for signature validation.

# Verify the signer's certificate was issued to this specific email address.
# cert_email = "signer@example.com"

# Verify the signer's certificate was issued by this specific OIDC issuer.
# Often used with cert_email or cert_url.
# Example: "https://github.com/login/oauth" for GitHub OIDC, or "https://accounts.google.com" for Google.
# cert_issuer = "https://oidc.issuer.example.com"

# Verify the signer's certificate SAN (Subject Alternative Name) matches this URL.
# cert_url = "https://github.com/my-org/my-repo/.github/workflows/release.yml@refs/tags/v1.0.0"
```

### How Verification Works:

1.  **Enabled by Default**: Signature verification is generally enabled (`insecure_skip_signature = false`).
2.  **Trust Root**:
    -   By default (`use_sigstore_tuf_data = true`), QueryMT attempts to fetch the latest trust materials (Fulcio CAs, Rekor keys) from the public Sigstore TUF repository.
    -   Alternatively, you can provide paths to local files for Rekor public keys and Fulcio certificates if you operate in an air-gapped environment or use a private Sigstore instance.
3.  **Constraints**: You can specify constraints like `cert_email`, `cert_issuer`, or `cert_url` to ensure the signature was created by an expected identity or build process.
4.  **Verification Process**:
    -   QueryMT (via its OCI client and Sigstore libraries) looks for a signature manifest associated with the plugin image (e.g., `ghcr.io/my-org/my-plugin:sha256-digest.sig`).
    -   It fetches the signature and the associated certificate/identity information.
    -   It verifies the signature against the image digest.
    -   It verifies the certificate chain against the trusted Fulcio roots.
    -   It checks if the signing identity satisfies the configured constraints.
    -   It may optionally verify transparency log entries in Rekor.

If verification fails (e.g., no valid signature found, constraints not met), QueryMT will refuse to load the plugin and log an error.

## Publishing OCI Plugins

To publish your plugin as an OCI image suitable for QueryMT:
1.  Ensure your Wasm module is named `plugin.wasm` (or be prepared for users to configure a custom extraction path if QueryMT supports it in the future).
2.  Create a simple `Dockerfile` or use a tool like `crane` or `oras` to package `plugin.wasm` into an OCI image layer.
    Example `Dockerfile` (very basic, might need adjustment for media types if your registry is strict):
    ```dockerfile
    FROM scratch
    COPY plugin.wasm /plugin.wasm
    ```
3.  Build and push the image to your chosen OCI registry.
4.  **Sign your image** using `cosign sign <your-image-ref>`. This is crucial for users who want to verify plugin authenticity.

Users can then refer to your plugin using its OCI reference (e.g., `oci://your-registry/your-plugin:tag`).

