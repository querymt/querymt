# querymt-service

Small HTTP server that exposes OpenAI-style endpoints backed by QueryMT providers, could be used as proxy server to run QueryMT provider (`llama-cpp` or `mistral-rs`) on remote host or as standalone server for OpenAI-style API.

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

## Endpoints

- `POST /v1/chat/completions`
- `POST /v1/completions`
