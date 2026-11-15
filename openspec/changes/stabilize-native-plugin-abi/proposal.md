# Proposal

## Why

Native provider plugins (llama-cpp, mrs, izwi) are shared libraries that hand Rust trait objects, `String`s, `Vec`s, futures and streams to the host. Rust gives none of these a stable layout across compiler versions, so a plugin built with a different rustc than the host can crash or corrupt memory. This has happened in practice.

Nothing detects the mismatch. The only check is a hand-bumped `NATIVE_PLUGIN_API = 2`. CI installs a floating `stable` toolchain for host and plugins separately, and the Nix host uses a toolchain fixed by `flake.lock`.

Publishing makes mismatches likely too. Every push to `main` overwrites the `latest*` tags and also the version tags (`<version>`, `<version>-<feature>`) of native and Wasm plugins, and the docs tell users to pull `:latest`. A released host therefore routinely pulls a plugin built later, from different source, by a different compiler.

The goal: a 0.6.x host loads any 0.6.x plugin published for its ABI, whichever compiler built either side. Breaking changes are allowed; they show up as an explicit ABI version bump and a clear load error, never as memory corruption.

## What Changes

- **BREAKING (native plugin interface): replace the trait-object boundary with a C-ABI boundary that passes only serialized bytes.**
  - The plugin exports two C functions: `querymt_native_plugin_abi()` and `querymt_native_plugin()`. The second returns a `#[repr(C)]` table of function pointers.
  - Requests and results cross as serialized bytes (MessagePack) through borrowed buffers. Results, stream chunks, errors and logs come back through C callbacks.
  - No Rust type layout, allocator, `std` copy or async runtime is shared between host and plugin.
  - The ABI version starts at 3. Host and plugin must match exactly; there is no compatibility window.
- **BREAKING: plugin authors use one export macro.** Plugins export through `querymt::export_native_plugin!(factory)` behind a new `native-plugin` feature, and keep implementing `LLMProviderFactory` and `LLMProvider` unchanged.
  - The macro's shim runs provider calls on a tokio runtime that the plugin owns.
  - It catches panics and turns them into plugin errors.
  - It forwards cancellation from the host to the running call.
  - It forwards logs to the host.
- **BREAKING (Rust API): remove the old export surface from `querymt::plugin`.** Removed:
  - the exported symbols `plugin_factory`, `plugin_http_factory`, `querymt_plugin_api`, `querymt_plugin_name` and `plugin_init_logging`;
  - the public types `FactoryCtor`, `HTTPFactoryCtor`, `LogCallbackFn`, `PluginInitLoggingFn`, the `plugin_log` module and `NATIVE_PLUGIN_API`;
  - `NATIVE_PLUGIN_ABI` replaces `NATIVE_PLUGIN_API`.
- **BREAKING: HTTP providers can no longer be loaded as native shared libraries.** The 14 HTTP providers (anthropic, openai, …) lose their `plugin_http_factory` export. They are still distributed as Wasm (Extism) plugins and can still be linked statically (`register_static_http`).
- **The host never unloads a native plugin library**, and initializes each library once per process. Providers, in-flight calls and streams can therefore never outlive the library's code.
- **New benchmark target for the native interface.** It runs without models, GPU or network and reports, against an in-process baseline:
  - call latency;
  - stream throughput (chunks/s and per-chunk overhead);
  - request and response bandwidth (MiB/s) for text and binary payloads.
- **BREAKING (artifact tags): plugin tag cleanup, for native and Wasm plugins.**
  - Only release (`v*`) builds write version tags (`<version>`, `<version>-<feature>`, `<version>-<os>-<arch>-<feature>`) and the `latest*` aliases. `latest` now means "newest release".
  - Builds from `main` publish only `edge*` aliases and immutable revision tags (`<version>-dev.<sha7>*`).
  - The `stable*` aliases are no longer published.
- **BREAKING (provider config): an `oci://` reference without a tag or digest resolves to the host's own release.** Example: `oci://ghcr.io/querymt/llama-cpp` with `features = ["metal"]` resolves to `:<host-version>-metal` instead of `:latest`. Explicit tags and digests are used unchanged.
- **Native artifacts carry their real ABI version** in the `mt.query.plugin.api` annotation (today it is hardcoded) and their source revision in a new `mt.query.plugin.revision` annotation. The host rejects an artifact whose ABI annotation differs from its own before downloading the library.
- **CI proves the compiler independence:** a fixture plugin built with the `beta` toolchain is loaded by a host test built with `stable`.

## Non-goals

