# querymt-service

Small HTTP server that exposes OpenAI-style endpoints backed by QueryMT providers, could be used as proxy server to run QueryMT provider (`llama-cpp` or `mistral-rs`) on remote host or as standalone server for OpenAI-style API.

## Endpoints

- `POST /v1/chat/completions`
- `POST /v1/completions`
- `POST /v1/embeddings`
- `GET /v1/models`

## Build

```bash
cargo build -p querymt-service --release
```

Binary: `target/release/qmt-service`

## Providers config

Create a `providers.toml` with the providers you want to load.

Example (llama_cpp via OCI):

```toml
[[providers]]
name = "llama_cpp"
path = "oci://ghcr.io/querymt/llama-cpp:latest-cuda12.8"

# Or:
# path = "oci://ghcr.io/querymt/llama-cpp:latest-vulkan"
# Or:
#path = "/abs/path/to/libqmt_llama_cpp.so"
# Or:
# name = "mrs" # for `mistral-rs`
# path = "/abs/path/to/libqmt_mrs.so"
```

Notes:
- Service allowlist is currently only `llama_cpp` and `mrs` (provider names must match).
- Provider config is passed via request JSON (see examples below). Static `config = { ... }` is optional.

## Run

```bash
RUST_LOG=info ./target/release/qmt-service --addr 0.0.0.0:9999 --providers providers.toml
```

Optional auth:

```bash
./target/release/qmt-service --addr 0.0.0.0:9999 --providers providers.toml --auth-key YOUR_KEY
```

Auth key from file (recommended for production/NixOS secrets):

```bash
./target/release/qmt-service --addr 0.0.0.0:9999 --providers providers.toml --auth-key-file /run/secrets/qmt-service-auth-key
```

## Environment variables

All CLI args can be configured through environment variables:

- `QMT_SERVICE_ADDR` (default: `0.0.0.0:8080`)
- `QMT_SERVICE_PROVIDERS`
- `QMT_SERVICE_AUTH_KEY`
- `QMT_SERVICE_AUTH_KEY_FILE`

Examples:

```bash
QMT_SERVICE_ADDR=127.0.0.1:9999 \
QMT_SERVICE_PROVIDERS=./providers.toml \
QMT_SERVICE_AUTH_KEY_FILE=/run/secrets/qmt-service-auth-key \
./target/release/qmt-service
```

## Nix

Build package:

```bash
nix build .#qmt-service
```

Run app:

```bash
nix run .#qmt-service -- --providers ./providers.toml
```

## NixOS module

This repo exports a module at `nixosModules.querymt-service`.

```nix
{
  inputs.querymt.url = "github:querymt/querymt";

  outputs = { nixpkgs, querymt, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        querymt.nixosModules.querymt-service
        ({ ... }: {
          services.querymt-service.enable = true;
          services.querymt-service.listenAddress = "127.0.0.1";
          services.querymt-service.port = 9999;
          services.querymt-service.providersFile = "/etc/querymt/providers.toml";
          services.querymt-service.authKeyFile = "/run/secrets/qmt-service-auth-key";

          # Optional extra env (for provider API keys, etc.)
          services.querymt-service.environment = {
            RUST_LOG = "info";
          };
        })
      ];
    };
  };
}
```