- Running plugins in a separate process. A ggml `abort()` still ends the host process. The byte-based ABI keeps this possible later.
- Any compatibility window between ABI versions, or loading plugins built for ABI 2.
- Pinning an exact Rust toolchain. Once this lands, compiler versions no longer affect compatibility.
- Forwarding `tracing` spans from plugins to the host. Only `log` records are forwarded, as today.
- API-key resolvers (`set_key_resolver`) for native providers. None of the three native providers use them.
- Changing the Extism (Wasm) host/plugin contract or its contract versions.
- Deleting existing `stable*` or `latest*` tags from the registry. Publishing simply stops writing `stable*`.
- Simplifying izwi's or mrs's own runtime handling beyond what the new shim makes unnecessary.

## Capabilities

### New Capabilities
- `native-plugin-abi`: how the host loads native provider plugins and calls them through a compiler-independent, versioned C interface:
  - the handshake and its errors;
  - factory and provider operations, streaming and cancellation;
  - error and panic handling, logging, and library lifetime.
- `native-plugin-benchmarks`: a benchmark target that measures latency, throughput and bandwidth of the native plugin interface against an in-process baseline.
- `plugin-artifact-distribution`: which OCI tags and annotations release and `main` builds publish for provider plugins, how the host resolves untagged references, and how it rejects ABI-incompatible native artifacts before downloading them.

### Modified Capabilities
- `chat-output-roundtrip`: the requirement "Compatibility failures are explicit" now checks native plugin compatibility with the ABI version handshake instead of the "coordinated build/version contract". It also requires structured output to cross the native boundary in the same serialized form used by other transports.

## Impact

- **Packages and features:**
  - `querymt`: `native` feature (host loader, proxy, OCI resolution in `extism_host`) and a new `native-plugin` feature (export macro and plugin-side shim). Gains an optional `rmp-serde` dependency, gated behind `native` and `native-plugin` only, so `wasm32-wasip1` provider builds are unaffected.
  - `qmt-llama-cpp`, `qmt-mrs`, `qmt-izwi`: `native` feature (export through the macro).
  - `qmt-alibaba`, `qmt-anthropic`, `qmt-codex`, `qmt-deepseek`, `qmt-google`, `qmt-groq`, `qmt-kimi-code`, `qmt-mistral`, `qmt-moonshot`, `qmt-ollama`, `qmt-openai`, `qmt-openrouter`, `qmt-xai`, `qmt-zai`: `native` feature (export removed; the feature stays for static linking and tests).
  - New unpublished workspace crate `querymt-native-plugin-fixture` (test plugin, integration tests, benchmark).
  - Recompile only, no source changes: `querymt-cli`, `querymt-py` (both use `querymt/desktop`) and `querymt-agent`. `querymt-mobile-ffi` does not enable `native` and is unaffected.
- **Public Rust API:** the removals listed under What Changes; new `export_native_plugin!` and `NATIVE_PLUGIN_ABI`. **BREAKING.**
- **Provider wire shapes:**
  - The native plugin boundary changes from Rust trait objects to MessagePack payloads. **BREAKING:** plugins built for ABI 2 are rejected.
  - The Extism contract is unchanged.
  - The mesh/remote wire is unchanged, though it serializes the same types the same way.
- **Config:**
  - `[[providers]] path` gains the untagged-reference rule. **BREAKING:** untagged references used to mean `:latest`.
  - `[[providers]] plugin_api` and `[profile.providers.<name>] plugin_api` keep their meaning: the required native ABI, which must equal the host's. The only accepted value becomes 3.
- **Artifact tags:** `latest*` now tracks releases, `edge*` and `<version>-dev.<sha7>*` are new, and `stable*` is retired. Applies to native and Wasm. **BREAKING** for anyone who used `:latest` to follow `main`. The provider-index (`repo.query.mt`) entries for `main` builds move from `:latest` to `:edge`.
- **CI:** `native-publish`, `native-create-manifests`, `publish-llama-cpp`, `publish-mrs`, `publish-izwi` and `publish-providers` change their tag logic and annotations; a cross-toolchain check is added to `test-querymt`; new release-script tests.
- **Docs:** `docs/docs/plugins/{interface_spec,development,configuration,oci_plugins}.md`, `docs/docs/core/{available-providers,providers}.md`, `providers.example.toml`, and an "Unreleased" entry in `CHANGELOG.md`.
- **Not affected:** session DB schema, ACP, hooks/middleware, typeshare types and Python binding APIs. No querymt-desktop UI follow-up is needed.
